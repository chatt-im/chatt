//! Best-effort socket quality-of-service settings for real-time voice media.

use std::{error::Error, fmt, io, net::SocketAddr};

use chatt_platform::{RawSock, apply_dscp};

/// Expedited Forwarding DSCP, recommended for low-delay voice bearer traffic.
pub const VOICE_DSCP: u8 = 46;

#[derive(Debug)]
pub enum VoiceQosError {
    TrafficClass(io::Error),
    SocketPriority(io::Error),
    TrafficClassAndSocketPriority {
        traffic_class: io::Error,
        socket_priority: io::Error,
    },
}

impl fmt::Display for VoiceQosError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TrafficClass(error) => write!(f, "IP traffic class: {error}"),
            Self::SocketPriority(error) => write!(f, "socket priority: {error}"),
            Self::TrafficClassAndSocketPriority {
                traffic_class,
                socket_priority,
            } => write!(
                f,
                "IP traffic class: {traffic_class}; socket priority: {socket_priority}"
            ),
        }
    }
}

impl Error for VoiceQosError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::TrafficClass(error) | Self::SocketPriority(error) => Some(error),
            Self::TrafficClassAndSocketPriority { traffic_class, .. } => Some(traffic_class),
        }
    }
}

/// Applies the outbound voice traffic class and any platform-local socket
/// priority supported by Chatt.
///
/// Every applicable setting is attempted even if an earlier one fails. The
/// caller decides whether the combined failure is fatal; media sockets use
/// this as a best-effort hint and continue after logging it.
pub fn apply_voice_qos(socket: RawSock, local_addr: SocketAddr) -> Result<(), VoiceQosError> {
    let traffic_class = apply_dscp(socket, local_addr, VOICE_DSCP);

    // `SO_PRIORITY` sets the kernel's local queueing priority and exists only
    // on Linux; elsewhere the DSCP marking is the whole story.
    #[cfg(target_os = "linux")]
    let socket_priority = chatt_platform::sock::setsockopt_int(
        socket,
        chatt_platform::SockLevel::Socket,
        libc::SO_PRIORITY,
        6,
    );
    #[cfg(not(target_os = "linux"))]
    let socket_priority = Ok(());

    match (traffic_class, socket_priority) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(VoiceQosError::TrafficClass(error)),
        (Ok(()), Err(error)) => Err(VoiceQosError::SocketPriority(error)),
        (Err(traffic_class), Err(socket_priority)) => {
            Err(VoiceQosError::TrafficClassAndSocketPriority {
                traffic_class,
                socket_priority,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chatt_platform::{AsRawSock, getsockopt_int, sock::dscp_option};
    use std::net::UdpSocket;

    /// [`VOICE_DSCP`] as it appears in the traffic-class byte, which is what
    /// reads back from the socket. Only the platforms that read it back
    /// compare against it; see the note in the assertions below.
    #[cfg(not(windows))]
    const VOICE_TRAFFIC_CLASS: i32 = (VOICE_DSCP as i32) << 2;

    #[test]
    fn applies_voice_qos_to_ipv4_socket() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let local = socket.local_addr().unwrap();
        apply_voice_qos(socket.as_raw_sock(), local).unwrap();

        let (level, option) = dscp_option(local);
        // Windows accepts IP_TOS and then ignores it unless the machine opts
        // back in through policy, so the value read back is not required to
        // match there; the call succeeding is all this can assert.
        #[cfg(not(windows))]
        assert_eq!(
            getsockopt_int(socket.as_raw_sock(), level, option).unwrap() & !0b11,
            VOICE_TRAFFIC_CLASS
        );
        #[cfg(windows)]
        let _ = getsockopt_int(socket.as_raw_sock(), level, option);

        #[cfg(target_os = "linux")]
        assert_eq!(
            getsockopt_int(
                socket.as_raw_sock(),
                chatt_platform::SockLevel::Socket,
                libc::SO_PRIORITY
            )
            .unwrap(),
            6
        );
    }

    #[test]
    fn applies_voice_qos_to_ipv6_socket_when_available() {
        let Ok(socket) = UdpSocket::bind("[::1]:0") else {
            return;
        };
        let local = socket.local_addr().unwrap();
        let applied = apply_voice_qos(socket.as_raw_sock(), local);
        #[cfg(not(windows))]
        applied.unwrap();

        // Unlike IP_TOS, which Windows accepts and then ignores, IPV6_TCLASS
        // has no setsockopt route there at all: marking v6 traffic goes
        // through the qWAVE QoS2 API, so the call comes back WSAENOPROTOOPT.
        // Voice QoS is best-effort and the media sockets log and carry on, so
        // that specific refusal is the expected outcome -- but only that one.
        #[cfg(windows)]
        {
            use windows_sys::Win32::Networking::WinSock::WSAENOPROTOOPT;
            match &applied {
                Ok(()) => {}
                Err(VoiceQosError::TrafficClass(error))
                    if error.raw_os_error() == Some(WSAENOPROTOOPT) => {}
                Err(error) => panic!("unexpected IPv6 voice QoS failure: {error}"),
            }
        }

        let (level, option) = dscp_option(local);
        #[cfg(not(windows))]
        assert_eq!(
            getsockopt_int(socket.as_raw_sock(), level, option).unwrap() & !0b11,
            VOICE_TRAFFIC_CLASS
        );
        #[cfg(windows)]
        let _ = getsockopt_int(socket.as_raw_sock(), level, option);

        #[cfg(target_os = "linux")]
        assert_eq!(
            getsockopt_int(
                socket.as_raw_sock(),
                chatt_platform::SockLevel::Socket,
                libc::SO_PRIORITY
            )
            .unwrap(),
            6
        );
    }
}
