//! Raw socket operations shared by the client and server loops.

use std::io;
use std::net::{SocketAddr, UdpSocket};

#[cfg(unix)]
pub type RawSock = std::os::fd::RawFd;
#[cfg(windows)]
pub type RawSock = std::os::windows::io::RawSocket;

/// The raw socket backing a socket type.
///
/// This is `AsRawFd` on Unix and `AsRawSocket` on Windows; naming it once keeps
/// the generic bounds in the I/O pumps platform-independent.
pub trait AsRawSock {
    fn as_raw_sock(&self) -> RawSock;
}

#[cfg(unix)]
impl<T: std::os::fd::AsRawFd> AsRawSock for T {
    fn as_raw_sock(&self) -> RawSock {
        self.as_raw_fd()
    }
}

#[cfg(windows)]
impl<T: std::os::windows::io::AsRawSocket> AsRawSock for T {
    fn as_raw_sock(&self) -> RawSock {
        self.as_raw_socket()
    }
}

/// A socket the receive buffers can read from directly.
///
/// The point of reading through a raw pointer is to land bytes in a `Vec`'s
/// uninitialised spare capacity without zeroing it first. How that read is
/// issued has to differ by platform, and not only for spelling:
///
/// On Unix a plain `read(2)` on the descriptor is both fastest and invisible to
/// the poller, because `epoll`/`kqueue` readiness is a property of the socket
/// rather than of the reader.
///
/// On Windows `mio` has no edge-triggered primitive to build on, so it
/// simulates one: after it reports a readiness event it clears its interest for
/// that socket, and re-arms only when a read or write *through one of its own
/// types* reports `WouldBlock`. A raw `recv` never tells `mio` anything, so the
/// socket would report readable exactly once and then stay silent forever. The
/// Windows implementation therefore reads through the `Read` impl, which
/// requires an initialised buffer, so it zeroes the region first. That memset
/// is the price of keeping `mio`'s readiness correct, and it is small next to
/// the syscall it accompanies.
pub trait SocketSource {
    /// Reads up to `len` bytes into `ptr`, returning the count. `Ok(0)` means
    /// the peer closed the connection.
    ///
    /// # Safety
    ///
    /// `ptr` must be valid for writes of `len` bytes.
    unsafe fn recv_into(&self, ptr: *mut u8, len: usize) -> io::Result<usize>;
}

#[cfg(unix)]
impl<T: AsRawSock + ?Sized> SocketSource for T {
    unsafe fn recv_into(&self, ptr: *mut u8, len: usize) -> io::Result<usize> {
        unsafe { recv_into(self.as_raw_sock(), ptr, len) }
    }
}

#[cfg(windows)]
impl<T: ?Sized> SocketSource for T
where
    for<'a> &'a T: io::Read,
{
    unsafe fn recv_into(&self, ptr: *mut u8, len: usize) -> io::Result<usize> {
        use std::io::Read;

        // SAFETY: the caller guarantees `ptr` is valid for `len` bytes, which
        // makes the region sound to initialise and then borrow as a slice.
        let buf = unsafe {
            std::ptr::write_bytes(ptr, 0, len);
            std::slice::from_raw_parts_mut(ptr, len)
        };
        let mut source = self;
        source.read(buf)
    }
}

/// Reads up to `len` bytes from `socket` into `ptr` with `read(2)`, which works
/// on descriptors and so is invisible to `epoll`/`kqueue`.
///
/// There is deliberately no Windows counterpart. Winsock's equivalent is a raw
/// `recv`, which is exactly the call that leaves `mio`'s simulated
/// edge-triggering un-armed, the reason [`SocketSource`] exists at all. Naming
/// it here would put the unsafe spelling next to the safe one with better
/// advertising.
///
/// # Safety
///
/// `ptr` must be valid for writes of `len` bytes.
#[cfg(unix)]
unsafe fn recv_into(socket: RawSock, ptr: *mut u8, len: usize) -> io::Result<usize> {
    // SAFETY: the caller guarantees `ptr` is valid for `len` bytes.
    let read = unsafe { libc::read(socket, ptr.cast(), len) };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(read as usize)
}

/// Whether an error means the syscall was interrupted and should be retried.
///
/// Winsock has no `EINTR` for these operations, so on Windows only the standard
/// kind can occur; on Unix the raw code is checked too, because a raw
/// `libc` call reports `EINTR` without going through `io::ErrorKind`.
pub fn is_interrupted(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::Interrupted {
        return true;
    }
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::EINTR)
    }
    #[cfg(windows)]
    {
        false
    }
}

