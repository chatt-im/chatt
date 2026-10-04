//! Thin platform layer for the operations Chatt performs below `std`.
//!
//! Most of the codebase works through `std`, but some paths need things `std`
//! does not expose: reading straight into uninitialised buffer capacity,
//! setting DSCP and reuse options on sockets, polling a heterogeneous set of
//! them, creating files and directories only their owner can reach, and
//! claiming a name so a second process cannot share local state. Those are
//! spelled differently on POSIX and Win32, so they are named once here rather
//! than at each call site.
//!
//! The naming follows Winsock's model, where a socket is its own kind of
//! handle: [`RawSock`] is a `RawFd` on Unix and a `RawSocket` on Windows, and
//! [`AsRawSock`] is whichever of `AsRawFd`/`AsRawSocket` the platform provides.

pub mod file;
pub mod instance;
pub mod osstr;
pub mod poll;
pub mod security;
pub mod sock;

pub use file::{PrivateOpenOptions, create_private_dir, read_at, read_exact_at, sync_dir};
pub use osstr::{os_str_to_bytes, os_string_from_bytes};
pub use security::is_owner_only;

pub use poll::{POLLERR, POLLHUP, POLLIN, POLLOUT, PollFd, poll, pollfd};
pub use sock::{
    AsRawSock, RawSock, SockLevel, SocketSource, apply_dscp, bind_udp_socket_with, dscp_option,
    getsockopt_int, ignore_udp_icmp_errors, is_datagram_error, is_interrupted, set_reuse_addr,
    setsockopt_int,
};
