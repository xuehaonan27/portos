//! Chunked byte streams: the dereference transport of the data plane.
//!
//! A payload rides after a JSON frame as a sequence of raw chunks, each a
//! 4-byte little-endian length prefix followed by that many bytes, terminated
//! by a zero-length chunk. Payload bytes never enter a JSON frame (and never
//! the model context — that is the property that matters; see
//! decisions-v1.md D25). Same-machine fd/shm transports may return later as
//! an optimization behind the same Ref semantics; this stream is the portable
//! baseline every SDK can speak, including JS without native addons.

use std::io::{Read, Write};

/// Hard cap a reader enforces on any single chunk.
pub const CHUNK_MAX: u32 = 4 * 1024 * 1024;
/// Size writers aim for. Anything ≤ CHUNK_MAX is legal on the wire.
pub const CHUNK_SIZE: usize = 1024 * 1024;

#[derive(Debug)]
pub enum ChunkError {
    Io(std::io::Error),
    TooLarge(u32),
}

impl From<std::io::Error> for ChunkError {
    fn from(e: std::io::Error) -> Self {
        ChunkError::Io(e)
    }
}
impl std::fmt::Display for ChunkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChunkError::Io(e) => write!(f, "chunk io: {e}"),
            ChunkError::TooLarge(n) => write!(f, "chunk too large: {n}"),
        }
    }
}
impl std::error::Error for ChunkError {}

/// Write one chunk. Empty slices are illegal (the empty chunk is the
/// terminator); use [`finish`] to end the stream.
pub fn write_chunk<W: Write>(w: &mut W, bytes: &[u8]) -> Result<(), ChunkError> {
    debug_assert!(!bytes.is_empty(), "empty chunk is reserved for finish()");
    let len = bytes.len() as u32;
    if len > CHUNK_MAX {
        return Err(ChunkError::TooLarge(len));
    }
    w.write_all(&len.to_le_bytes())?;
    w.write_all(bytes)?;
    Ok(())
}

/// Terminate a chunk stream.
pub fn finish<W: Write>(w: &mut W) -> Result<(), ChunkError> {
    w.write_all(&0u32.to_le_bytes())?;
    w.flush()?;
    Ok(())
}

/// Read the next chunk; `None` means the terminator was reached.
pub fn read_chunk<R: Read>(r: &mut R) -> Result<Option<Vec<u8>>, ChunkError> {
    let mut lenb = [0u8; 4];
    r.read_exact(&mut lenb)?;
    let len = u32::from_le_bytes(lenb);
    if len == 0 {
        return Ok(None);
    }
    if len > CHUNK_MAX {
        return Err(ChunkError::TooLarge(len));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// Stream everything from `r` into chunks on `w` (terminator included).
/// Returns the payload byte count.
pub fn copy_into_chunks<R: Read, W: Write>(r: &mut R, w: &mut W) -> Result<u64, ChunkError> {
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut total: u64 = 0;
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        write_chunk(w, &buf[..n])?;
        total += n as u64;
    }
    finish(w)?;
    Ok(total)
}

/// Drain a chunk stream from `r` into `w` until the terminator.
/// Returns the payload byte count.
pub fn copy_from_chunks<R: Read, W: Write>(r: &mut R, w: &mut W) -> Result<u64, ChunkError> {
    let mut total: u64 = 0;
    while let Some(chunk) = read_chunk(r)? {
        w.write_all(&chunk)?;
        total += chunk.len() as u64;
    }
    Ok(total)
}

/// An adapter that reads a chunk stream as a plain `Read` (used for
/// streaming CAS ingest without buffering the whole payload).
pub struct ChunkReader<R: Read> {
    inner: R,
    buf: Vec<u8>,
    pos: usize,
    done: bool,
}

impl<R: Read> ChunkReader<R> {
    pub fn new(inner: R) -> Self {
        ChunkReader {
            inner,
            buf: Vec::new(),
            pos: 0,
            done: false,
        }
    }

    /// Consume any remaining chunks (protocol resync after an error path).
    pub fn drain(&mut self) -> Result<(), ChunkError> {
        while !self.done {
            match read_chunk(&mut self.inner)? {
                Some(_) => {}
                None => self.done = true,
            }
        }
        Ok(())
    }
}

impl<R: Read> Read for ChunkReader<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.buf.len() {
            if self.done {
                return Ok(0);
            }
            match read_chunk(&mut self.inner) {
                Ok(Some(chunk)) => {
                    self.buf = chunk;
                    self.pos = 0;
                }
                Ok(None) => {
                    self.done = true;
                    return Ok(0);
                }
                Err(ChunkError::Io(e)) => return Err(e),
                Err(e) => return Err(std::io::Error::other(e.to_string())),
            }
        }
        let n = (self.buf.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn roundtrip_and_terminator() {
        let payload = vec![7u8; 3 * CHUNK_SIZE + 123];
        let mut wire = Vec::new();
        copy_into_chunks(&mut Cursor::new(&payload), &mut wire).unwrap();
        let mut out = Vec::new();
        let n = copy_from_chunks(&mut Cursor::new(&wire), &mut out).unwrap();
        assert_eq!(n, payload.len() as u64);
        assert_eq!(out, payload);
    }

    #[test]
    fn chunk_reader_streams_without_buffering_all() {
        let payload: Vec<u8> = (0..100_000u32).flat_map(|i| i.to_le_bytes()).collect();
        let mut wire = Vec::new();
        copy_into_chunks(&mut Cursor::new(&payload), &mut wire).unwrap();
        let mut rd = ChunkReader::new(Cursor::new(&wire));
        let mut out = Vec::new();
        std::io::copy(&mut rd, &mut out).unwrap();
        assert_eq!(out, payload);
    }

    #[test]
    fn rejects_oversize_chunk() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&(CHUNK_MAX + 1).to_le_bytes());
        assert!(matches!(
            read_chunk(&mut Cursor::new(wire)),
            Err(ChunkError::TooLarge(_))
        ));
    }
}
