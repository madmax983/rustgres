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
#[derive(Debug)]
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
///
/// v0.12: hardened against allocation-DoS — the length field is untrusted
/// client input, so we cap the packet (16 MiB; real startup packets are a
/// few hundred bytes) and read incrementally, allocating only for bytes
/// that actually arrive. A lying length that never delivers can no longer
/// pin gigabytes. Oversize packets fail with ERRCODE_PROTOCOL_VIOLATION
/// (08P01), like PostgreSQL's "invalid message length".
pub fn read_startup(stream: &mut TcpStream) -> io::Result<(i32, Vec<u8>)> {
    let len = read_i32_from(stream)?;
    if len < 8 {
        return invalid(format!("bad startup packet length {}", len));
    }
    let buf = read_bounded(stream, (len - 4) as i64, MAX_STARTUP_BYTES, "startup packet")?;
    let mut cur = Cursor::new(&buf);
    let proto = cur.read_i32()?;
    Ok((proto, buf))
}

/// Read one framed frontend message: Byte1 type, Int32 len, payload.
///
/// v0.12: same hardening as read_startup — 1 GiB cap (PostgreSQL's own
/// maximum message size) and incremental allocation.
pub fn read_message(stream: &mut TcpStream) -> io::Result<FrontendMessage> {
    let mut typ = [0u8; 1];
    stream.read_exact(&mut typ)?;
    let len = read_i32_from(stream)?;
    if len < 4 {
        return invalid(format!("bad message length {}", len));
    }
    let payload = read_bounded(stream, (len - 4) as i64, MAX_MESSAGE_BYTES, "message")?;
    Ok(FrontendMessage {
        typ: typ[0],
        payload,
    })
}

/// v0.12: maximum inbound frontend message size — 1 GiB, matching
/// PostgreSQL's documented maximum message size.
pub const MAX_MESSAGE_BYTES: usize = 1 << 30;

/// v0.12: maximum startup packet size — 16 MiB (real ones are < 1 KiB).
pub const MAX_STARTUP_BYTES: usize = 16 << 20;

/// Read `total` bytes from `stream` in 64 KiB chunks, failing if `total`
/// exceeds `cap`. Incremental: memory grows only with bytes actually
/// received, never with the claimed length alone.
fn read_bounded(
    stream: &mut TcpStream,
    total: i64,
    cap: usize,
    what: &str,
) -> io::Result<Vec<u8>> {
    if total < 0 || total as u64 > cap as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            // 08P01 surfaces in the fuzzer tests; the io error kind is what
            // callers see, the message carries the SQLSTATE context.
            format!("{} length {} exceeds maximum {} bytes (08P01)", what, total, cap),
        ));
    }
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    let mut remaining = total as usize;
    while remaining > 0 {
        let n = remaining.min(chunk.len());
        stream.read_exact(&mut chunk[..n])?;
        out.extend_from_slice(&chunk[..n]);
        remaining -= n;
    }
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    /// Feed a header claiming a 2 GiB payload, then deliver *nothing*.
    /// read_message must reject the lie without allocating 2 GiB.
    #[test]
    fn oversize_message_rejected_without_allocation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        // type 'Q' + length claiming ~2 GiB payload (i32::MAX).
        client.write_all(&[b'Q']).unwrap();
        client.write_all(&i32::MAX.to_be_bytes()).unwrap();
        client.flush().unwrap();
        let err = read_message(&mut server).unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("exceeds maximum"),
            "unexpected error: {}",
            msg
        );
    }

    /// Same for the startup packet: claim 1 GiB, deliver nothing.
    #[test]
    fn oversize_startup_rejected_without_allocation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        client.write_all(&i32::MAX.to_be_bytes()).unwrap();
        client.flush().unwrap();
        let err = read_startup(&mut server).unwrap_err();
        assert!(format!("{}", err).contains("exceeds maximum"));
    }

    /// A truncated-but-honest message fails cleanly instead of hanging.
    #[test]
    fn truncated_message_is_io_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        server
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        client.write_all(&[b'Q']).unwrap();
        client.write_all(&104i32.to_be_bytes()).unwrap(); // claims 100 payload bytes
        client.write_all(b"short").unwrap();
        drop(client); // EOF mid-message
        let err = read_message(&mut server).unwrap_err();
        assert!(
            err.kind() == io::ErrorKind::UnexpectedEof
                || format!("{}", err).contains("failed to fill whole buffer"),
            "unexpected error: {} ({:?})",
            err,
            err.kind()
        );
    }
}
