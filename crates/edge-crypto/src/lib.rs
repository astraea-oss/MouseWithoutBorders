use std::{collections::BTreeMap, path::Path, sync::Arc};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use snow::{Builder, params::NoiseParams};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::Mutex,
};

const MAX_NOISE_PACKET_BYTES: u32 = 4 * 1024 * 1024 + 16;

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("noise error: {0}")]
    Noise(#[from] snow::Error),
    #[error("invalid key length: expected 32 bytes, got {0}")]
    InvalidKeyLength(usize),
    #[error("hex decode error: {0}")]
    Hex(#[from] hex::FromHexError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("toml decode error: {0}")]
    TomlDecode(#[from] toml::de::Error),
    #[error("toml encode error: {0}")]
    TomlEncode(#[from] toml::ser::Error),
    #[error("noise packet too large: {0} bytes")]
    PacketTooLarge(u32),
    #[error("peer fingerprint mismatch: expected {expected}, got {actual}")]
    FingerprintMismatch { expected: String, actual: String },
}

pub type Result<T> = std::result::Result<T, CryptoError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityKey {
    pub private: [u8; 32],
    pub public: [u8; 32],
}

impl IdentityKey {
    pub fn generate() -> Result<Self> {
        let keypair = Builder::new(noise_params()?).generate_keypair()?;
        Ok(Self {
            private: vec_to_key(keypair.private)?,
            public: vec_to_key(keypair.public)?,
        })
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.public)
    }

    pub async fn load_or_create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match tokio::fs::read_to_string(path).await {
            Ok(text) => IdentityFile::decode(&text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let key = Self::generate()?;
                key.save(path).await?;
                Ok(key)
            }
            Err(err) => Err(CryptoError::Io(err)),
        }
    }

    pub async fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = IdentityFile {
            private_key_hex: hex::encode(self.private),
            public_key_hex: hex::encode(self.public),
        };
        tokio::fs::write(path, toml::to_string_pretty(&file)?).await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IdentityFile {
    private_key_hex: String,
    public_key_hex: String,
}

impl IdentityFile {
    fn decode(text: &str) -> Result<IdentityKey> {
        let file: Self = toml::from_str(text)?;
        Ok(IdentityKey {
            private: vec_to_key(hex::decode(file.private_key_hex)?)?,
            public: vec_to_key(hex::decode(file.public_key_hex)?)?,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PinStore {
    #[serde(default)]
    pub peers: BTreeMap<String, PinnedPeer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedPeer {
    pub fingerprint: String,
}

impl PinStore {
    pub async fn load_or_default(path: impl AsRef<Path>) -> Result<Self> {
        match tokio::fs::read_to_string(path).await {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(CryptoError::Io(err)),
        }
    }

    pub async fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, toml::to_string_pretty(self)?).await?;
        Ok(())
    }

    pub fn status(&self, peer: &str, fingerprint: &str) -> PinStatus {
        match self.peers.get(peer) {
            Some(pinned) if pinned.fingerprint == fingerprint => PinStatus::Trusted,
            Some(pinned) => PinStatus::Changed {
                expected: pinned.fingerprint.clone(),
            },
            None => PinStatus::Unknown,
        }
    }

    pub fn pin(&mut self, peer: impl Into<String>, fingerprint: impl Into<String>) {
        self.peers.insert(
            peer.into(),
            PinnedPeer {
                fingerprint: fingerprint.into(),
            },
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinStatus {
    Trusted,
    Unknown,
    Changed { expected: String },
}

impl PinStatus {
    pub fn is_trusted(&self) -> bool {
        matches!(self, Self::Trusted)
    }
}

pub fn fingerprint(public_key: &[u8]) -> String {
    let digest = Sha256::digest(public_key);
    let hex = hex::encode(digest);
    format!(
        "{}:{}:{}:{}",
        &hex[0..8],
        &hex[8..16],
        &hex[16..24],
        &hex[24..32]
    )
}

pub fn pairing_code(first_fingerprint: &str, second_fingerprint: &str) -> String {
    let (first, second) = if first_fingerprint <= second_fingerprint {
        (first_fingerprint, second_fingerprint)
    } else {
        (second_fingerprint, first_fingerprint)
    };
    let digest = Sha256::digest(format!("edge-kvm-pairing-v1\0{first}\0{second}").as_bytes());
    let value = u32::from_be_bytes(
        digest[0..4]
            .try_into()
            .expect("SHA-256 prefix is four bytes"),
    ) % 1_000_000;
    format!("{value:06}")
}

pub fn noise_params() -> Result<NoiseParams> {
    "Noise_XX_25519_ChaChaPoly_BLAKE2s"
        .parse()
        .map_err(CryptoError::Noise)
}

pub fn noise_builder(identity: &IdentityKey) -> Result<Builder<'_>> {
    Ok(Builder::new(noise_params()?).local_private_key(&identity.private))
}

pub struct NoiseSession<S> {
    io: S,
    transport: snow::TransportState,
}

impl<S> NoiseSession<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    pub async fn write_packet(&mut self, plaintext: &[u8]) -> Result<()> {
        let mut encrypted = vec![0; plaintext.len() + 16];
        let len = self.transport.write_message(plaintext, &mut encrypted)?;
        encrypted.truncate(len);
        write_packet(&mut self.io, &encrypted).await
    }

    pub async fn read_packet(&mut self) -> Result<Vec<u8>> {
        let encrypted = read_packet(&mut self.io).await?;
        let mut plaintext = vec![0; encrypted.len()];
        let len = self.transport.read_message(&encrypted, &mut plaintext)?;
        plaintext.truncate(len);
        Ok(plaintext)
    }

    pub fn into_inner(self) -> S {
        self.io
    }
}

impl NoiseSession<TcpStream> {
    pub fn split(self) -> (NoiseReader, NoiseWriter) {
        let (reader, writer) = self.io.into_split();
        let transport = Arc::new(Mutex::new(self.transport));
        (
            NoiseReader {
                io: reader,
                transport: transport.clone(),
            },
            NoiseWriter {
                io: writer,
                transport,
            },
        )
    }
}

pub struct NoiseReader {
    io: OwnedReadHalf,
    transport: Arc<Mutex<snow::TransportState>>,
}

impl NoiseReader {
    pub async fn read_packet(&mut self) -> Result<Vec<u8>> {
        let encrypted = read_packet(&mut self.io).await?;
        let mut plaintext = vec![0; encrypted.len()];
        let len = {
            let mut transport = self.transport.lock().await;
            transport.read_message(&encrypted, &mut plaintext)?
        };
        plaintext.truncate(len);
        Ok(plaintext)
    }
}

pub struct NoiseWriter {
    io: OwnedWriteHalf,
    transport: Arc<Mutex<snow::TransportState>>,
}

impl NoiseWriter {
    pub async fn write_packet(&mut self, plaintext: &[u8]) -> Result<()> {
        let mut encrypted = vec![0; plaintext.len() + 16];
        let len = {
            let mut transport = self.transport.lock().await;
            transport.write_message(plaintext, &mut encrypted)?
        };
        encrypted.truncate(len);
        write_packet(&mut self.io, &encrypted).await
    }
}

pub async fn initiate_noise_session<S>(
    mut io: S,
    identity: &IdentityKey,
    expected_peer_fingerprint: Option<&str>,
) -> Result<(NoiseSession<S>, String)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut noise = noise_builder(identity)?.build_initiator()?;
    let mut out = vec![0; 65_535];

    let len = noise.write_message(&[], &mut out)?;
    write_packet(&mut io, &out[..len]).await?;

    let message = read_packet(&mut io).await?;
    let mut payload = vec![0; 65_535];
    noise.read_message(&message, &mut payload)?;

    let len = noise.write_message(&[], &mut out)?;
    write_packet(&mut io, &out[..len]).await?;

    let remote_fingerprint = remote_static_fingerprint(&noise)?;
    if let Some(expected) = expected_peer_fingerprint
        && !expected.is_empty()
        && expected != remote_fingerprint
    {
        return Err(CryptoError::FingerprintMismatch {
            expected: expected.to_string(),
            actual: remote_fingerprint,
        });
    }

    Ok((
        NoiseSession {
            io,
            transport: noise.into_transport_mode()?,
        },
        remote_fingerprint,
    ))
}

pub async fn accept_noise_session<S>(
    mut io: S,
    identity: &IdentityKey,
) -> Result<(NoiseSession<S>, String)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut noise = noise_builder(identity)?.build_responder()?;
    let mut payload = vec![0; 65_535];
    let mut out = vec![0; 65_535];

    let message = read_packet(&mut io).await?;
    noise.read_message(&message, &mut payload)?;

    let len = noise.write_message(&[], &mut out)?;
    write_packet(&mut io, &out[..len]).await?;

    let message = read_packet(&mut io).await?;
    noise.read_message(&message, &mut payload)?;

    let remote_fingerprint = remote_static_fingerprint(&noise)?;
    Ok((
        NoiseSession {
            io,
            transport: noise.into_transport_mode()?,
        },
        remote_fingerprint,
    ))
}

async fn write_packet<W>(writer: &mut W, payload: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let len = u32::try_from(payload.len()).map_err(|_| CryptoError::PacketTooLarge(u32::MAX))?;
    if len > MAX_NOISE_PACKET_BYTES {
        return Err(CryptoError::PacketTooLarge(len));
    }
    writer.write_u32(len).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_packet<R>(reader: &mut R) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let len = reader.read_u32().await?;
    if len > MAX_NOISE_PACKET_BYTES {
        return Err(CryptoError::PacketTooLarge(len));
    }
    let mut payload = vec![0; len as usize];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

fn remote_static_fingerprint(noise: &snow::HandshakeState) -> Result<String> {
    let remote = noise
        .get_remote_static()
        .ok_or(CryptoError::InvalidKeyLength(0))?;
    Ok(fingerprint(remote))
}

fn vec_to_key(bytes: Vec<u8>) -> Result<[u8; 32]> {
    let len = bytes.len();
    bytes
        .try_into()
        .map_err(|_| CryptoError::InvalidKeyLength(len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_code_is_symmetric_and_six_digits() {
        let code = pairing_code("aa:bb", "cc:dd");
        assert_eq!(code, pairing_code("cc:dd", "aa:bb"));
        assert_eq!(code.len(), 6);
        assert!(code.bytes().all(|byte| byte.is_ascii_digit()));
        assert_ne!(code, pairing_code("aa:bb", "ee:ff"));
    }

    #[test]
    fn pin_status_does_not_mutate_until_confirmed() {
        let mut store = PinStore::default();
        assert_eq!(store.status("controller", "new"), PinStatus::Unknown);
        store.pin("controller", "old");
        assert_eq!(
            store.status("controller", "new"),
            PinStatus::Changed {
                expected: "old".to_string()
            }
        );
        assert_eq!(store.status("controller", "old"), PinStatus::Trusted);
    }

    #[tokio::test]
    async fn identity_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.toml");

        let first = IdentityKey::load_or_create(&path).await.unwrap();
        let second = IdentityKey::load_or_create(&path).await.unwrap();

        assert_eq!(first, second);
        assert_eq!(first.fingerprint(), second.fingerprint());
    }

    #[tokio::test]
    async fn noise_session_encrypts_packets() {
        let initiator_identity = IdentityKey::generate().unwrap();
        let responder_identity = IdentityKey::generate().unwrap();
        let expected_responder = responder_identity.fingerprint();
        let expected_initiator = initiator_identity.fingerprint();
        let expected_responder_for_task = expected_responder.clone();
        let (client, server) = tokio::io::duplex(4096);

        let initiator = tokio::spawn(async move {
            let (mut session, fingerprint) = initiate_noise_session(
                client,
                &initiator_identity,
                Some(&expected_responder_for_task),
            )
            .await
            .unwrap();
            session.write_packet(b"hello").await.unwrap();
            let reply = session.read_packet().await.unwrap();
            (fingerprint, reply)
        });

        let responder = tokio::spawn(async move {
            let (mut session, fingerprint) = accept_noise_session(server, &responder_identity)
                .await
                .unwrap();
            let message = session.read_packet().await.unwrap();
            session.write_packet(b"world").await.unwrap();
            (fingerprint, message)
        });

        let (initiator_peer, reply) = initiator.await.unwrap();
        let (responder_peer, message) = responder.await.unwrap();

        assert_eq!(initiator_peer, expected_responder);
        assert_eq!(responder_peer, expected_initiator);
        assert_eq!(message, b"hello");
        assert_eq!(reply, b"world");
    }
}
