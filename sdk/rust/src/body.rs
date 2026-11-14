//! Streamed request bodies.

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt};
use tokio::sync::mpsc;
use unsworn_proto::{Ack, Frame, Message};

use crate::Result;
use crate::worker::Out;

/// A request body that arrives in chunks, on routes with
/// `stream_body = true`. Get it with [`Request::body_stream`](crate::Request::body_stream).
///
/// Unsworn sends more of the body as the handler reads it, so a slow
/// handler slows the upload instead of buffering it.
pub struct BodyStream {
    id: String,
    rx: mpsc::UnboundedReceiver<Result<Bytes>>,
    out: Out,
}

impl BodyStream {
    pub(crate) fn new(id: String, rx: mpsc::UnboundedReceiver<Result<Bytes>>, out: Out) -> Self {
        BodyStream { id, rx, out }
    }

    /// Reads the rest of the body into memory.
    pub async fn bytes(mut self) -> Result<Bytes> {
        let mut all = BytesMut::new();
        while let Some(chunk) = self.next().await {
            all.extend_from_slice(&chunk?);
        }
        Ok(all.freeze())
    }
}

impl Stream for BodyStream {
    type Item = Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let polled = self.rx.poll_recv(cx);
        if let Poll::Ready(Some(Ok(chunk))) = &polled {
            self.out.send(Frame::new(Message::Ack(Ack {
                id: self.id.clone(),
                bytes: chunk.len() as u64,
            })));
        }
        polled
    }
}

impl fmt::Debug for BodyStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BodyStream").field("id", &self.id).finish()
    }
}
