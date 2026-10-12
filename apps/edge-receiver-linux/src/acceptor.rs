//! Accepts peer connections independently of the running session.
//!
//! Each connection gets its own handshake task (Noise, then the peer's
//! `Hello`), so a slow or silent peer never blocks anyone else. Authenticated
//! peers are handed to the receiver loop, which owns pairing and sessions.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use edge_crypto::{IdentityKey, NoiseSession, accept_noise_session};
use edge_protocol::{Frame, Hello};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Semaphore, mpsc},
    task::JoinSet,
    time,
};

use crate::{
    AbortOnDropTask, append_portable_log, configure_controller_socket, read_secure_frame,
    reject_pairing, validate_controller_hello,
};

/// Connections that may be mid-handshake or waiting for the receiver loop at
/// once. Anything beyond this is closed immediately.
const MAX_PENDING_PEERS: usize = 4;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// A peer that completed the Noise handshake and sent a valid `Hello`.
pub(crate) struct IncomingPeer {
    pub session: NoiseSession<TcpStream>,
    pub addr: SocketAddr,
    pub fingerprint: String,
    pub hello: Hello,
}

pub(crate) enum AcceptEvent {
    Peer(Box<IncomingPeer>),
    /// A connection failed before it authenticated. Already logged; the
    /// message is for the tray.
    Failed(String),
}

pub(crate) struct Acceptor {
    events: mpsc::Receiver<AcceptEvent>,
    _task: AbortOnDropTask,
}

impl Acceptor {
    pub fn spawn(listener: TcpListener, identity: IdentityKey, log_path: PathBuf) -> Self {
        let (events_tx, events) = mpsc::channel(1);
        let task = tokio::spawn(accept_loop(listener, identity, log_path, events_tx));
        Self {
            events,
            _task: AbortOnDropTask(task),
        }
    }

    pub async fn recv(&mut self) -> Option<AcceptEvent> {
        self.events.recv().await
    }
}

async fn accept_loop(
    listener: TcpListener,
    identity: IdentityKey,
    log_path: PathBuf,
    events: mpsc::Sender<AcceptEvent>,
) {
    let identity = Arc::new(identity);
    let permits = Arc::new(Semaphore::new(MAX_PENDING_PEERS));
    // Dropping the set (when the receiver stops) aborts unfinished handshakes.
    let mut handshakes = JoinSet::new();
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(incoming) => incoming,
            Err(err) => {
                tracing::warn!(%err, "failed to accept connection");
                append_portable_log(&log_path, format!("failed to accept connection: {err:#}"));
                time::sleep(ACCEPT_ERROR_BACKOFF).await;
                continue;
            }
        };
        while handshakes.try_join_next().is_some() {}
        // The permit is held until the receiver loop takes the peer, so
        // unauthenticated peers can't pile up while it's busy.
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            tracing::warn!(%addr, "too many pending connections; closing");
            append_portable_log(
                &log_path,
                format!("too many pending connections; closed {addr}"),
            );
            continue;
        };
        let identity = identity.clone();
        let log_path = log_path.clone();
        let events = events.clone();
        handshakes.spawn(async move {
            if let Some(event) = handshake(stream, addr, &identity, &log_path).await {
                events.send(event).await.ok();
            }
            drop(permit);
        });
    }
}

async fn handshake(
    stream: TcpStream,
    addr: SocketAddr,
    identity: &IdentityKey,
    log_path: &Path,
) -> Option<AcceptEvent> {
    if let Err(err) = configure_controller_socket(&stream) {
        tracing::warn!(%err, %addr, "failed to configure controller socket timeout");
        append_portable_log(
            log_path,
            format!("failed to configure controller socket timeout for {addr}: {err:#}"),
        );
    }
    tracing::info!(%addr, "controller connected");
    append_portable_log(log_path, format!("controller connected: {addr}"));

    let (mut session, fingerprint) =
        match time::timeout(HANDSHAKE_TIMEOUT, accept_noise_session(stream, identity)).await {
            Ok(Ok(session)) => session,
            Ok(Err(err)) => {
                tracing::warn!(%err, "Noise handshake failed");
                append_portable_log(log_path, format!("Noise handshake failed: {err:#}"));
                return Some(AcceptEvent::Failed(format!(
                    "Noise handshake failed: {err}"
                )));
            }
            Err(_) => {
                let message = "Noise handshake timed out";
                tracing::warn!(message);
                append_portable_log(log_path, message);
                return Some(AcceptEvent::Failed(message.to_string()));
            }
        };

    let hello = match time::timeout(HANDSHAKE_TIMEOUT, read_secure_frame(&mut session)).await {
        Ok(Ok(Frame::Hello(hello))) => hello,
        Ok(Ok(other)) => {
            tracing::warn!(?other, "first frame was not Hello");
            return None;
        }
        Ok(Err(err)) => {
            tracing::warn!(%err, "failed to read Hello");
            append_portable_log(log_path, format!("failed to read Hello: {err:#}"));
            return Some(AcceptEvent::Failed(format!("failed to read Hello: {err}")));
        }
        Err(_) => {
            let message = "timed out waiting for controller hello";
            tracing::warn!(message);
            append_portable_log(log_path, message);
            return Some(AcceptEvent::Failed(message.to_string()));
        }
    };

    if let Err(error) = validate_controller_hello(&hello, &fingerprint) {
        reject_pairing(&mut session, "invalid_hello", &error.to_string()).await;
        append_portable_log(log_path, format!("rejected controller hello: {error:#}"));
        return Some(AcceptEvent::Failed(error.to_string()));
    }

    Some(AcceptEvent::Peer(Box::new(IncomingPeer {
        session,
        addr,
        fingerprint,
        hello,
    })))
}

