use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use snow::{Builder, params::NoiseParams};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf},
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
        let file = IdentityFile {
            private_key_hex: hex::encode(self.private),
            public_key_hex: hex::encode(self.public),
        };
        write_atomic(path.as_ref(), toml::to_string_pretty(&file)?, true).await
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
    /// PC peers, keyed by device name.
    #[serde(default)]
    pub peers: BTreeMap<String, PinnedPeer>,
    /// Phones, keyed by device name. Kept apart from PC peers so a phone can
    /// never take a PC's place (or the reverse) by choosing its name. Omitted
    /// when empty, so pin files without phones are unchanged.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub phones: BTreeMap<String, PinnedPhone>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedPeer {
    pub fingerprint: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedPhone {
    pub fingerprint: String,
    #[serde(default = "PhoneFeature::all")]
    pub allowed_features: BTreeSet<PhoneFeature>,
}

/// What a paired phone may use. Enforced by the receiver in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PhoneFeature {
    Clipboard,
    Notifications,
    Audio,
}

impl PhoneFeature {
    pub fn all() -> BTreeSet<Self> {
        BTreeSet::from([Self::Clipboard, Self::Notifications, Self::Audio])
    }
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
        write_atomic(path.as_ref(), toml::to_string_pretty(self)?, false).await
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

    pub fn phone_status(&self, phone: &str, fingerprint: &str) -> PinStatus {
        match self.phones.get(phone) {
            Some(pinned) if pinned.fingerprint == fingerprint => PinStatus::Trusted,
            Some(pinned) => PinStatus::Changed {
                expected: pinned.fingerprint.clone(),
            },
            None => PinStatus::Unknown,
        }
    }

    pub fn phone(&self, phone: &str) -> Option<&PinnedPhone> {
        self.phones.get(phone)
    }

    pub fn pin_phone(
        &mut self,
        phone: impl Into<String>,
        fingerprint: impl Into<String>,
        allowed_features: BTreeSet<PhoneFeature>,
    ) {
        self.phones.insert(
            phone.into(),
            PinnedPhone {
                fingerprint: fingerprint.into(),
                allowed_features,
            },
        );
    }

    pub fn remove_phone(&mut self, phone: &str) -> Option<PinnedPhone> {
        self.phones.remove(phone)
    }

    /// Whether this key is pinned for a PC peer, under any name.
    pub fn is_pc_key(&self, fingerprint: &str) -> bool {
        self.peers
            .values()
            .any(|pinned| pinned.fingerprint == fingerprint)
    }

