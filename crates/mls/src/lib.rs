//! Chatt's policy and persistence boundary around `mls-rs`.
//!
//! MLS owns group cryptography. This crate owns only the Chatt-specific
//! binding between MLS signing identities, current device rosters and fixed
//! encrypted-room account membership.

mod bootstrap;
mod installation;
mod persistent;
mod policy;
mod server;

pub use persistent::{
    CachedApplicationEvent, CachedIncomingEvent, DurableFileUpload, OutboxEntry, OutboxState,
    PendingUiDispatch, PersistentClient, ProcessedDelivery,
};
pub use policy::{ChattIdentityProvider, ChattMlsPolicy, PolicyError};
pub use server::{
    AppliedPublicCommit, PublicGroupState, PublicGroupValidator, PublicValidationError,
};

/// The only cipher suite enabled by the initial Chatt MLS protocol.
pub const CIPHER_SUITE: mls_rs::CipherSuite = mls_rs::CipherSuite::CURVE25519_AES128;
pub use bootstrap::{
    BOOTSTRAP_VERSION, BootstrapLoad, BootstrapState, E2eBootstrap, InstallationState,
    classify_installation, load_bootstrap,
};
pub use installation::LocalInstallation;
