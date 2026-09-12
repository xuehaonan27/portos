//! Frame codec. 4-byte little-endian length prefix, then JSON payload over a
//! byte stream (UDS SOCK_STREAM). Max frame size guards against a misbehaving
//! peer. Frames carry control-plane JSON only; payload bytes ride after a
//! frame as a chunked byte stream (see [`crate::chunk`]) and never inside
//! JSON.
//!
//! Reading and writing are generic over the frame type, so callers name the
//! message they expect (see [`crate::wire`]) and a frame that is not that
//! message fails here rather than downstream. [`read_bytes`] is the escape
//! hatch for a caller that must measure or inspect the raw frame before
//! deciding how to parse it.

use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::{Read, Write};

pub const MAX_FRAME: u32 = 8 * 1024 * 1024; // 8 MiB of JSON is already absurd

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame io: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame too large: {0}")]
    TooLarge(u32),
    #[error("frame json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Write one frame. Returns the number of JSON bytes written, which is what
/// the context meter counts.
pub fn write_frame<W: Write, T: Serialize + ?Sized>(
    w: &mut W,
    payload: &T,
) -> Result<u64, FrameError> {
    let bytes = serde_json::to_vec(payload)?;
    write_bytes(w, &bytes)?;
    Ok(bytes.len() as u64)
}

/// Write an already-serialized frame body.
pub fn write_bytes<W: Write>(w: &mut W, bytes: &[u8]) -> Result<(), FrameError> {
    let len = bytes.len() as u32;
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    w.write_all(&len.to_le_bytes())?;
    w.write_all(bytes)?;
    w.flush()?;
    Ok(())
}

/// Read one frame's bytes without interpreting them.
pub fn read_bytes<R: Read>(r: &mut R) -> Result<Vec<u8>, FrameError> {
    let mut lenb = [0u8; 4];
    r.read_exact(&mut lenb)?;
    let len = u32::from_le_bytes(lenb);
    if len > MAX_FRAME {
        return Err(FrameError::TooLarge(len));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Read one frame and parse it as `T`.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<T, FrameError> {
    Ok(serde_json::from_slice(&read_bytes(r)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn roundtrip() {
        let v = serde_json::json!({"m": "hello", "n": 42});
        let mut buf = Vec::new();
        let n = write_frame(&mut buf, &v).unwrap();
        assert_eq!(n, serde_json::to_vec(&v).unwrap().len() as u64);
        assert_eq!(buf.len() as u64, n + 4); // length prefix
        let out: serde_json::Value = read_frame(&mut Cursor::new(buf)).unwrap();
        assert_eq!(v, out);
    }

    #[test]
    fn rejects_oversize_header() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_FRAME + 1).to_le_bytes());
        assert!(matches!(
            read_bytes(&mut Cursor::new(buf)),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[test]
    fn a_frame_of_the_wrong_shape_fails_at_the_read() {
        #[derive(serde::Deserialize)]
        struct Expected {
            #[allow(dead_code)]
            wanted: u32,
        }
        let mut buf = Vec::new();
        write_frame(&mut buf, &serde_json::json!({"other": 1})).unwrap();
        assert!(read_frame::<_, Expected>(&mut Cursor::new(buf)).is_err());
    }
}