    /// Whether this key is pinned for a phone, under any name.
    pub fn is_phone_key(&self, fingerprint: &str) -> bool {
        self.phones
            .values()
            .any(|pinned| pinned.fingerprint == fingerprint)
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

    pub fn get_ref(&self) -> &S {
        &self.io
    }

    pub fn split(self) -> (NoiseReader<ReadHalf<S>>, NoiseWriter<WriteHalf<S>>) {
        let (reader, writer) = tokio::io::split(self.io);
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

pub struct NoiseReader<R> {
    io: R,
    transport: Arc<Mutex<snow::TransportState>>,
}

impl<R> NoiseReader<R>
where
    R: AsyncRead + Unpin,
{
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

pub struct NoiseWriter<W> {
    io: W,
    transport: Arc<Mutex<snow::TransportState>>,
}

impl<W> NoiseWriter<W>
where
    W: AsyncWrite + Unpin,
{
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

/// Replaces `path` with `contents` so that a crash or failed write at any
/// point leaves either the previous file or the new one, never a mix: write a
/// temporary file beside it, sync it, then rename it over the original.
/// `private` restricts the file to its owner on Unix.
async fn write_atomic(path: &Path, contents: String, private: bool) -> Result<()> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        write_atomic_with(&path, private, |file| file.write_all(contents.as_bytes()))
    })
    .await
    .map_err(|err| CryptoError::Io(std::io::Error::other(err)))?
}

fn write_atomic_with(
    path: &Path,
    private: bool,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<()> {
    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let file_name = path.file_name().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
    })?;
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));

    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        #[cfg(not(unix))]
        let _ = private;
        let mut file = options.open(&temp)?;
        write(&mut file)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)?;
        // Persist the rename itself; Windows has no directory handles to sync.
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map_err(CryptoError::Io)
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
    async fn pin_files_without_phones_load_and_save_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pins.toml");
        let legacy = "[peers.\"Main PC\"]\nfingerprint = \"aa:bb\"\n";
        std::fs::write(&path, legacy).unwrap();

        let store = PinStore::load_or_default(&path).await.unwrap();
        assert_eq!(store.status("Main PC", "aa:bb"), PinStatus::Trusted);
        assert!(store.phones.is_empty());
        store.save(&path).await.unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), legacy);
    }

    #[tokio::test]
    async fn phone_pins_round_trip_apart_from_pc_pins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pins.toml");
        let mut store = PinStore::default();
        store.pin("Pixel", "pc-key");
        store.pin_phone(
            "Pixel",
            "phone-key",
            BTreeSet::from([PhoneFeature::Clipboard]),
        );
        store.save(&path).await.unwrap();

        let loaded = PinStore::load_or_default(&path).await.unwrap();
        assert_eq!(loaded.status("Pixel", "pc-key"), PinStatus::Trusted);
        assert_eq!(
            loaded.phone_status("Pixel", "phone-key"),
            PinStatus::Trusted
        );
        assert_eq!(
            loaded.phone_status("Pixel", "pc-key"),
            PinStatus::Changed {
                expected: "phone-key".to_string()
            }
        );
        assert_eq!(
            loaded.phone("Pixel").unwrap().allowed_features,
            BTreeSet::from([PhoneFeature::Clipboard])
        );
        assert!(loaded.is_pc_key("pc-key") && !loaded.is_pc_key("phone-key"));
        assert!(loaded.is_phone_key("phone-key") && !loaded.is_phone_key("pc-key"));
    }

    #[test]
    fn phone_pins_without_features_allow_everything() {
        let store: PinStore =
            toml::from_str("[phones.Pixel]\nfingerprint = \"phone-key\"\n").unwrap();
        assert_eq!(
            store.phone("Pixel").unwrap().allowed_features,
            PhoneFeature::all()
        );
    }

    #[tokio::test]
    async fn pin_store_save_replaces_file_without_leaving_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pins.toml");
        let mut store = PinStore::default();
        store.pin("windows", "first");
        store.save(&path).await.unwrap();
        store.pin("windows", "second");
        store.save(&path).await.unwrap();

        let loaded = PinStore::load_or_default(&path).await.unwrap();
        assert_eq!(loaded.status("windows", "second"), PinStatus::Trusted);
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "only pins.toml should remain");
    }

    #[tokio::test]
    async fn interrupted_save_leaves_previous_file_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pins.toml");
        let mut store = PinStore::default();
        store.pin("windows", "existing");
        store.save(&path).await.unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let result = write_atomic_with(&path, false, |file| {
            file.write_all(b"[peers.windo")?;
            Err(std::io::Error::other("simulated crash mid-write"))
        });

        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "the partial temp file should be removed");
        let loaded = PinStore::load_or_default(&path).await.unwrap();
        assert_eq!(loaded.status("windows", "existing"), PinStatus::Trusted);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn identity_file_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.toml");
        IdentityKey::load_or_create(&path).await.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
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

    #[tokio::test]
    async fn split_noise_session_uses_the_same_duplex_transport_path_as_tcp() {
        let initiator_identity = IdentityKey::generate().unwrap();
        let responder_identity = IdentityKey::generate().unwrap();
        let expected_responder = responder_identity.fingerprint();
        let (client, server) = tokio::io::duplex(4096);

        let initiator = tokio::spawn(async move {
            let (session, _) =
                initiate_noise_session(client, &initiator_identity, Some(&expected_responder))
                    .await
                    .unwrap();
            let (mut reader, mut writer) = session.split();
            writer.write_packet(b"split hello").await.unwrap();
            reader.read_packet().await.unwrap()
        });

        let responder = tokio::spawn(async move {
            let (session, _) = accept_noise_session(server, &responder_identity)
                .await
                .unwrap();
            let (mut reader, mut writer) = session.split();
            let message = reader.read_packet().await.unwrap();
            writer.write_packet(b"split world").await.unwrap();
            message
        });

        assert_eq!(initiator.await.unwrap(), b"split world");
        assert_eq!(responder.await.unwrap(), b"split hello");
    }
}
