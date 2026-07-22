use std::collections::BTreeMap;

use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit, Nonce,
    aead::{Aead, OsRng, Payload, rand_core::RngCore},
};
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
pub const FRAME_MS: u16 = 5;
pub const SAMPLES_PER_CHANNEL: usize = 240;
pub const SAMPLES_PER_FRAME: usize = SAMPLES_PER_CHANNEL * CHANNELS;
pub const MAX_DATAGRAM_BYTES: usize = 1_200;
pub const PCM_BYTES_PER_FRAME: usize = SAMPLES_PER_FRAME * 2;

const MAGIC: [u8; 4] = *b"EKA1";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 36;
pub const FLAG_PROBE: u8 = 1;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("audio packet is truncated")]
    Truncated,
    #[error("invalid audio packet magic or version")]
    InvalidPacket,
    #[error("audio packet belongs to another session")]
    WrongSession,
    #[error("audio datagram exceeds {MAX_DATAGRAM_BYTES} bytes")]
    Oversized,
    #[error("audio authentication failed")]
    Authentication,
}

pub type Result<T> = std::result::Result<T, AudioError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSecrets {
    pub session_id: [u8; 16],
    pub session_salt: [u8; 4],
    pub session_key: [u8; 32],
}

impl SessionSecrets {
    pub fn generate() -> Self {
        let mut session_id = [0; 16];
        let mut session_salt = [0; 4];
        OsRng.fill_bytes(&mut session_id);
        OsRng.fill_bytes(&mut session_salt);
        let key = ChaCha20Poly1305::generate_key(&mut OsRng);
        Self {
            session_id,
            session_salt,
            session_key: key.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    pub sequence: u64,
    pub sample_timestamp: u32,
    pub flags: u8,
    pub payload: Vec<u8>,
}

#[derive(Clone)]
pub struct PacketCipher {
    session_id: [u8; 16],
    session_salt: [u8; 4],
    cipher: ChaCha20Poly1305,
}

impl PacketCipher {
    pub fn new(secrets: &SessionSecrets) -> Self {
        Self {
            session_id: secrets.session_id,
            session_salt: secrets.session_salt,
            cipher: ChaCha20Poly1305::new((&secrets.session_key).into()),
        }
    }

    pub fn seal(&self, packet: &AudioPacket) -> Result<Vec<u8>> {
        self.seal_payload(
            packet.sequence,
            packet.sample_timestamp,
            packet.flags,
            &packet.payload,
        )
    }

    pub fn seal_payload(
        &self,
        sequence: u64,
        sample_timestamp: u32,
        flags: u8,
        payload: &[u8],
    ) -> Result<Vec<u8>> {
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(&MAGIC);
        header.push(VERSION);
        header.push(flags);
        header.extend_from_slice(&(HEADER_LEN as u16).to_be_bytes());
        header.extend_from_slice(&self.session_id);
        header.extend_from_slice(&sequence.to_be_bytes());
        header.extend_from_slice(&sample_timestamp.to_be_bytes());
        let encrypted = self
            .cipher
            .encrypt(
                &self.nonce(sequence),
                Payload {
                    msg: payload,
                    aad: &header,
                },
            )
            .map_err(|_| AudioError::Authentication)?;
        header.extend_from_slice(&encrypted);
        if header.len() > MAX_DATAGRAM_BYTES {
            return Err(AudioError::Oversized);
        }
        Ok(header)
    }

    pub fn open(&self, datagram: &[u8]) -> Result<AudioPacket> {
        if datagram.len() < HEADER_LEN + 16 {
            return Err(AudioError::Truncated);
        }
        if datagram.len() > MAX_DATAGRAM_BYTES {
            return Err(AudioError::Oversized);
        }
        if datagram[..4] != MAGIC || datagram[4] != VERSION {
            return Err(AudioError::InvalidPacket);
        }
        let header_len = u16::from_be_bytes([datagram[6], datagram[7]]) as usize;
        if header_len != HEADER_LEN {
            return Err(AudioError::InvalidPacket);
        }
        if datagram[8..24] != self.session_id {
            return Err(AudioError::WrongSession);
        }
        let sequence = u64::from_be_bytes(datagram[24..32].try_into().unwrap());
        let sample_timestamp = u32::from_be_bytes(datagram[32..36].try_into().unwrap());
        let payload = self
            .cipher
            .decrypt(
                &self.nonce(sequence),
                Payload {
                    msg: &datagram[HEADER_LEN..],
                    aad: &datagram[..HEADER_LEN],
                },
            )
            .map_err(|_| AudioError::Authentication)?;
        Ok(AudioPacket {
            sequence,
            sample_timestamp,
            flags: datagram[5],
            payload,
        })
    }

    fn nonce(&self, sequence: u64) -> Nonce {
        let mut bytes = [0; 12];
        bytes[..4].copy_from_slice(&self.session_salt);
        bytes[4..].copy_from_slice(&sequence.to_be_bytes());
        bytes.into()
    }
}

pub struct PcmCodec;

impl PcmCodec {
    pub fn encode(pcm: &[f32]) -> Result<Vec<u8>> {
        if pcm.len() != SAMPLES_PER_FRAME {
            return Err(AudioError::InvalidPacket);
        }
        let mut output = Vec::with_capacity(PCM_BYTES_PER_FRAME);
        for sample in pcm {
            let value = (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
            output.extend_from_slice(&value.to_le_bytes());
        }
        Ok(output)
    }

    pub fn encode_f32le_into(input: &[u8], output: &mut Vec<u8>) -> Result<()> {
        if input.len() != SAMPLES_PER_FRAME * size_of::<f32>() {
            return Err(AudioError::InvalidPacket);
        }
        output.clear();
        output.reserve(PCM_BYTES_PER_FRAME.saturating_sub(output.capacity()));
        for bytes in input.chunks_exact(size_of::<f32>()) {
            let sample = f32::from_le_bytes(bytes.try_into().unwrap());
            let value = (sample.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
            output.extend_from_slice(&value.to_le_bytes());
        }
        Ok(())
    }

    pub fn decode(packet: Option<&[u8]>) -> Result<Vec<f32>> {
        let Some(packet) = packet else {
            return Ok(vec![0.0; SAMPLES_PER_FRAME]);
        };
        if packet.len() != PCM_BYTES_PER_FRAME {
            return Err(AudioError::InvalidPacket);
        }
        Ok(packet
            .chunks_exact(2)
            .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]) as f32 / i16::MAX as f32)
            .collect())
    }
}

#[derive(Debug)]
pub struct JitterBuffer {
    packets: BTreeMap<u64, AudioPacket>,
    next_sequence: Option<u64>,
    target_packets: usize,
    max_packets: usize,
    started: bool,
}

impl JitterBuffer {
    pub fn new(target_ms: u32) -> Self {
        let target_packets = ((target_ms.max(FRAME_MS as u32) / FRAME_MS as u32) as usize).max(1);
        Self {
            packets: BTreeMap::new(),
            next_sequence: None,
            target_packets,
            max_packets: (target_packets * 3).clamp(24, 96),
            started: false,
        }
    }

