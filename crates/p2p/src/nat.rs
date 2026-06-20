use std::net::SocketAddr;

use crate::candidate::NatKind;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReflexiveObservation {
    pub server_addr: SocketAddr,
    pub mapped_addr: SocketAddr,
}

#[derive(Clone, Debug, Default)]
pub struct NatClassifier {
    observations: Vec<ReflexiveObservation>,
}

impl NatClassifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&mut self, observation: ReflexiveObservation) {
        if self
            .observations
            .iter()
            .any(|existing| existing.server_addr == observation.server_addr)
        {
            self.observations
                .retain(|existing| existing.server_addr != observation.server_addr);
        }
        self.observations.push(observation);
    }

    pub fn observations(&self) -> &[ReflexiveObservation] {
        &self.observations
    }

    pub fn classify(&self) -> NatKind {
        let Some(first) = self.observations.first() else {
            return NatKind::Unknown;
        };
        if self.observations.len() < 2 {
            return NatKind::Unknown;
        }
        if self
            .observations
            .iter()
            .all(|observation| observation.mapped_addr == first.mapped_addr)
        {
            NatKind::Cone
        } else {
            NatKind::Symmetric
        }
    }

    pub fn primary_reflexive_addr(&self) -> Option<SocketAddr> {
        self.observations
            .iter()
            .min_by_key(|observation| observation.server_addr)
            .map(|observation| observation.mapped_addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_cone_from_stable_mapping() {
        let mut classifier = NatClassifier::new();
        classifier.observe(ReflexiveObservation {
            server_addr: "203.0.113.1:41001".parse().unwrap(),
            mapped_addr: "198.51.100.5:55000".parse().unwrap(),
        });
        assert_eq!(classifier.classify(), NatKind::Unknown);
        classifier.observe(ReflexiveObservation {
            server_addr: "203.0.113.1:41002".parse().unwrap(),
            mapped_addr: "198.51.100.5:55000".parse().unwrap(),
        });
        assert_eq!(classifier.classify(), NatKind::Cone);
    }

    #[test]
    fn classifies_symmetric_from_destination_dependent_mapping() {
        let mut classifier = NatClassifier::new();
        classifier.observe(ReflexiveObservation {
            server_addr: "203.0.113.1:41001".parse().unwrap(),
            mapped_addr: "198.51.100.5:55000".parse().unwrap(),
        });
        classifier.observe(ReflexiveObservation {
            server_addr: "203.0.113.1:41002".parse().unwrap(),
            mapped_addr: "198.51.100.5:55001".parse().unwrap(),
        });
        assert_eq!(classifier.classify(), NatKind::Symmetric);
    }
}
