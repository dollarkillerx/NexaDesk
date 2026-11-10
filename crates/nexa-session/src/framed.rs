//! Reliable streams (§3.1): a type byte, then `varint length + protobuf` messages.

use bytes::{Bytes, BytesMut};
use nexa_wire::framing::{DEFAULT_MAX_FRAME, FrameDecoder, StreamType, encode_frame};
use prost::Message;
use quinn::{Connection, RecvStream, SendStream};

use crate::SessionError;

/// Handshake and steady-state messages are small; refuse anything absurd early.
pub const MAX_CONTROL_FRAME: usize = 64 * 1024;
/// A key event or a batch of typed text.
pub const MAX_INPUT_FRAME: usize = 16 * 1024;
pub const MAX_CLIPBOARD_FRAME: usize = DEFAULT_MAX_FRAME;
/// A file chunk ([`crate::transfer::CHUNK`]) and its envelope.
pub const MAX_FILE_FRAME: usize = 128 * 1024;

const READ_CHUNK: usize = 8192;

/// §3.1 priorities.
pub fn priority_of(kind: StreamType) -> i32 {
    match kind {
        StreamType::Control | StreamType::Input => 10,
        StreamType::Clipboard => 0,
        StreamType::File => -10,
    }
}

fn max_frame_of(kind: StreamType) -> usize {
    match kind {
        StreamType::Control => MAX_CONTROL_FRAME,
        StreamType::Input => MAX_INPUT_FRAME,
        StreamType::Clipboard => MAX_CLIPBOARD_FRAME,
        StreamType::File => MAX_FILE_FRAME,
    }
}

pub struct FramedSend {
    inner: SendStream,
    frame: BytesMut,
}

impl FramedSend {
    fn new(inner: SendStream) -> Self {
        Self {
            inner,
            frame: BytesMut::new(),
        }
    }

    pub async fn send_body(&mut self, body: &[u8]) -> Result<(), SessionError> {
        self.frame.clear();
        encode_frame(body, &mut self.frame);
        self.inner
            .write_all(&self.frame)
            .await
            .map_err(|e| SessionError::Stream(e.to_string()))
    }

    pub async fn send<M: Message>(&mut self, message: &M) -> Result<(), SessionError> {
        self.send_body(&message.encode_to_vec()).await
    }

    /// Abandons the stream: the peer's reads fail with `code`.
    pub fn reset(&mut self, code: u32) {
        let _ = self.inner.reset(code.into());
    }

    /// Closes the send side without waiting.
    pub fn finish(&mut self) {
        let _ = self.inner.finish();
    }

    /// Closes the send side, then waits until the peer has the data or has given up.
    /// A `Connection::close` right after a send would otherwise throw the data away.
    pub async fn finish_and_flush(mut self, limit: std::time::Duration) {
        if self.inner.finish().is_ok() {
            let _ = tokio::time::timeout(limit, self.inner.stopped()).await;
        }
    }
}

pub struct FramedRecv {
    inner: RecvStream,
    decoder: FrameDecoder,
    chunk: Box<[u8; READ_CHUNK]>,
}

impl FramedRecv {
    fn new(inner: RecvStream, kind: StreamType) -> Self {
        Self {
            inner,
            decoder: FrameDecoder::new(max_frame_of(kind)),
            chunk: Box::new([0; READ_CHUNK]),
        }
    }

    /// `Ok(None)` is a clean end of stream. Safe to race in `select!`: no data is
    /// consumed across an await point without being kept.
    pub async fn next_body(&mut self) -> Result<Option<Bytes>, SessionError> {
        loop {
            if let Some(frame) = self
                .decoder
                .next_frame()
                .map_err(|_| SessionError::Protocol("bad frame"))?
            {
                return Ok(Some(frame));
            }
            match self.inner.read(&mut self.chunk[..]).await {
                Ok(Some(n)) => self.decoder.push(&self.chunk[..n]),
                Ok(None) => return Ok(None),
                Err(e) => return Err(SessionError::Stream(e.to_string())),
            }
        }
    }

    pub async fn next<M: Message + Default>(&mut self) -> Result<Option<M>, SessionError> {
        match self.next_body().await? {
            Some(body) => M::decode(body)
                .map(Some)
                .map_err(|_| SessionError::Protocol("bad message")),
            None => Ok(None),
        }
    }

    /// Tells the peer no more of this stream will be read.
    pub fn stop(mut self) {
        let _ = self.inner.stop(0u32.into());
    }

    /// [`Self::stop`] with an application code, keeping the handle.
    pub fn stop_with(&mut self, code: u32) {
        let _ = self.inner.stop(code.into());
    }
}

async fn start_send(send: &mut SendStream, kind: StreamType) -> Result<(), SessionError> {
    let _ = send.set_priority(priority_of(kind));
    send.write_all(&[kind as u8])
        .await
        .map_err(|e| SessionError::Stream(e.to_string()))
}

pub async fn open_bi(
    conn: &Connection,
    kind: StreamType,
) -> Result<(FramedSend, FramedRecv), SessionError> {
    let (mut send, recv) = conn.open_bi().await?;
    start_send(&mut send, kind).await?;
    Ok((FramedSend::new(send), FramedRecv::new(recv, kind)))
}

pub async fn open_uni(conn: &Connection, kind: StreamType) -> Result<FramedSend, SessionError> {
    let mut send = conn.open_uni().await?;
    start_send(&mut send, kind).await?;
    Ok(FramedSend::new(send))
}

async fn read_kind(recv: &mut RecvStream) -> Result<StreamType, SessionError> {
    let mut byte = [0u8; 1];
    recv.read_exact(&mut byte)
        .await
        .map_err(|e| SessionError::Stream(e.to_string()))?;
    StreamType::from_byte(byte[0]).map_err(|_| SessionError::Protocol("unknown stream type"))
}

/// Accepts one bidirectional stream and reads its type byte.
pub async fn accept_bi(
    conn: &Connection,
) -> Result<(StreamType, FramedSend, FramedRecv), SessionError> {
    let (send, mut recv) = conn.accept_bi().await?;
    let kind = read_kind(&mut recv).await?;
    let send = FramedSend::new(send);
    let _ = send.inner.set_priority(priority_of(kind));
    Ok((kind, send, FramedRecv::new(recv, kind)))
}

/// Accepts one unidirectional stream and reads its type byte.
pub async fn accept_uni(conn: &Connection) -> Result<(StreamType, FramedRecv), SessionError> {
    let mut recv = conn.accept_uni().await?;
    let kind = read_kind(&mut recv).await?;
    Ok((kind, FramedRecv::new(recv, kind)))
}
