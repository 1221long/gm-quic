use std::{
    borrow::Cow,
    fmt::Debug,
    io,
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use closing::ClosingConnection;
use draining::DrainingConnection;
use futures::{channel::mpsc, StreamExt};
use qbase::{
    cid::{self, ConnectionId},
    config::Parameters,
    error::{Error, ErrorKind},
    packet::{DataPacket, RetryHeader},
    streamid::Role,
    token::ArcTokenRegistry,
};
use qcongestion::CongestionControl;
use qrecovery::{
    recv::Reader, reliable::ArcReliableFrameDeque, send::Writer, space::Epoch, streams,
};
use qudp::ArcUsc;
use qunreliable::{DatagramReader, DatagramWriter};
use raw::RawConnection;
use tokio::task::JoinHandle;

use crate::{
    connection::ConnState::{Closed, Closing, Draining, Raw},
    path::pathway::Pathway,
    router::{Router, RouterRegistry},
    tls::ArcTlsSession,
};

pub mod closing;
pub mod draining;
pub mod parameters;
pub mod raw;
pub mod scope;
pub mod transmit;

pub type PacketEntry = mpsc::UnboundedSender<(DataPacket, Pathway, ArcUsc)>;
pub type RcvdPackets = mpsc::UnboundedReceiver<(DataPacket, Pathway, ArcUsc)>;

pub type ArcLocalCids = cid::ArcLocalCids<RouterRegistry<ArcReliableFrameDeque>>;
pub type ArcRemoteCids = cid::ArcRemoteCids<ArcReliableFrameDeque>;
pub type CidRegistry = cid::Registry<ArcLocalCids, ArcRemoteCids>;

pub type DataStreams = streams::DataStreams<ArcReliableFrameDeque>;
pub type Handshake = qbase::handshake::Handshake<ArcReliableFrameDeque>;

enum ConnState {
    Raw(RawConnection),
    Closing(ClosingConnection),
    Draining(DrainingConnection),
    Closed,
}

impl ConnState {
    fn should_enter_close(
        &mut self,
        error: Error,
    ) -> Option<([JoinHandle<RcvdPackets>; 4], Duration)> {
        let conn = core::mem::replace(self, Closed);
        let Raw(raw_conn) = conn else {
            // has been closing/draining
            *self = conn;
            return None;
        };

        raw_conn.datagrams.on_conn_error(&error);
        raw_conn.flow_ctrl.on_conn_error(&error);
        raw_conn.streams.on_conn_error(&error);
        raw_conn.params.on_conn_error(&error);
        raw_conn.tls_session.abort();
        raw_conn.notify.notify_waiters();

        let hs = raw_conn.hs.try_into().ok();
        let one_rtt = raw_conn.data.try_into().ok();

        let recv_packets = raw_conn.join_handles;
        let pto_time = raw_conn
            .pathes
            .iter()
            .map(|path| path.cc.pto_time(Epoch::Data))
            .max()
            .unwrap();

        *self = match (hs, one_rtt) {
            (None, None) => {
                let local_cids = raw_conn.cid_registry.local.active_cids();
                let draining_connection = DrainingConnection::new(local_cids, error);
                Draining(draining_connection)
            }
            (hs, one_rtt) => {
                let local_cids = raw_conn.cid_registry.local.active_cids();
                let closing_connection = ClosingConnection::new(error, local_cids, hs, one_rtt);
                Closing(closing_connection)
            }
        };

        Some((recv_packets, pto_time))
    }

    fn enter_draining(&mut self, error: Error) -> Option<Duration> {
        let conn = core::mem::replace(self, Closed);
        let Raw(raw_conn) = conn else {
            // has been closing/draining
            *self = conn;
            return None;
        };

        raw_conn.datagrams.on_conn_error(&error);
        raw_conn.flow_ctrl.on_conn_error(&error);
        raw_conn.streams.on_conn_error(&error);
        raw_conn.params.on_conn_error(&error);
        raw_conn.tls_session.abort();
        raw_conn.notify.notify_waiters();

        let local_cids = raw_conn.cid_registry.local.active_cids();
        *self = Draining(DrainingConnection::new(local_cids, error));

        let pto_time = raw_conn
            .pathes
            .iter()
            .map(|path| path.cc.pto_time(Epoch::Data))
            .max()
            .unwrap();
        Some(pto_time)
    }

    fn no_vaiable_path(&mut self) {
        let conn = core::mem::replace(self, Closed);
        // no need to reset the state to conn
        let Raw(raw_conn) = conn else { return };
        let error = Error::with_default_fty(ErrorKind::NoViablePath, "No viable path");

        raw_conn.datagrams.on_conn_error(&error);
        raw_conn.flow_ctrl.on_conn_error(&error);
        raw_conn.streams.on_conn_error(&error);
        raw_conn.params.on_conn_error(&error);
        raw_conn.tls_session.abort();
        raw_conn.notify.notify_waiters();

        let local_cids = &raw_conn.cid_registry.local;
        local_cids.active_cids().iter().for_each(Router::remove);
    }

    fn die(&mut self) {
        let conn = core::mem::replace(self, Closed);
        let local_cids = match conn {
            Closing(conn) => conn.local_cids,
            Draining(conn) => conn.local_cids,
            Raw(..) | Closed => unreachable!(),
        };

        for cid in local_cids {
            Router::remove(&cid);
        }
    }
}
#[derive(Clone)]
pub struct ArcConnection(Arc<Mutex<ConnState>>);

impl Debug for ArcConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "QUIC Connection")
    }
}

