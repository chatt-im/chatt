use std::{collections::VecDeque, net::SocketAddr, time::Duration};

#[derive(Clone, Debug)]
pub struct RestartPortPolicy {
    recent_ports: VecDeque<u16>,
    max_recent: usize,
    pub quarantine: Duration,
}

impl RestartPortPolicy {
    pub fn new(max_recent: usize, quarantine: Duration) -> Self {
        Self {
            recent_ports: VecDeque::with_capacity(max_recent),
            max_recent,
            quarantine,
        }
    }

    pub fn bind_addr_for_restart(previous: SocketAddr) -> SocketAddr {
        SocketAddr::new(previous.ip(), 0)
    }

    pub fn record(&mut self, port: u16) {
        if self.max_recent == 0 {
            return;
        }
        if self.recent_ports.contains(&port) {
            self.recent_ports.retain(|existing| *existing != port);
        }
        while self.recent_ports.len() >= self.max_recent {
            self.recent_ports.pop_front();
        }
        self.recent_ports.push_back(port);
    }

    pub fn accepts(&self, port: u16) -> bool {
        port != 0 && !self.recent_ports.contains(&port)
    }
}

impl Default for RestartPortPolicy {
    fn default() -> Self {
        Self::new(16, Duration::from_secs(120))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_recently_used_ports() {
        let mut policy = RestartPortPolicy::default();
        policy.record(5000);
        assert!(!policy.accepts(5000));
        assert!(policy.accepts(5001));
    }

    #[test]
    fn restart_bind_addr_requests_fresh_ephemeral_port() {
        let addr = RestartPortPolicy::bind_addr_for_restart("127.0.0.1:5000".parse().unwrap());
        assert_eq!(addr.port(), 0);
    }
}
