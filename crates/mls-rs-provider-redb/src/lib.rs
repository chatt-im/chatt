// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// Copyright by contributors to this project.
// SPDX-License-Identifier: (Apache-2.0 OR MIT)

//! Persistent redb storage for mls-rs.
//!
//! # Security
//!
//! redb does not encrypt database contents, and its copy-on-write design means
//! deletion does not securely erase old pages. MLS state contains secrets. Use
//! encrypted storage or authenticated record encryption with externally managed
//! keys in production.

use std::{
    fs::{self, OpenOptions},
    path::Path,
    sync::Arc,
};

use redb::{Database, MultimapTableDefinition, ReadableTable, TableDefinition};
use thiserror::Error;

mod application;
mod group_state;
mod key_package;
mod psk;

pub mod storage {
    pub use crate::{
        application::{Item, RedbApplicationStorage},
        group_state::RedbGroupStateStorage,
        key_package::RedbKeyPackageStorage,
        psk::RedbPreSharedKeyStorage,
    };
}

use storage::{
    RedbApplicationStorage, RedbGroupStateStorage, RedbKeyPackageStorage, RedbPreSharedKeyStorage,
};

const SCHEMA_VERSION: u64 = 1;
const SCHEMA_VERSION_KEY: &str = "schema_version";

pub(crate) const GROUPS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("mls_groups");
pub(crate) const EPOCHS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("mls_epochs");
pub(crate) const KEY_PACKAGES: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("mls_key_packages");
pub(crate) const KEY_PACKAGE_EXPIRY: MultimapTableDefinition<u64, &[u8]> =
    MultimapTableDefinition::new("mls_key_package_expiry");
pub(crate) const PSKS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("mls_psks");
pub(crate) const APPLICATION_DATA: TableDefinition<&str, &[u8]> =
    TableDefinition::new("application_data");
pub(crate) const PROVIDER_METADATA: TableDefinition<&str, u64> =
    TableDefinition::new("provider_metadata");

#[derive(Debug, Error)]
pub enum RedbDataStorageError {
    #[error("database file error: {0}")]
    DatabaseFile(String),
    /// A redb operation failed. After a write or commit I/O failure, discard
    /// every clone of the engine and reopen the database before continuing.
    #[error(transparent)]
    Database(#[from] redb::Error),
    #[error(transparent)]
    MlsCodec(#[from] mls_rs_core::mls_rs_codec::Error),
    #[error("unsupported provider schema version {found}; supported version is {supported}")]
    UnsupportedSchemaVersion { found: u64, supported: u64 },
    #[error("key package ID already exists")]
    DuplicateKeyPackage,
    #[error("epoch {0} already exists")]
    DuplicateEpoch(u64),
    #[error("epoch {0} does not exist")]
    MissingEpoch(u64),
    #[error("group ID length {0} exceeds u32::MAX")]
    GroupIdTooLong(usize),
    #[error("corrupt stored record: {0}")]
    CorruptRecord(String),
    #[error("key-package expiration secondary index is inconsistent: {0}")]
    SecondaryIndexInconsistency(String),
}

impl mls_rs_core::error::IntoAnyError for RedbDataStorageError {
    fn into_dyn_error(self) -> Result<Box<dyn std::error::Error + Send + Sync>, Self> {
        Ok(self.into())
    }
}

pub(crate) fn database_error<E: Into<redb::Error>>(error: E) -> RedbDataStorageError {
    RedbDataStorageError::Database(error.into())
}

#[derive(Clone, Debug)]
pub struct RedbDataStorageEngine {
    database: Arc<Database>,
}

impl RedbDataStorageEngine {
    /// Opens or creates a database at `path` and initializes the provider schema.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RedbDataStorageError> {
        let path = path.as_ref();
        ensure_private_file(path)?;
        Self::new(Database::create(path).map_err(database_error)?)
    }

    /// Uses an already-open redb database and initializes the provider schema.
    pub fn new(database: Database) -> Result<Self, RedbDataStorageError> {
        let engine = Self {
            database: Arc::new(database),
        };
        engine.initialize_schema()?;
        Ok(engine)
    }

    fn initialize_schema(&self) -> Result<(), RedbDataStorageError> {
        let transaction = self.database.begin_write().map_err(database_error)?;

        {
            let mut metadata = transaction
                .open_table(PROVIDER_METADATA)
                .map_err(database_error)?;
            let current_version = metadata
                .get(SCHEMA_VERSION_KEY)
                .map_err(database_error)?
                .map(|value| value.value());
            match current_version {
                Some(SCHEMA_VERSION) => {}
                Some(found) => {
                    return Err(RedbDataStorageError::UnsupportedSchemaVersion {
                        found,
                        supported: SCHEMA_VERSION,
                    });
                }
                None => {
                    metadata
                        .insert(SCHEMA_VERSION_KEY, SCHEMA_VERSION)
                        .map_err(database_error)?;
                }
            };
        }

        transaction.open_table(GROUPS).map_err(database_error)?;
        transaction.open_table(EPOCHS).map_err(database_error)?;
        transaction
            .open_table(KEY_PACKAGES)
            .map_err(database_error)?;
        transaction
            .open_multimap_table(KEY_PACKAGE_EXPIRY)
            .map_err(database_error)?;
        transaction.open_table(PSKS).map_err(database_error)?;
        transaction
            .open_table(APPLICATION_DATA)
            .map_err(database_error)?;

        transaction.commit().map_err(database_error)
    }

    pub fn group_state_storage(&self) -> RedbGroupStateStorage {
        RedbGroupStateStorage::new(Arc::clone(&self.database))
    }

    pub fn key_package_storage(&self) -> RedbKeyPackageStorage {
        RedbKeyPackageStorage::new(Arc::clone(&self.database))
    }

    pub fn pre_shared_key_storage(&self) -> RedbPreSharedKeyStorage {
        RedbPreSharedKeyStorage::new(Arc::clone(&self.database))
    }

    pub fn application_data_storage(&self) -> RedbApplicationStorage {
        RedbApplicationStorage::new(Arc::clone(&self.database))
    }

    /// Returns the shared database for advanced redb configuration or inspection.
    pub fn database(&self) -> Arc<Database> {
        Arc::clone(&self.database)
    }
}

fn ensure_private_file(path: &Path) -> Result<(), RedbDataStorageError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path).map_err(|error| {
        RedbDataStorageError::DatabaseFile(format!(
            "failed to create or open {}: {error}",
            path.display()
        ))
    })?;
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let metadata = fs::metadata(path).map_err(|error| {
            RedbDataStorageError::DatabaseFile(format!(
                "failed to inspect {}: {error}",
                path.display()
            ))
        })?;
        let mode = metadata.mode() & 0o777;
        if mode != 0o600 {
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
                RedbDataStorageError::DatabaseFile(format!(
                    "failed to secure {}: {error}",
                    path.display()
                ))
            })?;
        }
    }
    Ok(())
}
