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

/// Binds a UDP socket at `addr` with `options` applied before the bind.
pub fn bind_udp_socket(addr: SocketAddr, options: UdpSocketOptions) -> io::Result<UdpSocket> {
    bind_udp_socket_inner(addr, options, None)
}

/// As [`bind_udp_socket`], additionally scoping the socket to `interface`.
pub fn bind_udp_socket_on_interface(
    addr: SocketAddr,
    interface: &LocalInterface,
    options: UdpSocketOptions,
) -> io::Result<UdpSocket> {
    bind_udp_socket_inner(addr, options, Some(interface))
}

fn bind_udp_socket_inner(
    addr: SocketAddr,
    options: UdpSocketOptions,
    interface: Option<&LocalInterface>,
) -> io::Result<UdpSocket> {
    let socket = chatt_platform::bind_udp_socket_with(addr, |socket| {
        if options.reuse_addr {
            chatt_platform::set_reuse_addr(socket, true)?;
        }
        if options.reuse_port {
            set_reuse_port(socket)?;
        }
        if options.ignore_icmp_errors {
            chatt_platform::ignore_udp_icmp_errors(socket)?;
        }
        if let Some(interface) = interface {
            bind_to_interface(socket, interface)?;
        }
        Ok(())
    })?;
    socket.set_nonblocking(options.nonblocking)?;
    Ok(socket)
}

/// Requests that several sockets share `addr`, where the platform has a
/// separate option for it.
///
/// Windows has no `SO_REUSEPORT`: there `SO_REUSEADDR` alone is what lets a
/// second socket bind a port already in use, so this is a successful no-op.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd"
))]
fn set_reuse_port(socket: chatt_platform::RawSock) -> io::Result<()> {
    chatt_platform::setsockopt_int(
        socket,
        chatt_platform::SockLevel::Socket,
        libc::SO_REUSEPORT,
        1,
    )
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd"
)))]
fn set_reuse_port(_socket: chatt_platform::RawSock) -> io::Result<()> {
    Ok(())
}

pub fn is_ignorable_udp_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionReset
    )
}

/// Pins outbound traffic to one interface, where the platform can.
///
/// Only Linux has `SO_BINDTODEVICE`; elsewhere binding to the interface's own
/// address is the closest equivalent, and the caller has already done that.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn bind_to_interface(
    socket: chatt_platform::RawSock,
    interface: &LocalInterface,
) -> io::Result<()> {
    let mut name = interface.name.as_bytes().to_vec();
    name.push(0);
    // SAFETY: the name outlives the call and its length is passed explicitly.
    let result = unsafe {
        libc::setsockopt(
            socket,
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            name.as_ptr().cast(),
            name.len() as libc::socklen_t,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn bind_to_interface(
    _socket: chatt_platform::RawSock,
    _interface: &LocalInterface,
) -> io::Result<()> {
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