    pub fn push(&mut self, packet: AudioPacket) -> bool {
        if self.started {
            if self
                .next_sequence
                .is_some_and(|next| packet.sequence < next)
            {
                return false;
            }
        } else {
            self.next_sequence = Some(
                self.next_sequence
                    .map_or(packet.sequence, |next| next.min(packet.sequence)),
            );
        }
        self.next_sequence.get_or_insert(packet.sequence);
        let inserted = self.packets.insert(packet.sequence, packet).is_none();
        while self.packets.len() > self.max_packets {
            self.packets.pop_first();
            self.next_sequence = self
                .packets
                .first_key_value()
                .map(|(sequence, _)| *sequence);
        }
        inserted
    }

    pub fn pop_ready(&mut self) -> Option<Option<AudioPacket>> {
        let sequence = self.next_sequence?;
        let highest = *self.packets.last_key_value()?.0;
        let buffered_timeline = highest.saturating_sub(sequence) as usize + 1;
        if buffered_timeline < self.target_packets {
            return None;
        }
        self.started = true;
        self.next_sequence = Some(sequence.wrapping_add(1));
        Some(self.packets.remove(&sequence))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(sequence: u64) -> AudioPacket {
        AudioPacket {
            sequence,
            sample_timestamp: sequence as u32 * SAMPLES_PER_CHANNEL as u32,
            flags: 0,
            payload: vec![1, 2, 3],
        }
    }

    #[test]
    fn encrypted_packet_round_trip_and_tamper_rejection() {
        let secrets = SessionSecrets::generate();
        let cipher = PacketCipher::new(&secrets);
        let original = packet(42);
        let encrypted = cipher.seal(&original).unwrap();
        assert_eq!(cipher.open(&encrypted).unwrap(), original);
        let mut tampered = encrypted;
        *tampered.last_mut().unwrap() ^= 1;
        assert!(matches!(
            cipher.open(&tampered),
            Err(AudioError::Authentication)
        ));
    }

    #[test]
    fn wrong_session_is_rejected() {
        let a = PacketCipher::new(&SessionSecrets::generate());
        let b = PacketCipher::new(&SessionSecrets::generate());
        assert!(matches!(
            b.open(&a.seal(&packet(1)).unwrap()),
            Err(AudioError::WrongSession)
        ));
    }

    #[test]
    fn jitter_buffer_reorders_and_marks_loss() {
        let mut jitter = JitterBuffer::new(10);
        assert!(jitter.push(packet(10)));
        assert!(jitter.push(packet(12)));
        assert_eq!(jitter.pop_ready().unwrap().unwrap().sequence, 10);
        assert!(jitter.pop_ready().unwrap().is_none());
        assert!(jitter.pop_ready().is_none());
        assert!(jitter.push(packet(13)));
        assert_eq!(jitter.pop_ready().unwrap().unwrap().sequence, 12);
    }

    #[test]
    fn jitter_buffer_accepts_initial_out_of_order_packets() {
        let mut jitter = JitterBuffer::new(10);
        assert!(jitter.push(packet(11)));
        assert!(jitter.push(packet(10)));
        assert_eq!(jitter.pop_ready().unwrap().unwrap().sequence, 10);
        assert!(jitter.pop_ready().is_none());
        assert!(jitter.push(packet(12)));
        assert_eq!(jitter.pop_ready().unwrap().unwrap().sequence, 11);
    }

    #[test]
    fn pcm_stereo_round_trip() {
        let pcm: Vec<f32> = (0..SAMPLES_PER_FRAME)
            .map(|sample| ((sample as f32 / 24.0).sin()) * 0.25)
            .collect();
        let encoded = PcmCodec::encode(&pcm).unwrap();
        assert_eq!(encoded.len(), PCM_BYTES_PER_FRAME);
        let decoded = PcmCodec::decode(Some(&encoded)).unwrap();
        assert_eq!(decoded.len(), SAMPLES_PER_FRAME);
        for (expected, actual) in pcm.iter().zip(decoded) {
            assert!((expected - actual).abs() < 0.000_1);
        }
    }

    #[test]
    fn raw_f32le_encoder_matches_sample_encoder() {
        let pcm = (0..SAMPLES_PER_FRAME)
            .map(|index| (index as f32 / SAMPLES_PER_FRAME as f32) * 2.0 - 1.0)
            .collect::<Vec<_>>();
        let raw = pcm
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        let expected = PcmCodec::encode(&pcm).unwrap();
        let mut actual = Vec::new();
        PcmCodec::encode_f32le_into(&raw, &mut actual).unwrap();
        assert_eq!(actual, expected);
    }
}