#[cfg(test)]
mod tests {
    use edge_common::Role;
    use edge_crypto::initiate_noise_session;
    use edge_protocol::{PROTOCOL_VERSION, RemoteError};
    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::write_secure_frame;

    fn hello_for(identity: &IdentityKey, name: &str, protocol_version: u16) -> Frame {
        Frame::Hello(Hello {
            protocol_version,
            device_name: name.to_string(),
            role: Role::Controller,
            public_key_fingerprint: identity.fingerprint(),
            capabilities: Vec::new(),
            extensions: Vec::new(),
            node_capabilities: Vec::new(),
        })
    }

    async fn start() -> (Acceptor, SocketAddr, tempfile::TempDir) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let acceptor = Acceptor::spawn(
            listener,
            IdentityKey::generate().unwrap(),
            dir.path().join("receiver.log"),
        );
        (acceptor, addr, dir)
    }

    async fn connect(
        addr: SocketAddr,
        name: &str,
        protocol_version: u16,
    ) -> NoiseSession<TcpStream> {
        let identity = IdentityKey::generate().unwrap();
        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut session, _) = initiate_noise_session(stream, &identity, None)
            .await
            .unwrap();
        write_secure_frame(&mut session, &hello_for(&identity, name, protocol_version))
            .await
            .unwrap();
        session
    }

    async fn next_peer(acceptor: &mut Acceptor) -> IncomingPeer {
        match time::timeout(Duration::from_secs(5), acceptor.recv()).await {
            Ok(Some(AcceptEvent::Peer(peer))) => *peer,
            Ok(Some(AcceptEvent::Failed(message))) => panic!("unexpected failure: {message}"),
            Ok(None) => panic!("acceptor stopped"),
            Err(_) => panic!("timed out waiting for a peer"),
        }
    }

    #[tokio::test]
    async fn silent_connection_does_not_block_other_peers() {
        let (mut acceptor, addr, _dir) = start().await;
        // Connects but never starts the Noise handshake.
        let _silent = TcpStream::connect(addr).await.unwrap();
        let _session = connect(addr, "windows", PROTOCOL_VERSION).await;

        let peer = next_peer(&mut acceptor).await;
        assert_eq!(peer.hello.device_name, "windows");
    }

    #[tokio::test]
    async fn peers_are_accepted_while_an_earlier_peer_is_held() {
        let (mut acceptor, addr, _dir) = start().await;
        let _first_session = connect(addr, "first", PROTOCOL_VERSION).await;
        let first = next_peer(&mut acceptor).await;
        let _second_session = connect(addr, "second", PROTOCOL_VERSION).await;
        let second = next_peer(&mut acceptor).await;

        assert_eq!(first.hello.device_name, "first");
        assert_eq!(second.hello.device_name, "second");
    }

    #[tokio::test]
    async fn connections_beyond_the_pending_limit_are_closed() {
        let (_acceptor, addr, _dir) = start().await;
        let mut silent = Vec::new();
        for _ in 0..MAX_PENDING_PEERS {
            silent.push(TcpStream::connect(addr).await.unwrap());
        }
        // Let the acceptor take the first connections before the extra one.
        time::sleep(Duration::from_millis(100)).await;
        let mut extra = TcpStream::connect(addr).await.unwrap();

        let closed = time::timeout(Duration::from_secs(5), async {
            loop {
                if extra.write_all(&[0; 64]).await.is_err() {
                    return;
                }
                time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(closed.is_ok(), "the extra connection should be closed");
    }

    #[tokio::test]
    async fn incompatible_hello_is_rejected_with_an_error_frame() {
        let (mut acceptor, addr, _dir) = start().await;
        let mut session = connect(addr, "old", PROTOCOL_VERSION - 1).await;

        match time::timeout(Duration::from_secs(5), acceptor.recv()).await {
            Ok(Some(AcceptEvent::Failed(message))) => {
                assert!(message.contains("protocol version"), "{message}");
            }
            _ => panic!("expected a failure event"),
        }
        match read_secure_frame(&mut session).await.unwrap() {
            Frame::Error(RemoteError { code, .. }) => assert_eq!(code, "invalid_hello"),
            other => panic!("expected an error frame, got {other:?}"),
        }
    }
}
