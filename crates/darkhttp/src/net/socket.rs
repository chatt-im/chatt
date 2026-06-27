use std::io;
use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

pub(crate) fn bind_listener(addr: SocketAddr) -> io::Result<TcpListener> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

pub(crate) fn duration_to_poll_ms(duration: Duration) -> i32 {
    if duration.is_zero() {
        0
    } else {
        i32::try_from(duration.as_millis()).unwrap_or(i32::MAX)
    }
}
