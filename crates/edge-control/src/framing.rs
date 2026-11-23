//! Length-prefixed JSON frames: a u32 big-endian byte count, then that many
//! bytes of JSON. Frames over [`MAX_FRAME`] are refused before any of the
//! body is read, so a confused or hostile peer can't make either side
//! allocate without bound.

use std::io::{Read, Write};

use serde::{Serialize, de::DeserializeOwned};

use crate::protocol::{PREAMBLE, PROTOCOL_VERSION};

/// Large enough for a snapshot or a settings view by a wide margin. Video
/// never travels over this channel.
pub const MAX_FRAME: usize = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("connection closed")]
    Closed,
    #[error("frame of {0} bytes is over the {MAX_FRAME} byte limit")]
    TooLarge(usize),
    #[error("not a control client (bad preamble)")]
    BadPreamble,
    #[error("malformed frame: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error(transparent)]
    Io(std::io::Error),
}

impl From<std::io::Error> for FrameError {
    fn from(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            Self::Closed
        } else {
            Self::Io(error)
        }
    }
}

pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, FrameError> {
    let body = serde_json::to_vec(message)?;
    if body.len() > MAX_FRAME {
        return Err(FrameError::TooLarge(body.len()));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

fn checked_len(header: [u8; 4]) -> Result<usize, FrameError> {
    let len = u32::from_be_bytes(header) as usize;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    Ok(len)
}

/// The bytes a client writes first.
pub fn preamble(version: u32) -> [u8; 8] {
    let mut bytes = [0; 8];
    bytes[..4].copy_from_slice(&PREAMBLE);
    bytes[4..].copy_from_slice(&version.to_be_bytes());
    bytes
}

/// Checks the seven bytes after a leading NUL and returns the client's
/// protocol version.
pub fn parse_preamble_rest(rest: [u8; 7]) -> Result<u32, FrameError> {
    if rest[..3] != PREAMBLE[1..] {
        return Err(FrameError::BadPreamble);
    }
    Ok(u32::from_be_bytes([rest[3], rest[4], rest[5], rest[6]]))
}

/// Whether a server speaking [`PROTOCOL_VERSION`] can serve this client.
pub fn supports(client_version: u32) -> bool {
    client_version == PROTOCOL_VERSION
}

pub fn write_frame<T: Serialize>(writer: &mut impl Write, message: &T) -> Result<(), FrameError> {
    writer.write_all(&encode(message)?)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T, FrameError> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let mut body = vec![0; checked_len(header)?];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(feature = "tokio")]
mod async_io {
    use serde::{Serialize, de::DeserializeOwned};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    use super::{FrameError, checked_len, encode};

    pub async fn write_frame_async<T: Serialize>(
        writer: &mut (impl AsyncWrite + Unpin),
        message: &T,
    ) -> Result<(), FrameError> {
        writer.write_all(&encode(message)?).await?;
        writer.flush().await?;
        Ok(())
    }

    pub async fn read_frame_async<T: DeserializeOwned>(
        reader: &mut (impl AsyncRead + Unpin),
    ) -> Result<T, FrameError> {
        let mut header = [0; 4];
        reader.read_exact(&mut header).await?;
        let mut body = vec![0; checked_len(header)?];
        reader.read_exact(&mut body).await?;
        Ok(serde_json::from_slice(&body)?)
    }
}

#[cfg(feature = "tokio")]
pub use async_io::{read_frame_async, write_frame_async};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ClientMessage, Request};

    #[test]
    fn frames_round_trip() {
        let message = ClientMessage {
            id: 9,
            request: Request::UnpairPhone {
                name: "CMF Phone 2 Pro".into(),
            },
        };
        let bytes = encode(&message).unwrap();
        let decoded: ClientMessage = read_frame(&mut bytes.as_slice()).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn oversized_headers_are_refused_before_reading_the_body() {
        let mut bytes = ((MAX_FRAME + 1) as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(b"{}");
        let error = read_frame::<ClientMessage>(&mut bytes.as_slice()).unwrap_err();
        assert!(matches!(error, FrameError::TooLarge(_)));
    }

    #[test]
    fn truncated_frames_read_as_closed() {
        let mut bytes = encode(&ClientMessage {
            id: 1,
            request: Request::Snapshot,
        })
        .unwrap();
        bytes.truncate(bytes.len() - 2);
        assert!(matches!(
            read_frame::<ClientMessage>(&mut bytes.as_slice()),
            Err(FrameError::Closed)
        ));
    }

    #[test]
    fn malformed_json_is_reported() {
        let mut bytes = 3u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"{x}");
        assert!(matches!(
            read_frame::<ClientMessage>(&mut bytes.as_slice()),
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn preambles_carry_the_version() {
        let bytes = preamble(PROTOCOL_VERSION);
        assert_eq!(bytes[0], 0);
        let rest: [u8; 7] = bytes[1..].try_into().unwrap();
        assert_eq!(parse_preamble_rest(rest).unwrap(), PROTOCOL_VERSION);
        assert!(supports(PROTOCOL_VERSION));
        assert!(!supports(PROTOCOL_VERSION + 1));
        let mut bad = rest;
        bad[0] = b'X';
        assert!(matches!(
            parse_preamble_rest(bad),
            Err(FrameError::BadPreamble)
        ));
    }
}