impl ArcConnection {
    pub fn new_client(
        scid: ConnectionId,
        server_name: String,
        mut parameters: Parameters,
        tls_config: Arc<rustls::ClientConfig>,
        token_registry: ArcTokenRegistry,
    ) -> Self {
        let Ok(server_name) = server_name.try_into() else {
            panic!("server_name is not valid")
        };

        parameters.set_initial_source_connection_id(Some(scid));

        let dcid = ConnectionId::random_gen(8);
        let tls_session = ArcTlsSession::new_client(server_name, tls_config.clone(), &parameters);
        let initial_keys =
            ArcTlsSession::initial_keys(tls_config.crypto_provider(), rustls::Side::Client, dcid);
        let raw_conn = RawConnection::new(
            Role::Client,
            parameters,
            tls_session,
            scid,
            dcid,
            initial_keys,
            token_registry,
        );
        raw_conn.into()
    }

    pub fn add_initial_path(&self, pathway: Pathway, usc: ArcUsc) {
        let guard = self.0.lock().unwrap();
        if let Raw(ref conn) = *guard {
            _ = conn.pathes.get_or_create(pathway, usc);
        }
    }

    pub fn new_server(
        initial_scid: ConnectionId,
        initial_dcid: ConnectionId,
        mut parameters: Parameters,
        initial_keys: rustls::quic::Keys,
        tls_config: Arc<rustls::ServerConfig>,
        token_registry: ArcTokenRegistry,
    ) -> Self {
        parameters.set_original_destination_connection_id(Some(initial_dcid));

        let tls_session = ArcTlsSession::new_server(tls_config.clone(), &parameters);
        let raw_conn = RawConnection::new(
            Role::Server,
            parameters,
            tls_session,
            initial_scid,
            initial_dcid,
            initial_keys,
            token_registry,
        );
        raw_conn.into()
    }

    // /// Get the streams of the connection, return error if the connection is in closing state or
    // /// draining state. Even if the connection will enter closing state in future, the returned
    // /// data streams are still available. It doesn't matter, because the returned DataStreams will
    // /// be synced into Error state, and do anything about this DataStreams will return an Error.
    // pub fn streams(&self) -> io::Result<DataStreams> {
    //     // TODO: ArcConnection不再暴露赤裸的streams接口，而是根据双方Parameters使用
    //     //      raw_conn.streams().open_bi(...)去异步地创建
    //     let guard = self.0.lock().unwrap();
    //     if let ConnState::Raw(ref raw_conn) = *guard {
    //         Ok(raw_conn.streams.clone())
    //     } else {
    //         Err(io::Error::new(
    //             io::ErrorKind::BrokenPipe,
    //             "Connection is closing or closed",
    //         ))
    //     }
    // }

    pub async fn open_bi_stream(&self) -> io::Result<Option<(Reader, Writer)>> {
        let (remote_params, data_streams, conn_error) = {
            let guard = self.0.lock().unwrap();
            let raw_conn = match guard.deref() {
                Raw(raw) => raw,
                Closing(closing) => return Err(closing.error.clone())?,
                Draining(draining) => return Err(draining.error.clone())?,
                Closed => unreachable!(),
            };

            (
                raw_conn.params.remote.clone(),
                raw_conn.streams.clone(),
                raw_conn.error.clone(),
            )
        };

        let remote_params = remote_params.read().await?;

        let result = data_streams
            .open_bi(remote_params.initial_max_stream_data_bidi_remote().into())
            .await
            .inspect_err(|e| conn_error.on_error(e.clone()));
        Ok(result?)
    }

