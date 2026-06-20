use std::{io, net::SocketAddr, net::UdpSocket};

use crate::interfaces::LocalInterface;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpSocketOptions {
    pub reuse_addr: bool,
    pub reuse_port: bool,
    pub ignore_icmp_errors: bool,
    pub nonblocking: bool,
}

impl Default for UdpSocketOptions {
    fn default() -> Self {
        Self {
            reuse_addr: true,
            reuse_port: true,
            ignore_icmp_errors: true,
            nonblocking: true,
        }
    }
}

#[cfg(unix)]
pub fn bind_udp_socket(addr: SocketAddr, options: UdpSocketOptions) -> io::Result<UdpSocket> {
    bind_udp_socket_inner(addr, options, None)
}

#[cfg(unix)]
pub fn bind_udp_socket_on_interface(
    addr: SocketAddr,
    interface: &LocalInterface,
    options: UdpSocketOptions,
) -> io::Result<UdpSocket> {
    bind_udp_socket_inner(addr, options, Some(interface))
}

#[cfg(unix)]
fn bind_udp_socket_inner(
    addr: SocketAddr,
    options: UdpSocketOptions,
    interface: Option<&LocalInterface>,
) -> io::Result<UdpSocket> {
    use std::os::fd::FromRawFd;

    let domain = if addr.is_ipv4() {
        libc::AF_INET
    } else {
        libc::AF_INET6
    };
    let fd = unsafe { libc::socket(domain, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let result = (|| {
        if options.reuse_addr {
            set_bool_opt(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, true)?;
        }
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd"
        ))]
        if options.reuse_port {
            set_bool_opt(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT, true)?;
        }
        if options.ignore_icmp_errors {
            ignore_icmp_errors(fd);
        }
        if let Some(interface) = interface {
            bind_to_interface(fd, interface)?;
        }
        bind_fd(fd, addr)?;
        let socket = unsafe { UdpSocket::from_raw_fd(fd) };
        socket.set_nonblocking(options.nonblocking)?;
        Ok(socket)
    })();

    if result.is_err() {
        unsafe {
            libc::close(fd);
        }
    }
    result
}

#[cfg(not(unix))]
pub fn bind_udp_socket(addr: SocketAddr, options: UdpSocketOptions) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(addr)?;
    socket.set_nonblocking(options.nonblocking)?;
    let _ = options;
    Ok(socket)
}

#[cfg(not(unix))]
pub fn bind_udp_socket_on_interface(
    addr: SocketAddr,
    _interface: &LocalInterface,
    options: UdpSocketOptions,
) -> io::Result<UdpSocket> {
    bind_udp_socket(addr, options)
}

pub fn is_ignorable_udp_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset
    )
}

#[cfg(unix)]
fn set_bool_opt(
    fd: libc::c_int,
    level: libc::c_int,
    opt: libc::c_int,
    value: bool,
) -> io::Result<()> {
    let value: libc::c_int = i32::from(value);
    let rc = unsafe {
        libc::setsockopt(
            fd,
            level,
            opt,
            &value as *const _ as *const libc::c_void,
            std::mem::size_of_val(&value) as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn bind_fd(fd: libc::c_int, addr: SocketAddr) -> io::Result<()> {
    match addr {
        SocketAddr::V4(addr) => {
            let raw = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: addr.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from(*addr.ip()).to_be(),
                },
                sin_zero: [0; 8],
            };
            bind_sockaddr(
                fd,
                &raw as *const _ as *const libc::sockaddr,
                std::mem::size_of_val(&raw),
            )
        }
        SocketAddr::V6(addr) => {
            let raw = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: addr.port().to_be(),
                sin6_flowinfo: addr.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: addr.ip().octets(),
                },
                sin6_scope_id: addr.scope_id(),
            };
            bind_sockaddr(
                fd,
                &raw as *const _ as *const libc::sockaddr,
                std::mem::size_of_val(&raw),
            )
        }
    }
}

#[cfg(unix)]
fn bind_sockaddr(fd: libc::c_int, addr: *const libc::sockaddr, len: usize) -> io::Result<()> {
    let rc = unsafe { libc::bind(fd, addr, len as libc::socklen_t) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn ignore_icmp_errors(fd: libc::c_int) {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        #[allow(clippy::unnecessary_cast)]
        let _ = set_bool_opt(fd, libc::IPPROTO_IP, libc::IP_RECVERR as libc::c_int, false);
        #[allow(clippy::unnecessary_cast)]
        let _ = set_bool_opt(
            fd,
            libc::IPPROTO_IPV6,
            libc::IPV6_RECVERR as libc::c_int,
            false,
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn bind_to_interface(fd: libc::c_int, interface: &LocalInterface) -> io::Result<()> {
    let mut name = interface.name.as_bytes().to_vec();
    name.push(0);
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            name.as_ptr() as *const libc::c_void,
            name.len() as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn bind_to_interface(_fd: libc::c_int, _interface: &LocalInterface) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_nonblocking_udp_socket() {
        let socket =
            bind_udp_socket("127.0.0.1:0".parse().unwrap(), UdpSocketOptions::default()).unwrap();
        assert_ne!(socket.local_addr().unwrap().port(), 0);
        let mut buf = [0u8; 1];
        assert_eq!(
            socket.recv_from(&mut buf).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn classifies_udp_icmp_errors_as_ignorable() {
        assert!(is_ignorable_udp_error(&io::Error::from(
            io::ErrorKind::ConnectionRefused
        )));
        assert!(is_ignorable_udp_error(&io::Error::from(
            io::ErrorKind::ConnectionReset
        )));
        assert!(!is_ignorable_udp_error(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
    }
}
