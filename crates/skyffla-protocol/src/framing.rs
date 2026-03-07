use std::fmt;
use std::io::{Read, Write};

use serde::de::DeserializeOwned;
use serde::Serialize;

pub const MAX_FRAME_SIZE: usize = 8 * 1024 * 1024;

#[derive(Debug)]
pub enum FrameError {
    Io(std::io::Error),
    Encode(serde_cbor::Error),
    Decode(serde_cbor::Error),
    FrameTooLarge { actual: usize, max: usize },
    InvalidLengthPrefix { declared: usize, actual: usize },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error while reading or writing frame: {error}"),
            Self::Encode(error) => write!(f, "failed to encode CBOR frame payload: {error}"),
            Self::Decode(error) => write!(f, "failed to decode CBOR frame payload: {error}"),
            Self::FrameTooLarge { actual, max } => {
                write!(f, "frame size {actual} exceeds maximum allowed size {max}")
            }
            Self::InvalidLengthPrefix { declared, actual } => write!(
                f,
                "frame length prefix declared {declared} payload bytes, but {actual} bytes were provided"
            ),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Encode(error) => Some(error),
            Self::Decode(error) => Some(error),
            Self::FrameTooLarge { .. } | Self::InvalidLengthPrefix { .. } => None,
        }
    }
}

impl From<std::io::Error> for FrameError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub fn encode_frame<T>(value: &T) -> Result<Vec<u8>, FrameError>
where
    T: Serialize,
{
    let payload = serde_cbor::to_vec(value).map_err(FrameError::Encode)?;
    if payload.len() > MAX_FRAME_SIZE {
        return Err(FrameError::FrameTooLarge {
            actual: payload.len(),
            max: MAX_FRAME_SIZE,
        });
    }

    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_frame<T>(frame: &[u8]) -> Result<T, FrameError>
where
    T: DeserializeOwned,
{
    if frame.len() < 4 {
        return Err(FrameError::InvalidLengthPrefix {
            declared: 0,
            actual: frame.len().saturating_sub(4),
        });
    }

    let declared = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
    if declared > MAX_FRAME_SIZE {
        return Err(FrameError::FrameTooLarge {
            actual: declared,
            max: MAX_FRAME_SIZE,
        });
    }

    let actual = frame.len() - 4;
    if declared != actual {
        return Err(FrameError::InvalidLengthPrefix { declared, actual });
    }

    serde_cbor::from_slice(&frame[4..]).map_err(FrameError::Decode)
}

pub fn write_frame<W, T>(writer: &mut W, value: &T) -> Result<usize, FrameError>
where
    W: Write,
    T: Serialize,
{
    let frame = encode_frame(value)?;
    writer.write_all(&frame)?;
    Ok(frame.len())
}

pub fn read_frame<R, T>(reader: &mut R) -> Result<T, FrameError>
where
    R: Read,
    T: DeserializeOwned,
{
    let mut prefix = [0_u8; 4];
    reader.read_exact(&mut prefix)?;

    let declared = u32::from_be_bytes(prefix) as usize;
    if declared > MAX_FRAME_SIZE {
        return Err(FrameError::FrameTooLarge {
            actual: declared,
            max: MAX_FRAME_SIZE,
        });
    }

    let mut payload = vec![0_u8; declared];
    reader.read_exact(&mut payload)?;
    serde_cbor::from_slice(&payload).map_err(FrameError::Decode)
}
