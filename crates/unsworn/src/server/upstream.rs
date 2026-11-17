//! Connections to upstreams that speak plain HTTP/1.1: `http://` ones and
//! those on a Unix socket.
//!
//! A proxied request to one goes over a connection kept here, not through
//! the general client in `support`, which has TLS and HTTP/2. Idle
//! connections are kept in one list per thread, so a request usually
//! reuses a connection its own thread used last and doesn't wait for a
//! lock another thread holds.

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::uri::Authority;
use http::{HeaderValue, Request, Response, StatusCode, Uri, header};
use http_body::{Frame, SizeHint};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::client::conn::http1::{self, SendRequest};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, UnixStream};

use super::support::{Body, BoxError, thread_slot, thread_slots};

/// How long a connection may sit unused before it's closed.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

struct Idle {
    sender: SendRequest<Body>,
    since: Instant,
}

/// Where an upstream listens.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Peer {
    /// A host and port.
    Tcp(Authority),
    /// A Unix socket's path.
    Unix(Arc<str>),
}

/// One thread's idle connections, by upstream. The newest is at the back.
type Shard = HashMap<Peer, VecDeque<Idle>>;

pub struct Upstreams {
    shards: Box<[Mutex<Shard>]>,
}

impl Upstreams {
    /// Must be called from within the runtime: it starts the task that
    /// closes connections left idle.
    pub fn new() -> Arc<Upstreams> {
        let upstreams = Arc::new(Upstreams {
            shards: (0..thread_slots()).map(|_| Mutex::default()).collect(),
        });
        tokio::spawn(sweep(Arc::downgrade(&upstreams)));
        upstreams
    }

    fn shard(&self) -> &Mutex<Shard> {
        &self.shards[thread_slot() % self.shards.len()]
    }

    /// An idle connection to `peer` that can take a request now: one this
    /// thread left, or failing that, another thread's. A request can end on
    /// a different thread from the one it began on, so without the second
    /// look a thread whose own list ran dry would open a connection while
    /// others sat idle elsewhere.
    fn take(&self, peer: &Peer) -> Option<SendRequest<Body>> {
        let own = thread_slot() % self.shards.len();
        if let Some(sender) = Self::take_from(&mut self.shards[own].lock().unwrap(), peer) {
            return Some(sender);
        }
        (1..self.shards.len()).find_map(|step| {
            let other = (own + step) % self.shards.len();
            Self::take_from(&mut self.shards[other].lock().unwrap(), peer)
        })
    }

    fn take_from(shard: &mut Shard, peer: &Peer) -> Option<SendRequest<Body>> {
        let idle = shard.get_mut(peer)?;
        while let Some(Idle { sender, since }) = idle.pop_back() {
            if since.elapsed() >= IDLE_TIMEOUT {
                // Everything in front of it is older still.
                idle.clear();
                return None;
            }
            if sender.is_ready() {
                return Some(sender);
            }
        }
        None
    }

    fn put(&self, peer: &Peer, sender: SendRequest<Body>) {
        let idle = Idle {
            sender,
            since: Instant::now(),
        };
        let mut shard = self.shard().lock().unwrap();
        match shard.get_mut(peer) {
            Some(list) => list.push_back(idle),
            None => {
                shard.insert(peer.clone(), VecDeque::from([idle]));
            }
        }
    }

    /// Sends a request to `peer` and returns its response. The connection
    /// goes back to the idle list when the response's body has been read
    /// to its end.
    pub async fn send(
        self: &Arc<Self>,
        peer: Peer,
        mut req: Request<Body>,
    ) -> Result<Response<Body>, BoxError> {
        // An HTTP/1.1 request to an origin names the path only, and says
        // which host it's for in `Host`.
        if req.uri().authority().is_some() {
            *req.uri_mut() = match req.uri().path_and_query() {
                Some(path) => Uri::from(path.clone()),
                None => Uri::from_static("/"),
            };
        }
        if !req.headers().contains_key(header::HOST) {
            let host = match &peer {
                Peer::Tcp(authority) => HeaderValue::from_str(authority.as_str()).ok(),
                Peer::Unix(_) => Some(HeaderValue::from_static("localhost")),
            };
            if let Some(host) = host {
                req.headers_mut().insert(header::HOST, host);
            }
        }
        loop {
            let (mut sender, reused) = match self.take(&peer) {
                Some(sender) => (sender, true),
                None => (connect(&peer).await?, false),
            };
            match sender.try_send_request(req).await {
                // The connection is the tunnel's from here on.
                Ok(resp) if resp.status() == StatusCode::SWITCHING_PROTOCOLS => {
                    return Ok(resp.map(|body| body.map_err(BoxError::from).boxed_unsync()));
                }
                Ok(resp) => {
                    let lease = Lease {
                        sender,
                        peer,
                        upstreams: self.clone(),
                    };
                    return Ok(resp.map(|body| Returning::new(body, lease).boxed_unsync()));
                }
                Err(mut e) => match e.take_message() {
                    // The upstream closed the connection while it was idle,
                    // and the request never left. Another connection can
                    // take it.
                    Some(unsent) if reused => req = unsent,
                    _ => return Err(e.into_error().into()),
                },
            }
        }
    }
}