/// Whether a UDP receive error describes one datagram rather than the socket.
///
/// An ICMP port-unreachable drawn by an earlier send surfaces on the receive
/// that follows it as `ConnectionRefused` (Linux, connected socket) or
/// `ConnectionReset` (Windows, `WSAECONNRESET`), and a datagram larger than the
/// buffer is `WSAEMSGSIZE` on Windows. All of them are remotely triggerable and
/// say nothing about the next datagram, so a receive loop skips them; any other
/// error is a fault of the socket itself.
pub fn is_datagram_error(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionRefused
    ) {
        return true;
    }
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::EMSGSIZE)
    }
    #[cfg(windows)]
    {
        error.raw_os_error() == Some(windows_sys::Win32::Networking::WinSock::WSAEMSGSIZE)
    }
}

/// The protocol level an option applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SockLevel {
    Ip,
    Ipv6,
    Socket,
}

#[cfg(unix)]
fn level_value(level: SockLevel) -> libc::c_int {
    match level {
        SockLevel::Ip => libc::IPPROTO_IP,
        SockLevel::Ipv6 => libc::IPPROTO_IPV6,
        SockLevel::Socket => libc::SOL_SOCKET,
    }
}

#[cfg(windows)]
fn level_value(level: SockLevel) -> i32 {
    use windows_sys::Win32::Networking::WinSock;
    match level {
        SockLevel::Ip => WinSock::IPPROTO_IP,
        SockLevel::Ipv6 => WinSock::IPPROTO_IPV6,
        SockLevel::Socket => WinSock::SOL_SOCKET,
    }
}

