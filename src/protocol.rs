//! PostgreSQL wire protocol (v3) message framing helpers.
//!
//! Pure std. All integers are big-endian. Every message length is an Int32
//! that includes the 4 length bytes themselves. All strings are NUL-terminated.

use std::io::{self, Read, Write};
use std::net::TcpStream;

fn invalid<T>(msg: impl Into<String>) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidData, msg.into()))
}

/// Cursor over a received message payload.
pub struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Cursor { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> io::Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            return invalid("message truncated");
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn read_i16(&mut self) -> io::Result<i16> {
        let b = self.take(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    pub fn read_i32(&mut self) -> io::Result<i32> {
        let b = self.take(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn read_u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// Read exactly n raw bytes.
    pub fn read_bytes(&mut self, n: usize) -> io::Result<Vec<u8>> {
        Ok(self.take(n)?.to_vec())
    }

    /// Read a NUL-terminated UTF-8 string.
    pub fn read_cstring(&mut self) -> io::Result<String> {
        let end = self.buf[self.pos..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unterminated cstring"))?;
        let s = std::str::from_utf8(&self.buf[self.pos..self.pos + end])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid utf-8 in cstring"))?;
        let out = s.to_string();
        self.pos += end + 1;
        Ok(out)
    }
}

/// One message received from the frontend: type byte + payload.
pub struct FrontendMessage {
    pub typ: u8,
    pub payload: Vec<u8>,
}

fn read_i32_from(stream: &mut TcpStream) -> io::Result<i32> {
    let mut b = [0u8; 4];
    stream.read_exact(&mut b)?;
    Ok(i32::from_be_bytes(b))
}

/// Read the startup packet (no type byte): Int32 len, Int32 protocol, params.
/// Returns (protocol, raw parameter bytes).
pub fn read_startup(stream: &mut TcpStream) -> io::Result<(i32, Vec<u8>)> {
    let len = read_i32_from(stream)?;
    if len < 8 {
        return invalid(format!("bad startup packet length {}", len));
    }
    let mut buf = vec![0u8; (len - 4) as usize];
    stream.read_exact(&mut buf)?;
    let mut cur = Cursor::new(&buf);
    let proto = cur.read_i32()?;
    Ok((proto, buf))
}

/// Read one framed frontend message: Byte1 type, Int32 len, payload.
pub fn read_message(stream: &mut TcpStream) -> io::Result<FrontendMessage> {
    let mut typ = [0u8; 1];
    stream.read_exact(&mut typ)?;
    let len = read_i32_from(stream)?;
    if len < 4 {
        return invalid(format!("bad message length {}", len));
    }
    let mut payload = vec![0u8; (len - 4) as usize];
    stream.read_exact(&mut payload)?;
    Ok(FrontendMessage {
        typ: typ[0],
        payload,
    })
}

/// Builder for one backend message.
pub struct MsgBuilder {
    typ: u8,
    payload: Vec<u8>,
}

impl MsgBuilder {
    pub fn new(typ: u8) -> Self {
        MsgBuilder {
            typ,
            payload: Vec::new(),
        }
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.payload.push(v);
        self
    }

    pub fn i16(&mut self, v: i16) -> &mut Self {
        self.payload.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.payload.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn cstr(&mut self, s: &str) -> &mut Self {
        self.payload.extend_from_slice(s.as_bytes());
        self.payload.push(0);
        self
    }

    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.payload.extend_from_slice(b);
        self
    }

    /// Write type byte + Int32 length (including itself) + payload.
    /// Generic over the writer so the server can buffer through BufWriter.
    pub fn send(&self, stream: &mut impl Write) -> io::Result<()> {
        let len = (self.payload.len() + 4) as i32;
        stream.write_all(&[self.typ])?;
        stream.write_all(&len.to_be_bytes())?;
        stream.write_all(&self.payload)?;
        Ok(())
    }
}