    pub async fn open_uni_stream(&self) -> io::Result<Option<Writer>> {
        let (remote_params, data_streams, conn_error) = {
            let guard = self.0.lock().unwrap();
            let raw_conn = match guard.deref() {
                Raw(raw) => raw,
                Closing(closing) => return Err(closing.error.clone())?,
                Draining(draining) => return Err(draining.error.clone())?,
                Closed => unreachable!(),
            };

            (
                raw_conn.params.remote.clone(),
                raw_conn.streams.clone(),
                raw_conn.error.clone(),
            )
        };

        let remote_params = remote_params.read().await?;

        let result = data_streams
            .open_uni(remote_params.initial_max_stream_data_uni().into())
            .await
            .inspect_err(|e| conn_error.on_error(e.clone()));
        Ok(result?)
    }

    pub async fn accept_bi_stream(&self) -> io::Result<(Reader, Writer)> {
        let (remote_params, data_streams, conn_error) = {
            let guard = self.0.lock().unwrap();
            let raw_conn = match guard.deref() {
                Raw(raw) => raw,
                Closing(closing) => return Err(closing.error.clone())?,
                Draining(draining) => return Err(draining.error.clone())?,
                Closed => unreachable!(),
            };

            (
                raw_conn.params.remote.clone(),
                raw_conn.streams.clone(),
                raw_conn.error.clone(),
            )
        };

        let remote_params = remote_params.read().await?;

        let result = data_streams
            .accept_bi(remote_params.initial_max_stream_data_bidi_local().into())
            .await
            .inspect_err(|e| conn_error.on_error(e.clone()))?;
        Ok(result)
    }

    pub async fn accept_uni_stream(&self) -> io::Result<Reader> {
        let (data_streams, conn_error) = {
            let guard = self.0.lock().unwrap();
            let raw_conn = match guard.deref() {
                Raw(raw) => raw,
                Closing(closing) => return Err(closing.error.clone())?,
                Draining(draining) => return Err(draining.error.clone())?,
                Closed => unreachable!(),
            };

            (raw_conn.streams.clone(), raw_conn.error.clone())
        };

        let result = data_streams
            .accept_uni()
            .await
            .inspect_err(|e| conn_error.on_error(e.clone()))?;
        Ok(result)
    }

    pub fn datagram_reader(&self) -> io::Result<DatagramReader> {
        let guard = self.0.lock().unwrap();

        match guard.deref() {
            Raw(raw) => raw.datagrams.reader(),
            Closing(closing) => Err(closing.error.clone())?,
            Draining(draining) => Err(draining.error.clone())?,
            Closed => unreachable!(),
        }
    }

    pub async fn datagram_writer(&self) -> io::Result<DatagramWriter> {
        let (remote_params, datagram_flow) = {
            let guard = self.0.lock().unwrap();
            let raw_conn = match guard.deref() {
                Raw(raw) => raw,
                Closing(closing) => return Err(closing.error.clone())?,
                Draining(draining) => return Err(draining.error.clone())?,
                Closed => unreachable!(),
            };

            (raw_conn.params.remote.clone(), raw_conn.datagrams.clone())
        };

        let remote_params = remote_params.read().await?;
        datagram_flow.writer(remote_params.max_datagram_frame_size().into())
    }

    /// Gracefully closes the connection.
    ///
    /// Closes the connection with a specified error.
    /// This function is intended for use by the application layer to signal an
    /// error and initiate the connection closure.
    pub fn close(&self, msg: impl Into<Cow<'static, str>>) {
        let mut state = self.0.lock().unwrap();
        if let Raw(conn) = state.deref_mut() {
            let error = Error::with_default_fty(ErrorKind::Application, msg);
            log::info!("Connection is closed by application: {}", error);
            conn.error.set_app_error(error.clone());
            drop(state);
            self.should_enter_closing(error);
        }
    }

