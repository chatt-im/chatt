// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// Copyright by contributors to this project.
// SPDX-License-Identifier: (Apache-2.0 OR MIT)

use std::{ops::Deref, sync::Arc};

use mls_rs_core::psk::{ExternalPskId, PreSharedKey, PreSharedKeyStorage};
use redb::{Database, ReadableDatabase, ReadableTable};

use crate::{database_error, RedbDataStorageError, PSKS};

#[derive(Clone, Debug)]
pub struct RedbPreSharedKeyStorage {
    database: Arc<Database>,
}

impl RedbPreSharedKeyStorage {
    pub(crate) fn new(database: Arc<Database>) -> Self {
        Self { database }
    }

    pub fn insert(
        &self,
        psk_id: &[u8],
        psk: &PreSharedKey,
    ) -> Result<(), RedbDataStorageError> {
        let transaction = self.database.begin_write().map_err(database_error)?;
        let modified = {
            let mut table = transaction.open_table(PSKS).map_err(database_error)?;
            let unchanged = table
                .get(psk_id)
                .map_err(database_error)?
                .is_some_and(|stored| stored.value() == psk.deref());
            if !unchanged {
                table
                    .insert(psk_id, psk.deref())
                    .map_err(database_error)?;
            }
            !unchanged
        };
        if modified {
            transaction.commit().map_err(database_error)
        } else {
            transaction.abort().map_err(database_error)
        }
    }

    pub fn get(&self, psk_id: &[u8]) -> Result<Option<PreSharedKey>, RedbDataStorageError> {
        let transaction = self.database.begin_read().map_err(database_error)?;
        let table = transaction.open_table(PSKS).map_err(database_error)?;
        Ok(table
            .get(psk_id)
            .map_err(database_error)?
            .map(|value| PreSharedKey::new(value.value().to_vec())))
    }

    pub fn delete(&self, psk_id: &[u8]) -> Result<(), RedbDataStorageError> {
        let transaction = self.database.begin_write().map_err(database_error)?;
        let removed = {
            let mut table = transaction.open_table(PSKS).map_err(database_error)?;
            let removed = table.remove(psk_id).map_err(database_error)?.is_some();
            removed
        };
        if removed {
            transaction.commit().map_err(database_error)
        } else {
            transaction.abort().map_err(database_error)
        }
    }
}

#[cfg_attr(not(mls_build_async), maybe_async::must_be_sync)]
#[cfg_attr(mls_build_async, maybe_async::must_be_async)]
impl PreSharedKeyStorage for RedbPreSharedKeyStorage {
    type Error = RedbDataStorageError;

    async fn get(&self, id: &ExternalPskId) -> Result<Option<PreSharedKey>, Self::Error> {
        RedbPreSharedKeyStorage::get(self, id)
    }
}
