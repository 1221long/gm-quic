use std::sync::{Arc, Mutex, RwLock};

use qbase::{error::Error, frame::DatagramFrame, util::TransportLimit};

use super::{
    reader::{DatagramReader, RawDatagramReader},
    writer::{DatagramWriter, RawDatagramWriter},
};

/// The unique [`RawDatagramFlow`] struct represents a flow for sending and receiving datagrams frame from a connection.
#[derive(Debug, Clone)]
pub struct RawDatagramFlow {
    reader: DatagramReader,
    writer: DatagramWriter,
}

impl RawDatagramFlow {
    /// Creates a new instance of [`DatagramFlow`].
    ///
    /// # Arguments
    ///
    /// * `local_max_datagram_frame_size` - The maximum size of the datagram frame that can be received.
    ///
    /// * `remote_max_datagram_frame_size` - The maximum size of the datagram frame that can be sent.
    ///
    /// # Notes
    ///
    /// The arguments chould be the default value, or the value negotiation by last connection.
    ///
    /// If the new `remote_max_datagram_frame_size` is smaller than the previous value, a connection error will occur,
    /// see [`DatagramWriter::update_remote_max_datagram_frame_size`] for more details.
    fn new(local_max_datagram_frame_size: u64, remote_max_datagram_frame_size: u64) -> Self {
        let reader = RawDatagramReader::new(remote_max_datagram_frame_size as _);
        let writer = RawDatagramWriter::new(local_max_datagram_frame_size as _);

        Self {
            reader: DatagramReader(Arc::new(Mutex::new(Ok(reader)))),
            writer: DatagramWriter(Arc::new(Mutex::new(Ok(writer)))),
        }
    }
}

/// The shared [`RawDatagramFlow`] struct represents a flow for sending and receiving datagrams frame from a connection.
#[derive(Debug, Clone)]
pub struct DatagramFlow {
    raw_flow: Arc<RwLock<RawDatagramFlow>>,
}

impl DatagramFlow {
    /// see [`RawDatagramFlow::new`] for more details.
    #[inline]
    pub fn new(local_max_datagram_frame_size: u64, remote_max_datagram_frame_size: u64) -> Self {
        let flow = RawDatagramFlow::new(
            local_max_datagram_frame_size,
            remote_max_datagram_frame_size,
        );
        Self {
            raw_flow: Arc::new(RwLock::new(flow)),
        }
    }

    /// See [`DatagramWriter::update_remote_max_datagram_frame_size`] for more details.
    #[inline]
    pub fn update_remote_max_datagram_frame_size(&self, new_size: usize) -> Result<(), Error> {
        let flow = self.raw_flow.read().unwrap();
        flow.writer.update_remote_max_datagram_frame_size(new_size)
    }

    /// See [`DatagramWriter::try_read_datagram`] for more details.
    #[inline]
    pub fn try_read_datagram(
        &self,
        limit: &mut TransportLimit,
        buf: &mut [u8],
    ) -> Option<(DatagramFrame, usize)> {
        self.raw_flow
            .read()
            .unwrap()
            .writer
            .try_read_datagram(limit, buf)
    }

    /// See [`DatagramReader::recv_datagram`] for more details.
    #[inline]
    pub fn recv_datagram(&self, frame: DatagramFrame, body: bytes::Bytes) -> Result<(), Error> {
        self.raw_flow
            .read()
            .unwrap()
            .reader
            .recv_datagram(frame, body)
    }

    /// Create a pair of [`DatagramReader`] and [`DatagramWriter`] for the application to read and write datagrams.
    #[inline]
    pub fn rw(&self) -> (DatagramReader, DatagramWriter) {
        let flow = self.raw_flow.read().unwrap();
        (
            DatagramReader(flow.reader.0.clone()),
            DatagramWriter(flow.writer.0.clone()),
        )
    }

    /// Handles a connection error.
    ///
    /// # Arguments
    ///
    /// * `error` - The error that occurred.
    ///
    /// # Note
    ///
    /// This method will wake up all the wakers that are waiting for the data to be read.
    ///
    /// if the connection is already closed, the new error will be ignored.
    ///
    /// See [`DatagramReader::on_conn_error`] and [`DatagramWriter::on_conn_error`] for more details.
    #[inline]
    pub fn on_conn_error(&self, error: &Error) {
        let raw_flow = self.raw_flow.read().unwrap();
        raw_flow.reader.on_conn_error(error);
        raw_flow.writer.on_conn_error(error);
    }
}