/// Sets an integer-valued socket option.
pub fn setsockopt_int(
    socket: RawSock,
    level: SockLevel,
    option: i32,
    value: i32,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        let result = unsafe {
            libc::setsockopt(
                socket,
                level_value(level),
                option,
                &value as *const _ as *const libc::c_void,
                std::mem::size_of_val(&value) as libc::socklen_t,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Networking::WinSock;
        let result = unsafe {
            WinSock::setsockopt(
                socket as WinSock::SOCKET,
                level_value(level),
                option,
                &value as *const _ as *const u8,
                std::mem::size_of_val(&value) as i32,
            )
        };
        if result == WinSock::SOCKET_ERROR {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Reads an integer-valued socket option.
pub fn getsockopt_int(socket: RawSock, level: SockLevel, option: i32) -> io::Result<i32> {
    let mut value: i32 = 0;
    #[cfg(unix)]
    {
        let mut len = std::mem::size_of_val(&value) as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                socket,
                level_value(level),
                option,
                &mut value as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(value)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Networking::WinSock;
        let mut len = std::mem::size_of_val(&value) as i32;
        let result = unsafe {
            WinSock::getsockopt(
                socket as WinSock::SOCKET,
                level_value(level),
                option,
                &mut value as *mut _ as *mut u8,
                &mut len,
            )
        };
        if result == WinSock::SOCKET_ERROR {
            return Err(io::Error::last_os_error());
        }
        Ok(value)
    }
}

/// Lets this socket bind an address another socket already holds.
///
/// Both stacks want it set before the bind, which is why it cannot be applied
/// to a socket `std` has already bound and why [`bind_udp_socket_with`] exists.
pub fn set_reuse_addr(socket: RawSock, reuse: bool) -> io::Result<()> {
    #[cfg(unix)]
    let option = libc::SO_REUSEADDR;
    #[cfg(windows)]
    let option = windows_sys::Win32::Networking::WinSock::SO_REUSEADDR;
    setsockopt_int(socket, SockLevel::Socket, option, i32::from(reuse))
}

/// Claims this socket's address exclusively, so no other socket can bind it
/// while this one holds it.
///
/// Windows only. There, `SO_REUSEADDR` lets any later socket bind the same
/// port and take over its traffic, regardless of who owns the first one, so a
/// socket that does not need to share its address should say so with
/// `SO_EXCLUSIVEADDRUSE`. Unix needs no counterpart: a port bound without
/// `SO_REUSEADDR`/`SO_REUSEPORT` cannot be bound again, and a reuse-port group
/// is limited to the same effective user. Set before the bind.
#[cfg(windows)]
pub fn set_exclusive_addr_use(socket: RawSock) -> io::Result<()> {
    setsockopt_int(
        socket,
        SockLevel::Socket,
        windows_sys::Win32::Networking::WinSock::SO_EXCLUSIVEADDRUSE,
        1,
    )
}

/// Stops a UDP socket reporting ICMP delivery failures on its receive path.
///
/// A datagram that draws an ICMP port-unreachable says something about the
/// destination, not about this socket. Windows surfaces it anyway, as
/// `WSAECONNRESET` from the *next* receive, unless `SIO_UDP_CONNRESET` is
/// cleared. Linux already reports nothing on an unconnected socket while
/// `IP_RECVERR` is off, which is its default; clearing it here only undoes an
/// option something else set. A connected Linux socket reports
/// `ECONNREFUSED` regardless of `IP_RECVERR`, so receive loops still skip such
/// errors through [`is_datagram_error`].
///
/// Best-effort by design: a platform with no such reporting to disable, and a
/// per-family option the socket's family does not accept, both count as
/// success.
pub fn ignore_udp_icmp_errors(socket: RawSock) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // One of these two is always for the wrong family; whichever it is
        // fails harmlessly.
        let _ = setsockopt_int(socket, SockLevel::Ip, libc::IP_RECVERR, 0);
        let _ = setsockopt_int(socket, SockLevel::Ipv6, libc::IPV6_RECVERR, 0);
        Ok(())
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
    {
        let _ = socket;
        Ok(())
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Networking::WinSock;

        let disable: i32 = 0;
        let mut returned: u32 = 0;
        // SAFETY: the input buffer outlives the call, its length is passed
        // explicitly, and the null overlapped/completion pair requests the
        // blocking form, which this ioctl always completes inline.
        let result = unsafe {
            WinSock::WSAIoctl(
                socket as WinSock::SOCKET,
                WinSock::SIO_UDP_CONNRESET,
                std::ptr::from_ref(&disable).cast(),
                size_of_val(&disable) as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
                None,
            )
        };
        if result == WinSock::SOCKET_ERROR {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Creates a UDP socket for `addr`'s family, hands its raw handle to
/// `configure`, then binds it to `addr`.
///
/// `std::net::UdpSocket::bind` creates and binds in one call, which leaves no
/// point at which to set the options that only take effect before a bind.
/// `SO_REUSEADDR` matters most, since without it a second process cannot share
/// the mDNS port. The socket is owned from the moment it exists, so a failure in
/// `configure` or in the bind closes it.
pub fn bind_udp_socket_with(
    addr: SocketAddr,
    configure: impl FnOnce(RawSock) -> io::Result<()>,
) -> io::Result<UdpSocket> {
    let socket = create_udp_socket(addr)?;
    configure(socket.as_raw_sock())?;
    bind_socket(socket.as_raw_sock(), addr)?;
    Ok(socket)
}

#[cfg(unix)]
fn create_udp_socket(addr: SocketAddr) -> io::Result<UdpSocket> {
    use std::os::fd::FromRawFd;

    let domain = if addr.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    // SAFETY: a plain socket creation; the descriptor is adopted below.
    let fd = unsafe { libc::socket(domain, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    // SAFETY: as above. `SOCK_CLOEXEC` is not portable, so the flag is set
    // separately; the window between the two is why Linux gets the atomic form.
    let fd = unsafe {
        let fd = libc::socket(domain, libc::SOCK_DGRAM, 0);
        if fd >= 0 && libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
            let error = io::Error::last_os_error();
            libc::close(fd);
            return Err(error);
        }
        fd
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor was just created and is owned by nothing else.
    Ok(unsafe { UdpSocket::from_raw_fd(fd) })
}

#[cfg(windows)]
fn create_udp_socket(addr: SocketAddr) -> io::Result<UdpSocket> {
    use std::os::windows::io::FromRawSocket;
    use windows_sys::Win32::Networking::WinSock;

    init_winsock();
    let domain = if addr.is_ipv4() {
        WinSock::AF_INET
    } else {
        WinSock::AF_INET6
    };
    // The flags `std` itself uses: overlapped so `mio`'s AFD poller can drive
    // the socket, and non-inheritable, the `FD_CLOEXEC` equivalent.
    // SAFETY: the optional protocol-info and group arguments are null/zero,
    // which the API accepts.
    let socket = unsafe {
        WinSock::WSASocketW(
            i32::from(domain),
            WinSock::SOCK_DGRAM,
            WinSock::IPPROTO_UDP,
            std::ptr::null(),
            0,
            WinSock::WSA_FLAG_OVERLAPPED | WinSock::WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if socket == WinSock::INVALID_SOCKET {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the socket was just created and is owned by nothing else.
    Ok(unsafe { UdpSocket::from_raw_socket(socket as std::os::windows::io::RawSocket) })
}

/// Starts Winsock for sockets this crate creates itself.
///
/// `std` does this lazily inside its own socket constructors, so a raw
/// `WSASocketW` reached before any `std` socket exists would fail with
/// `WSANOTINITIALISED`. `WSAStartup` is reference-counted, so calling it a
/// second time is harmless; the matching `WSACleanup` is deliberately omitted,
/// exactly as `std` omits it, because process exit does the same work faster.
#[cfg(windows)]
fn init_winsock() {
    use std::sync::Once;
    use windows_sys::Win32::Networking::WinSock;

    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        // SAFETY: the output struct is fully written by the call.
        unsafe {
            let mut data = std::mem::zeroed();
            WinSock::WSAStartup(0x202, &mut data);
        }
    });
}

#[cfg(unix)]
fn bind_socket(socket: RawSock, addr: SocketAddr) -> io::Result<()> {
    let result = match addr {
        SocketAddr::V4(addr) => {
            let raw = libc::sockaddr_in {
                #[cfg(any(target_os = "macos", target_os = "ios"))]
                sin_len: size_of::<libc::sockaddr_in>() as u8,
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: addr.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from(*addr.ip()).to_be(),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: the address outlives the call and its length is exact.
            unsafe {
                libc::bind(
                    socket,
                    std::ptr::from_ref(&raw).cast(),
                    size_of_val(&raw) as libc::socklen_t,
                )
            }
        }
        SocketAddr::V6(addr) => {
            let raw = libc::sockaddr_in6 {
                #[cfg(any(target_os = "macos", target_os = "ios"))]
                sin6_len: size_of::<libc::sockaddr_in6>() as u8,
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: addr.port().to_be(),
                sin6_flowinfo: addr.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: addr.ip().octets(),
                },
                sin6_scope_id: addr.scope_id(),
            };
            // SAFETY: as above.
            unsafe {
                libc::bind(
                    socket,
                    std::ptr::from_ref(&raw).cast(),
                    size_of_val(&raw) as libc::socklen_t,
                )
            }
        }
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn bind_socket(socket: RawSock, addr: SocketAddr) -> io::Result<()> {
    use windows_sys::Win32::Networking::WinSock;

    let result = match addr {
        SocketAddr::V4(addr) => {
            let raw = WinSock::SOCKADDR_IN {
                sin_family: WinSock::AF_INET,
                sin_port: addr.port().to_be(),
                sin_addr: WinSock::IN_ADDR {
                    S_un: WinSock::IN_ADDR_0 {
                        S_addr: u32::from(*addr.ip()).to_be(),
                    },
                },
                sin_zero: [0; 8],
            };
            // SAFETY: the address outlives the call and its length is exact.
            unsafe {
                WinSock::bind(
                    socket as WinSock::SOCKET,
                    std::ptr::from_ref(&raw).cast(),
                    size_of_val(&raw) as i32,
                )
            }
        }
        SocketAddr::V6(addr) => {
            let raw = WinSock::SOCKADDR_IN6 {
                sin6_family: WinSock::AF_INET6,
                sin6_port: addr.port().to_be(),
                sin6_flowinfo: addr.flowinfo(),
                sin6_addr: WinSock::IN6_ADDR {
                    u: WinSock::IN6_ADDR_0 {
                        Byte: addr.ip().octets(),
                    },
                },
                Anonymous: WinSock::SOCKADDR_IN6_0 {
                    sin6_scope_id: addr.scope_id(),
                },
            };
            // SAFETY: as above.
            unsafe {
                WinSock::bind(
                    socket as WinSock::SOCKET,
                    std::ptr::from_ref(&raw).cast(),
                    size_of_val(&raw) as i32,
                )
            }
        }
    };
    if result == WinSock::SOCKET_ERROR {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The IP-level option carrying the DSCP/traffic-class byte, per address family.
pub fn dscp_option(local_addr: SocketAddr) -> (SockLevel, i32) {
    #[cfg(unix)]
    {
        if local_addr.is_ipv4() {
            (SockLevel::Ip, libc::IP_TOS)
        } else {
            (SockLevel::Ipv6, libc::IPV6_TCLASS)
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Networking::WinSock;
        if local_addr.is_ipv4() {
            (SockLevel::Ip, WinSock::IP_TOS as i32)
        } else {
            (SockLevel::Ipv6, WinSock::IPV6_TCLASS as i32)
        }
    }
}

/// Requests a DSCP class for outbound packets on `socket`.
///
/// `dscp` is the 6-bit code point; it is shifted into the high bits of the
/// traffic-class byte here.
///
/// This is advisory on every platform, since routers may ignore or rewrite the
/// bits, and on Windows it is advisory twice over: since XP SP2 the stack accepts
/// `IP_TOS` and then drops it on the floor unless the machine opts back in
/// through the `DisableUserTOSSetting` policy, so a success here does not
/// guarantee marked packets. Callers already treat this as best-effort.
pub fn apply_dscp(socket: RawSock, local_addr: SocketAddr, dscp: u8) -> io::Result<()> {
    let (level, option) = dscp_option(local_addr);
    setsockopt_int(socket, level, option, i32::from(dscp) << 2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{TcpListener, TcpStream};

    /// A connected loopback pair, so `recv_into` can be exercised against a
    /// real socket on every platform.
    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().expect("loopback addr");
        let client = TcpStream::connect(addr).expect("connect loopback");
        let (server, _) = listener.accept().expect("accept loopback");
        (client, server)
    }

    /// Reads through the trait rather than the Unix free function, so these
    /// run on Windows too, where they are what would catch a regression in the
    /// simulated-edge-trigger handling.
    fn read_some(socket: &TcpStream, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `buf` is valid for its own length.
        unsafe { socket.recv_into(buf.as_mut_ptr(), buf.len()) }
    }

    #[test]
    fn recv_into_reads_written_bytes() {
        let (mut writer, reader) = pair();
        writer.write_all(b"hello").expect("write");
        writer.flush().expect("flush");

        let mut buf = [0u8; 16];
        let read = read_some(&reader, &mut buf).expect("recv_into");
        assert_eq!(&buf[..read], b"hello");
    }

    #[test]
    fn recv_into_reports_eof_as_zero() {
        let (writer, reader) = pair();
        drop(writer);

        let mut buf = [0u8; 8];
        let read = read_some(&reader, &mut buf).expect("recv_into");
        assert_eq!(read, 0, "a closed peer must read as end-of-stream");
    }

    #[test]
    fn recv_into_reports_would_block_on_an_empty_nonblocking_socket() {
        let (_writer, reader) = pair();
        reader.set_nonblocking(true).expect("set nonblocking");

        let mut buf = [0u8; 8];
        let error =
            read_some(&reader, &mut buf).expect_err("empty nonblocking socket must not read");
        assert_eq!(
            error.kind(),
            io::ErrorKind::WouldBlock,
            "callers key their retry loops off WouldBlock, so the platform code \
             must normalise to it (got {error:?})"
        );
    }

    #[test]
    fn a_refused_send_is_a_datagram_error_and_socket_faults_are_not() {
        let closed = UdpSocket::bind("127.0.0.1:0").expect("bind closed");
        let closed_addr = closed.local_addr().expect("closed addr");
        drop(closed);
        let socket = UdpSocket::bind("127.0.0.1:0").expect("bind");
        socket.connect(closed_addr).expect("connect");
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .expect("read timeout");
        socket.send(b"probe").expect("send");

        let mut buf = [0u8; 8];
        let error = socket.recv(&mut buf).expect_err("nothing listens there");
        assert!(
            is_datagram_error(&error),
            "a port-unreachable describes one datagram (got {error:?})"
        );
        #[cfg(unix)]
        assert!(is_datagram_error(&io::Error::from_raw_os_error(
            libc::EMSGSIZE
        )));
        #[cfg(windows)]
        assert!(is_datagram_error(&io::Error::from_raw_os_error(
            windows_sys::Win32::Networking::WinSock::WSAEMSGSIZE
        )));
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::NotConnected] {
            assert!(!is_datagram_error(&io::Error::from(kind)));
        }
    }

    #[test]
    fn bind_udp_socket_with_applies_options_before_binding() {
        let addr = "127.0.0.1:0".parse().expect("loopback addr");
        let socket = bind_udp_socket_with(addr, |socket| {
            set_reuse_addr(socket, true)?;
            ignore_udp_icmp_errors(socket)
        })
        .expect("bind udp socket");

        let local = socket.local_addr().expect("local addr");
        assert_ne!(local.port(), 0, "the socket must be bound");
        assert_eq!(local.ip(), addr.ip());
    }

    #[test]
    fn bind_udp_socket_with_reports_a_configure_failure() {
        let addr = "127.0.0.1:0".parse().expect("loopback addr");
        let error = bind_udp_socket_with(addr, |_| Err(io::Error::new(io::ErrorKind::Other, "no")))
            .expect_err("a configure failure must abort the bind");

        assert_eq!(error.to_string(), "no");
    }
}