    /// This function transitioning connection to a `Closing` state and
    /// initiating a background task to manage the closing handshake. This task awaits
    /// confirmation from the peer (Connection Close Frame) within a timeout derived
    /// from the connection's Path Termination Timeout (PTO).  Upon successful
    /// confirmation, any remaining data is drained.  If the timeout expires without
    /// confirmation, the connection is forcefully terminated.
    fn should_enter_closing(&self, error: Error) {
        let mut state = self.0.lock().unwrap();
        let state = state.deref_mut();
        if !matches!(state, Raw(..)) {
            return;
        }
        let Some((handles, pto)) = state.should_enter_close(error) else {
            return;
        };

        match state {
            Closing(closing) => {
                for handle in handles {
                    tokio::spawn({
                        let mut closing_conn = closing.clone();
                        async move {
                            let mut rcvd_packets = handle.await.unwrap();
                            while let Some((packet, pathway, usc)) = rcvd_packets.next().await {
                                closing_conn.recv_packet_via_pathway(packet, pathway, usc);
                            }
                        }
                    });
                }
                tokio::spawn({
                    let conn = self.clone();
                    let rcvd_ccf = closing.get_rcvd_ccf();
                    async move {
                        let start = Instant::now();
                        let time = pto * 3;
                        match tokio::time::timeout(time, rcvd_ccf.did_recv()).await {
                            Ok(_) => conn.draining(pto * 3 - start.elapsed()),
                            Err(_) => conn.die(),
                        }
                    }
                });
            }
            Draining(..) => {
                drop(handles); // break the channels
                self.draining(pto * 3)
            }
            _ => unreachable!(),
        }
    }

    pub fn enter_draining(&self, error: Error) {
        let mut state = self.0.lock().unwrap();
        let state = state.deref_mut();
        let Some(pto) = state.enter_draining(error) else {
            // has been closed
            return;
        };

        self.draining(pto * 3);
    }

    /// Enter draining state from raw state or closing state.
    /// Can only be called internally, and the app should not care this method.
    pub(crate) fn draining(&self, remaining: Duration) {
        assert!(matches!(self.0.lock().unwrap().deref_mut(), Draining(..)));

        tokio::spawn({
            let conn = self.clone();
            async move {
                tokio::time::sleep(remaining).await;
                conn.die();
            }
        });
    }

    pub(crate) fn no_vaiable_path(self) {
        self.0.lock().unwrap().no_vaiable_path();
    }

    /// Dismiss the connection, remove it from the global router.
    /// Can only be called internally, and the app should not care this method.
    pub fn die(self) {
        self.0.lock().unwrap().die();
    }

    pub fn update_path_recv_time(&self, pathway: Pathway) {
        let guard = self.0.lock().unwrap();
        if let ConnState::Raw(ref raw_conn) = *guard {
            raw_conn.update_path_recv_time(pathway);
        }
    }

    pub fn recv_retry_packet(&self, retry: &RetryHeader) {
        let guard = self.0.lock().unwrap();
        if let Raw(ref conn) = *guard {
            *conn.token.lock().unwrap() = retry.token.to_vec();
            conn.cid_registry.remote.revise_initial_dcid(retry.scid);
            let sent_record = conn.initial.space.sent_packets();
            let mut guard = sent_record.recv();
            for i in 0..guard.largest_pn() {
                for frame in guard.may_loss_pkt(i) {
                    conn.initial.crypto_stream.outgoing().may_loss_data(&frame);
                }
            }
        }
    }

    pub fn is_active(&self) -> bool {
        let guard = self.0.lock().unwrap();
        !matches!(&*guard, ConnState::Raw(..))
    }
}

impl From<RawConnection> for ArcConnection {
    fn from(raw_conn: RawConnection) -> Self {
        let conn_error = raw_conn.error.clone();
        let conn = ArcConnection(Arc::new(Mutex::new(ConnState::Raw(raw_conn))));

        tokio::spawn({
            let conn = conn.clone();
            async move {
                let (err, kind) = conn_error.did_error_occur().await;
                if kind != crate::error::ConnErrorKind::Application {
                    log::error!("Connection is closed unexpectedly: {}", err)
                };
                match kind {
                    crate::error::ConnErrorKind::Application => {} // resolved by ArcConnection::close
                    crate::error::ConnErrorKind::Transport => conn.should_enter_closing(err),
                    crate::error::ConnErrorKind::CcfReceived => conn.enter_draining(err),
                    crate::error::ConnErrorKind::NoViablePath => conn.no_vaiable_path(),
                }
            }
        });

        conn
    }
}
#[cfg(test)]
mod tests {}
