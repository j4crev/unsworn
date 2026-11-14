use std::collections::{BTreeMap, HashMap};
use std::io;
use std::os::fd::FromRawFd;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};

use bytes::{Bytes, BytesMut};
use futures_util::{FutureExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use unsworn_proto::{
    Chunk, Continue, DecodeError, End, ErrorMessage, Frame, HeaderValues, Message, MissingHandler,
    PROTOCOL_VERSION, Ready, Response, Verified, framed, jsonl,
};

use crate::handlers::Handlers;
use crate::{Body, Cancel, Reply, Request};

/// Runs the worker until Unsworn closes the connection, then exits the
/// process. Call it from `main`; it starts its own Tokio runtime.
pub fn run(handlers: Handlers) -> ! {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("unsworn: can't start the Tokio runtime");
    let code = match runtime.block_on(serve(handlers)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("unsworn: {e}");
            1
        }
    };
    std::process::exit(code)
}

/// Runs the worker until Unsworn closes the connection, inside an existing
/// Tokio runtime. It reads `UNSWORN_TRANSPORT` to pick Tier 1 (stdio) or
/// Tier 2 (a Unix socket).
///
/// On Tier 1, stdout carries protocol messages, so `serve` points file
/// descriptor 1 at stderr: `println!` output ends up in Unsworn's logs.
pub async fn serve(handlers: Handlers) -> io::Result<()> {
    let transport = std::env::var("UNSWORN_TRANSPORT").unwrap_or_default();
    if let Some(path) = transport.strip_prefix("unix:") {
        let stream = tokio::net::UnixStream::connect(path).await?;
        let (read, write) = stream.into_split();
        return run_loop(handlers, read, write, true).await;
    }
    // SAFETY: plain descriptor juggling at startup. The duplicate of fd 1
    // becomes the protocol stream, owned by the File below, and fd 1 then
    // points at stderr.
    let protocol = unsafe {
        let fd = libc::dup(1);
        if fd < 0 || libc::dup2(2, 1) < 0 {
            return Err(io::Error::last_os_error());
        }
        std::fs::File::from_raw_fd(fd)
    };
    run_loop(
        handlers,
        tokio::io::stdin(),
        tokio::fs::File::from_std(protocol),
        false,
    )
    .await
}

#[derive(Clone)]
struct Out {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    framed: bool,
}

impl Out {
    fn send(&self, frame: Frame) {
        let mut buf = Vec::new();
        let encoded = if self.framed {
            framed::encode(&frame, &mut buf)
        } else {
            jsonl::encode(&frame, &mut buf)
        };
        match encoded {
            Ok(()) => {
                let _ = self.tx.send(buf);
            }
            Err(e) => eprintln!("unsworn: can't encode a message: {e}"),
        }
    }

    fn error(&self, id: &str, message: String) {
        self.send(Frame::new(Message::Error(ErrorMessage {
            id: Some(id.to_string()),
            message,
        })));
    }
}

type InFlight = Arc<Mutex<HashMap<String, (AbortHandle, Cancel)>>>;

async fn run_loop<R, W>(
    handlers: Handlers,
    mut read: R,
    mut write: W,
    framed: bool,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(async move {
        while let Some(buf) = rx.recv().await {
            write.write_all(&buf).await?;
            write.flush().await?;
        }
        io::Result::Ok(())
    });
    let out = Out { tx, framed };
    let max_in_flight = std::env::var("UNSWORN_MAX_IN_FLIGHT")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(64);
    out.send(Frame::new(Message::Ready(Ready {
        protocol: PROTOCOL_VERSION,
        max_in_flight: Some(max_in_flight),
        verify: true,
        sdk: Some(format!("unsworn-rust/{}", crate::VERSION)),
    })));

    let handlers = Arc::new(handlers);
    let in_flight: InFlight = Arc::default();
    let mut jsonl_decoder = jsonl::Decoder::default();
    let mut framed_decoder = framed::Decoder::default();
    let mut buf = BytesMut::with_capacity(64 * 1024);
    loop {
        let decoded = if framed {
            framed_decoder.decode(&mut buf)
        } else {
            jsonl_decoder.decode(&mut buf)
        };
        let frame = match decoded {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                if read.read_buf(&mut buf).await? == 0 {
                    break;
                }
                continue;
            }
            Err(e @ DecodeError::Invalid { .. }) => {
                eprintln!("unsworn: {e}");
                continue;
            }
            Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e)),
        };
        match frame.message {
            Message::Request(req) => start(&handlers, &in_flight, &out, req, frame.body),
            Message::Cancel(cancel) => {
                if let Some((task, token)) = in_flight.lock().unwrap().remove(&cancel.id) {
                    token.cancel();
                    task.abort();
                }
            }
            Message::Verify(verify) => {
                let missing = verify
                    .handlers
                    .iter()
                    .filter_map(|reference| {
                        handlers
                            .resolve(reference)
                            .err()
                            .map(|reason| MissingHandler {
                                handler: reference.clone(),
                                reason,
                            })
                    })
                    .collect();
                out.send(Frame::new(Message::Verified(Verified {
                    id: verify.id,
                    missing,
                })));
            }
            _ => {}
        }
    }
    drop(out);
    let _ = writer.await;
    Ok(())
}