async fn connect(peer: &Peer) -> Result<SendRequest<Body>, BoxError> {
    match peer {
        Peer::Tcp(authority) => {
            let host = authority.host();
            // An IPv6 address is in brackets in a URL.
            let host = host
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .unwrap_or(host);
            let stream = TcpStream::connect((host, authority.port_u16().unwrap_or(80))).await?;
            let _ = stream.set_nodelay(true);
            handshake(stream).await
        }
        Peer::Unix(path) => handshake(UnixStream::connect(&**path).await?).await,
    }
}

async fn handshake<T>(stream: T) -> Result<SendRequest<Body>, BoxError>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sender, conn) = http1::handshake(TokioIo::new(stream)).await?;
    tokio::spawn(async move {
        if let Err(e) = conn.with_upgrades().await {
            tracing::debug!("upstream connection closed: {e}");
        }
    });
    Ok(sender)
}

/// Closes connections that have been idle too long, for as long as the
/// server lives.
async fn sweep(upstreams: Weak<Upstreams>) {
    let mut tick = tokio::time::interval(IDLE_TIMEOUT / 2);
    loop {
        tick.tick().await;
        let Some(upstreams) = upstreams.upgrade() else {
            return;
        };
        for shard in &upstreams.shards {
            let mut shard = shard.lock().unwrap();
            for idle in shard.values_mut() {
                idle.retain(|c| c.since.elapsed() < IDLE_TIMEOUT && !c.sender.is_closed());
            }
            shard.retain(|_, idle| !idle.is_empty());
        }
    }
}

/// A connection in use by one response, and where it goes afterwards.
struct Lease {
    sender: SendRequest<Body>,
    peer: Peer,
    upstreams: Arc<Upstreams>,
}

impl Lease {
    fn release(mut self) {
        if self.sender.is_ready() {
            self.upstreams.put(&self.peer, self.sender);
        } else if !self.sender.is_closed() {
            // The connection's own task hasn't yet seen that the response
            // is over.
            tokio::spawn(async move {
                if self.sender.ready().await.is_ok() {
                    self.upstreams.put(&self.peer, self.sender);
                }
            });
        }
    }
}

/// A response body that gives its connection back when it ends. A body
/// dropped before its end closes the connection, since the rest of the
/// response is still on it.
struct Returning {
    body: Incoming,
    lease: Option<Lease>,
}

impl Returning {
    fn new(body: Incoming, lease: Lease) -> Returning {
        let mut returning = Returning {
            body,
            lease: Some(lease),
        };
        returning.release_at_end();
        returning
    }

    fn release_at_end(&mut self) {
        if http_body::Body::is_end_stream(&self.body)
            && let Some(lease) = self.lease.take()
        {
            lease.release();
        }
    }
}

impl http_body::Body for Returning {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let frame = Pin::new(&mut self.body).poll_frame(cx);
        match &frame {
            Poll::Ready(None) => {
                if let Some(lease) = self.lease.take() {
                    lease.release();
                }
            }
            Poll::Ready(Some(Ok(_))) => self.release_at_end(),
            Poll::Ready(Some(Err(_))) => self.lease = None,
            Poll::Pending => {}
        }
        frame.map_err(BoxError::from)
    }

    fn is_end_stream(&self) -> bool {
        http_body::Body::is_end_stream(&self.body)
    }

    fn size_hint(&self) -> SizeHint {
        http_body::Body::size_hint(&self.body)
    }
}
