//! Listening-socket setup (v0.4).
//!
//! The server must be restartable at will — the crash-recovery story is
//! literally "kill -9, restart, verify" — so the listening socket sets
//! `SO_REUSEADDR`: gracefully closed connections lingering in `TIME_WAIT`
//! must not block rebinding 127.0.0.1:5433 for 60 seconds.
//!
//! Pure `std::net` exposes no `setsockopt`, and the project is
//! zero-dependency, so on Linux/x86_64 the socket is built with raw
//! syscalls via inline `asm!` (`socket`/`setsockopt`/`bind`/`listen`)
//! and wrapped with `TcpListener::from_raw_fd`. Other platforms fall
//! back to `TcpListener::bind` without `SO_REUSEADDR`.

use std::net::{SocketAddr, TcpListener};

/// Bind a listening TCP socket on `addr` with `SO_REUSEADDR` set (where
/// the platform allows it).
pub fn bind_listen(addr: SocketAddr) -> std::io::Result<TcpListener> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        return bind_reuseaddr_linux(addr);
    }
    #[allow(unreachable_code)]
    TcpListener::bind(addr)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn bind_reuseaddr_linux(addr: SocketAddr) -> std::io::Result<TcpListener> {
    use std::os::unix::io::FromRawFd;

    const AF_INET: u64 = 2;
    const SOCK_STREAM: u64 = 1;
    const IPPROTO_TCP: u64 = 6;
    const SOL_SOCKET: u64 = 1;
    const SO_REUSEADDR: u64 = 2;
    const NR_SOCKET: u64 = 41;
    const NR_SETSOCKOPT: u64 = 54;
    const NR_BIND: u64 = 49;
    const NR_LISTEN: u64 = 50;
    const NR_CLOSE: u64 = 3;

    // v0.4 only listens on IPv4 loopback; anything else takes the plain
    // std path (no SO_REUSEADDR there).
    //
    // Byte order: sin_addr/sin_port are big-endian on the wire, so we
    // want their *memory* bytes in network order. from_ne_bytes gives
    // exactly the u32 whose in-memory representation is the octet
    // sequence — no further swap (swapping here produced 1.0.0.127 and
    // EADDRNOTAVAIL).
    let (ip_bytes, port): (u32, u16) = match addr {
        SocketAddr::V4(v4) => (u32::from_ne_bytes(v4.ip().octets()), v4.port()),
        _ => return TcpListener::bind(addr),
    };

    /// Raw 6-argument syscall. Returns the kernel return value (a
    /// negative errno on error); clobbers rcx/r11 per the syscall ABI.
    unsafe fn syscall6(n: u64, a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> i64 {
        let ret: i64;
        // (edition 2024: unsafe ops inside `unsafe fn` still need a block)
        unsafe {
            std::arch::asm!(
                "syscall",
                in("rax") n,
                in("rdi") a,
                in("rsi") b,
                in("rdx") c,
                in("r10") d,
                in("r8") e,
                in("r9") f,
                lateout("rax") ret,
                lateout("rcx") _,
                lateout("r11") _,
                options(nostack),
            );
        }
        ret
    }

    unsafe fn close_fd(fd: i64) {
        unsafe {
            syscall6(NR_CLOSE, fd as u64, 0, 0, 0, 0, 0);
        }
    }

    // socket(AF_INET, SOCK_STREAM, IPPROTO_TCP)
    let fd = unsafe { syscall6(NR_SOCKET, AF_INET, SOCK_STREAM, IPPROTO_TCP, 0, 0, 0) };
    if fd < 0 {
        return Err(std::io::Error::from_raw_os_error(-fd as i32));
    }
    // setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one))
    let one: i32 = 1;
    let r = unsafe {
        syscall6(
            NR_SETSOCKOPT,
            fd as u64,
            SOL_SOCKET,
            SO_REUSEADDR,
            std::ptr::addr_of!(one) as u64,
            4,
            0,
        )
    };
    if r < 0 {
        let errno = -r as i32;
        unsafe { close_fd(fd) };
        return Err(std::io::Error::from_raw_os_error(errno));
    }
    // struct sockaddr_in { sin_family, sin_port, sin_addr, sin_zero[8] }
    #[repr(C)]
    struct SockAddrIn {
        sin_family: u16,
        sin_port: u16,
        sin_addr: u32,
        sin_zero: [u8; 8],
    }
    let sa = SockAddrIn {
        sin_family: AF_INET as u16,
        sin_port: port.to_be(),
        sin_addr: ip_bytes,
        sin_zero: [0; 8],
    };
    let r = unsafe {
        syscall6(
            NR_BIND,
            fd as u64,
            std::ptr::addr_of!(sa) as u64,
            16,
            0,
            0,
            0,
        )
    };
    if r < 0 {
        let errno = -r as i32;
        unsafe { close_fd(fd) };
        return Err(std::io::Error::from_raw_os_error(errno));
    }
    // listen(fd, backlog)
    let r = unsafe { syscall6(NR_LISTEN, fd as u64, 128, 0, 0, 0, 0) };
    if r < 0 {
        let errno = -r as i32;
        unsafe { close_fd(fd) };
        return Err(std::io::Error::from_raw_os_error(errno));
    }
    Ok(unsafe { TcpListener::from_raw_fd(fd as i32) })
}
