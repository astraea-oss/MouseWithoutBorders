//! An async client for the local control endpoint.
//!
//! One background task reads the connection: responses complete their
//! pending request, and events go to a bounded channel. If the UI stops
//! draining events, the oldest are dropped rather than growing without
//! bound; snapshots are whole, so the next one repairs any gap.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    sync::{mpsc, oneshot},
    time,
};

use crate::{
    framing::{FrameError, preamble, read_frame_async, write_frame_async},
    protocol::{
        ClientMessage, ControlError, ErrorKind, Event, Hello, Outcome, PROTOCOL_VERSION, Reply,
        Request, ServerMessage,
    },
};

/// How long the server has to answer the preamble.
const HELLO_TIMEOUT: Duration = Duration::from_secs(2);
/// Settings saves validate and write a file; everything else is quicker.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const EVENT_BUFFER: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// Nothing is listening: the backend isn't running.
    #[error("the backend isn't running ({0})")]
    Absent(String),
    /// `server` is 0 for a backend from before the framed protocol.
    #[error("the backend speaks control protocol {server}, this UI speaks {PROTOCOL_VERSION}")]
    Incompatible { server: u32 },
    #[error("the backend didn't answer: {0}")]
    NoAnswer(String),
}

#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    #[error(transparent)]
    Control(#[from] ControlError),
    #[error("lost the connection to the backend")]
    Disconnected,
    #[error("the backend didn't answer in time")]
    Timeout,
}

type Pending = Arc<Mutex<HashMap<u32, oneshot::Sender<Outcome>>>>;

/// A connected client. Dropping it closes the connection, which cancels any
/// phone pairing it started.
pub struct Client {
    writer: tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>,
    pending: Pending,
    next_id: AtomicU32,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Events pushed after [`Request::Subscribe`]. Yields `None` once the
/// connection is gone.
pub struct Events(mpsc::Receiver<Event>);

impl Events {
    pub async fn recv(&mut self) -> Option<Event> {
        self.0.recv().await
    }
}

impl Client {
    /// Connects to this user's backend.
    pub async fn connect() -> Result<(Hello, Client, Events), ConnectError> {
        #[cfg(unix)]
        {
            let path = crate::endpoint::socket_path()
                .ok_or_else(|| ConnectError::Absent("XDG_RUNTIME_DIR isn't set".into()))?;
            let stream = tokio::net::UnixStream::connect(&path)
                .await
                .map_err(|error| ConnectError::Absent(format!("{}: {error}", path.display())))?;
            let (reader, writer) = stream.into_split();
            Self::handshake(reader, writer).await
        }
        #[cfg(windows)]
        {
            let name = crate::endpoint::pipe_name();
            let pipe = tokio::net::windows::named_pipe::ClientOptions::new()
                .open(&name)
                .map_err(|error| ConnectError::Absent(format!("{name}: {error}")))?;
            let (reader, writer) = tokio::io::split(pipe);
            Self::handshake(reader, writer).await
        }
    }

    /// Speaks the protocol over any byte stream (tests use an in-memory one).
    pub async fn handshake(
        mut reader: impl AsyncRead + Send + Unpin + 'static,
        mut writer: impl AsyncWrite + Send + Unpin + 'static,
    ) -> Result<(Hello, Client, Events), ConnectError> {
        let no_answer = |error: FrameError| ConnectError::NoAnswer(error.to_string());
        writer
            .write_all(&preamble(PROTOCOL_VERSION))
            .await
            .map_err(|error| no_answer(error.into()))?;
        // A backend from before this protocol waits for a text command line
        // and never answers the preamble. Report it as protocol 0.
        let hello = time::timeout(
            HELLO_TIMEOUT,
            read_frame_async::<ServerMessage>(&mut reader),
        )
        .await
        .map_err(|_| ConnectError::Incompatible { server: 0 })?
        .map_err(no_answer)?;
        let hello = match hello {
            ServerMessage::Hello(hello) => hello,
            ServerMessage::Incompatible { server_protocol } => {
                return Err(ConnectError::Incompatible {
                    server: server_protocol,
                });
            }
            other => {
                return Err(ConnectError::NoAnswer(format!(
                    "expected a hello, got {other:?}"
                )));
            }
        };
        if hello.protocol != PROTOCOL_VERSION {
            return Err(ConnectError::Incompatible {
                server: hello.protocol,
            });
        }

        let pending: Pending = Arc::default();
        let (events_tx, events_rx) = mpsc::channel(EVENT_BUFFER);
        let reader = tokio::spawn(read_loop(reader, pending.clone(), events_tx));
        Ok((
            hello,
            Client {
                writer: tokio::sync::Mutex::new(Box::new(writer)),
                pending,
                next_id: AtomicU32::new(1),
                reader,
            },
            Events(events_rx),
        ))
    }

    pub async fn request(&self, request: Request) -> Result<Reply, RequestError> {
        self.request_with_timeout(request, REQUEST_TIMEOUT).await
    }

    pub async fn request_with_timeout(
        &self,
        request: Request,
        timeout: Duration,
    ) -> Result<Reply, RequestError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(id, reply_tx);
        let sent = {
            let mut writer = self.writer.lock().await;
            write_frame_async(&mut *writer, &ClientMessage { id, request }).await
        };
        if let Err(error) = sent {
            self.forget(id);
            return Err(match error {
                FrameError::TooLarge(_) | FrameError::Malformed(_) => {
                    ControlError::new(ErrorKind::BadRequest, error.to_string()).into()
                }
                _ => RequestError::Disconnected,
            });
        }
        match time::timeout(timeout, reply_rx).await {
            Ok(Ok(Outcome::Ok(reply))) => Ok(reply),
            Ok(Ok(Outcome::Error(error))) => Err(error.into()),
            Ok(Err(_)) => Err(RequestError::Disconnected),
            Err(_) => {
                self.forget(id);
                Err(RequestError::Timeout)
            }
        }
    }

    /// Whether the reader still has a connection.
    pub fn is_connected(&self) -> bool {
        !self.reader.is_finished()
    }

    fn forget(&self, id: u32) {
        self.pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&id);
    }
}

async fn read_loop(
    mut reader: impl AsyncRead + Unpin,
    pending: Pending,
    events: mpsc::Sender<Event>,
) {
    loop {
        match read_frame_async::<ServerMessage>(&mut reader).await {
            Ok(ServerMessage::Response { id, outcome }) => {
                let waiter = pending
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&id);
                if let Some(waiter) = waiter {
                    let _ = waiter.send(outcome);
                }
            }
            Ok(ServerMessage::Event { event }) => {
                // Full: drop this one. Snapshots are whole, so the next
                // repairs the gap, and pairing outcomes are rare enough that
                // a stuck UI is the only way to lose one.
                let _ = events.try_send(event);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    // Dropping the waiters fails their requests as disconnected.
    pending
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clear();
}
