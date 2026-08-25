//! Frame codec. 4-byte little-endian length prefix, then JSON payload over a
//! byte stream (UDS SOCK_STREAM). Max frame size guards against a misbehaving
//! peer. fd attachments ride on a parallel SCM_RIGHTS message. A frame whose
//! JSON carries `"fds": n` is followed by exactly one ancillary message with
//! n descriptors.

use std::io::{Read, Write};

pub const MAX_FRAME: u32 = 8 * 1024 * 1024; // 8 MiB of JSON is already absurd

#[derive(Debug)]
pub enum FrameError {
    Io(std::io::Error),
    TooLarge(u32),
    Json(serde_json::Error),
}

impl From<std::io::Error> for FrameError {
    fn from(e: std::io::Error) -> Self {
        FrameError::Io(e)
    }
}
impl From<serde_json::Error> for FrameError {
    fn from(e: serde_json::Error) -> Self {
        FrameError::Json(e)
    }
}
impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Io(e) => write!(f, "frame io: {e}"),
            FrameError::TooLarge(n) => write!(f, "frame too large: {n}"),
            FrameError::Json(e) => write!(f, "frame json: {e}"),
        }
    }
}
impl std::error::Error for FrameError {}

pub fn write_frame<W: Write>(w: &mut W, payload: &serde_json::Value) -> Result<(), FrameError> {
    let bytes = serde_json::to_vec(payload)?;
    let len = bytes.len() as u32;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&bytes)?;
    w.flush()?;
    Ok(())
}

pub fn read_frame<R: Read>(r: &mut R) -> Result<serde_json::Value, FrameError> {
    let mut lenb = [0u8; 4];
    r.read_exact(&mut lenb)?;
    let len = u32::from_le_bytes(lenb);
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(serde_json::from_slice(&buf)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn roundtrip() {
        let v = serde_json::json!({"m": "hello", "n": 42});
        let mut buf = Vec::new();
        write_frame(&mut buf, &v).unwrap();
        let out = read_frame(&mut Cursor::new(buf)).unwrap();
        assert_eq!(v, out);
    }

    #[test]
    fn rejects_oversize_header() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_FRAME + 1).to_le_bytes());
        assert!(matches!(
            read_frame(&mut Cursor::new(buf)),
            Err(FrameError::TooLarge(_))
        ));
    }
}