fn start(
    handlers: &Arc<Handlers>,
    in_flight: &InFlight,
    out: &Out,
    msg: unsworn_proto::Request,
    body: Option<Bytes>,
) {
    let cancel = Cancel::default();
    let id = msg.id.clone();
    let req = Request {
        id: msg.id,
        phase: msg.phase,
        handler: msg.handler,
        route: msg.route,
        method: msg.method,
        path: msg.path,
        params: msg.params,
        query: msg.query,
        raw_query: msg.raw_query,
        headers: msg.headers,
        ctx: msg.context,
        json: msg.json,
        text: msg.text,
        body,
        cancel: cancel.clone(),
    };
    let handlers = handlers.clone();
    let out = out.clone();
    let registry = in_flight.clone();
    let task_id = id.clone();
    let task_cancel = cancel.clone();
    // Holding the lock while spawning means the task can't finish and
    // remove itself before it's been inserted.
    let mut map = in_flight.lock().unwrap();
    let task = tokio::spawn(async move {
        let id = task_id;
        let result = match handlers.resolve(&req.handler) {
            Ok(handler) => AssertUnwindSafe(handler(req)).catch_unwind().await,
            Err(reason) => Ok(Err(crate::Error::msg(reason))),
        };
        match result {
            Ok(Ok(reply)) => send_reply(&out, &id, reply, &task_cancel).await,
            Ok(Err(e)) => {
                eprintln!("unsworn: {e}");
                out.error(&id, e.to_string());
            }
            Err(panic) => {
                let message = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "handler panicked".into());
                out.error(&id, format!("panic: {message}"));
            }
        }
        registry.lock().unwrap().remove(&id);
    });
    map.insert(id, (task.abort_handle(), cancel));
}

async fn send_reply(out: &Out, id: &str, reply: Reply, cancel: &Cancel) {
    let status = reply.status;
    let mut headers: BTreeMap<String, HeaderValues> = BTreeMap::new();
    for (name, value) in reply.headers {
        match headers.remove(&name) {
            None => headers.insert(name, HeaderValues::One(value)),
            Some(HeaderValues::One(first)) => {
                headers.insert(name, HeaderValues::Many(vec![first, value]))
            }
            Some(HeaderValues::Many(mut all)) => {
                all.push(value);
                headers.insert(name, HeaderValues::Many(all))
            }
        };
    }
    let mut response = Response::new(id, status);
    response.headers = headers;
    match reply.body {
        Body::Next(context) => {
            let serde_json::Value::Object(context) = context else {
                out.error(id, "Reply::next takes a JSON object".into());
                return;
            };
            out.send(Frame::new(Message::Continue(Continue {
                id: id.to_string(),
                context,
            })));
        }
        Body::Empty => out.send(Frame::new(Message::Response(response))),
        Body::Json(Ok(value)) => {
            response.json = Some(value);
            out.send(Frame::new(Message::Response(response)));
        }
        Body::Json(Err(e)) => out.error(id, format!("can't encode the JSON reply: {e}")),
        Body::Text(text) => {
            response.text = Some(text);
            out.send(Frame::new(Message::Response(response)));
        }
        Body::Bytes(bytes) if bytes.is_empty() => {
            out.send(Frame::new(Message::Response(response)));
        }
        Body::Bytes(bytes) => match Frame::with_body(Message::Response(response), bytes) {
            Ok(frame) => out.send(frame),
            Err(e) => out.error(id, e.to_string()),
        },
        Body::Stream(mut stream) => {
            response.stream = true;
            out.send(Frame::new(Message::Response(response)));
            while let Some(item) = stream.next().await {
                if cancel.is_cancelled() {
                    return;
                }
                match item {
                    Ok(chunk) if chunk.is_empty() => {}
                    Ok(chunk) => {
                        let message = Message::Chunk(Chunk {
                            id: id.to_string(),
                            text: None,
                            body_len: None,
                        });
                        match Frame::with_body(message, chunk) {
                            Ok(frame) => out.send(frame),
                            Err(e) => return out.error(id, e.to_string()),
                        }
                    }
                    Err(e) => return out.error(id, e.to_string()),
                }
            }
            out.send(Frame::new(Message::End(End { id: id.to_string() })));
        }
    }
}
