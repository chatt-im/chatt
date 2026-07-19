//! Best-effort socket quality-of-service settings for real-time voice media.

use std::{error::Error, fmt, io, net::SocketAddr, os::fd::RawFd};

/// Expedited Forwarding DSCP, recommended for low-delay voice bearer traffic.
pub const VOICE_DSCP: u8 = 46;

const VOICE_TRAFFIC_CLASS: libc::c_int = (VOICE_DSCP as libc::c_int) << 2;

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
pub fn apply_voice_qos(fd: RawFd, local_addr: SocketAddr) -> Result<(), VoiceQosError> {
    let (level, option) = if local_addr.is_ipv4() {
        (libc::IPPROTO_IP, libc::IP_TOS)
    } else {
        (libc::IPPROTO_IPV6, libc::IPV6_TCLASS)
    };
    let traffic_class = set_int_option(fd, level, option, VOICE_TRAFFIC_CLASS);

    #[cfg(target_os = "linux")]
    let socket_priority = set_int_option(fd, libc::SOL_SOCKET, libc::SO_PRIORITY, 6);
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

fn set_int_option(
    fd: RawFd,
    level: libc::c_int,
    option: libc::c_int,
    value: libc::c_int,
) -> io::Result<()> {
    let result = unsafe {
        libc::setsockopt(
            fd,
            level,
            option,
            &value as *const _ as *const libc::c_void,
            std::mem::size_of_val(&value) as libc::socklen_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::UdpSocket, os::fd::AsRawFd};

    #[test]
    fn applies_voice_qos_to_ipv4_socket() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        apply_voice_qos(socket.as_raw_fd(), socket.local_addr().unwrap()).unwrap();

        assert_eq!(
            get_int_option(socket.as_raw_fd(), libc::IPPROTO_IP, libc::IP_TOS).unwrap() & !0b11,
            VOICE_TRAFFIC_CLASS
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            get_int_option(socket.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PRIORITY).unwrap(),
            6
        );
    }

    #[test]
    fn applies_voice_qos_to_ipv6_socket_when_available() {
        let Ok(socket) = UdpSocket::bind("[::1]:0") else {
            return;
        };
        apply_voice_qos(socket.as_raw_fd(), socket.local_addr().unwrap()).unwrap();

        assert_eq!(
            get_int_option(socket.as_raw_fd(), libc::IPPROTO_IPV6, libc::IPV6_TCLASS,).unwrap()
                & !0b11,
            VOICE_TRAFFIC_CLASS
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            get_int_option(socket.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PRIORITY).unwrap(),
            6
        );
    }

    fn get_int_option(fd: RawFd, level: libc::c_int, option: libc::c_int) -> io::Result<i32> {
        let mut value = 0;
        let mut len = std::mem::size_of_val(&value) as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                fd,
                level,
                option,
                &mut value as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if result == 0 {
            Ok(value)
        } else {
            Err(io::Error::last_os_error())
        }
    }
}
