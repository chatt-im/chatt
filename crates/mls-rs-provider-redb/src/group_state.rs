// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// Copyright by contributors to this project.
// SPDX-License-Identifier: (Apache-2.0 OR MIT)

use std::sync::Arc;

use mls_rs_core::group::{EpochRecord, GroupState, GroupStateStorage};
use redb::{Database, ReadableDatabase, ReadableTable};
use zeroize::Zeroizing;

use crate::{database_error, RedbDataStorageError, EPOCHS, GROUPS};

pub const DEFAULT_EPOCH_RETENTION_LIMIT: u64 = 3;

#[derive(Clone, Debug)]
pub struct RedbGroupStateStorage {
    database: Arc<Database>,
    max_epoch_retention: u64,
}

impl RedbGroupStateStorage {
    pub(crate) fn new(database: Arc<Database>) -> Self {
        Self {
            database,
            max_epoch_retention: DEFAULT_EPOCH_RETENTION_LIMIT,
        }
    }

    pub fn with_max_epoch_retention(self, max_epoch_retention: u64) -> Self {
        Self {
            max_epoch_retention,
            ..self
        }
    }

    pub fn max_epoch_retention(&self) -> u64 {
        self.max_epoch_retention
    }

    pub fn group_ids(&self) -> Result<Vec<Vec<u8>>, RedbDataStorageError> {
        let transaction = self.database.begin_read().map_err(database_error)?;
        let table = transaction.open_table(GROUPS).map_err(database_error)?;
        table
            .iter()
            .map_err(database_error)?
            .map(|entry| {
                let (key, _) = entry.map_err(database_error)?;
                Ok(key.value().to_vec())
            })
            .collect()
    }

    pub fn delete_group(&self, group_id: &[u8]) -> Result<(), RedbDataStorageError> {
        let (start, end) = epoch_range(group_id)?;
        let transaction = self.database.begin_write().map_err(database_error)?;

        let removed_group = {
            let mut groups = transaction.open_table(GROUPS).map_err(database_error)?;
            let removed = groups.remove(group_id).map_err(database_error)?.is_some();
            removed
        };

        let removed_epochs = {
            let mut epochs = transaction.open_table(EPOCHS).map_err(database_error)?;
            let keys = epochs
                .range(start.as_slice()..=end.as_slice())
                .map_err(database_error)?
                .map(|entry| {
                    let (key, _) = entry.map_err(database_error)?;
                    Ok(key.value().to_vec())
                })
                .collect::<Result<Vec<_>, RedbDataStorageError>>()?;

            let removed = keys.len();
            for key in keys {
                epochs.remove(key.as_slice()).map_err(database_error)?;
            }
            removed
        };

        if removed_group || removed_epochs != 0 {
            transaction.commit().map_err(database_error)
        } else {
            transaction.abort().map_err(database_error)
        }
    }

    fn get_snapshot_data(&self, group_id: &[u8]) -> Result<Option<Vec<u8>>, RedbDataStorageError> {
        let transaction = self.database.begin_read().map_err(database_error)?;
        let table = transaction.open_table(GROUPS).map_err(database_error)?;
        Ok(table
            .get(group_id)
            .map_err(database_error)?
            .map(|value| value.value().to_vec()))
    }

    fn get_epoch_data(
        &self,
        group_id: &[u8],
        epoch_id: u64,
    ) -> Result<Option<Vec<u8>>, RedbDataStorageError> {
        let key = epoch_key(group_id, epoch_id)?;
        let transaction = self.database.begin_read().map_err(database_error)?;
        let table = transaction.open_table(EPOCHS).map_err(database_error)?;
        Ok(table
            .get(key.as_slice())
            .map_err(database_error)?
            .map(|value| value.value().to_vec()))
    }

    fn get_max_epoch_id(&self, group_id: &[u8]) -> Result<Option<u64>, RedbDataStorageError> {
        let (start, end) = epoch_range(group_id)?;
        let transaction = self.database.begin_read().map_err(database_error)?;
        let table = transaction.open_table(EPOCHS).map_err(database_error)?;
        let entry = table
            .range(start.as_slice()..=end.as_slice())
            .map_err(database_error)?
            .next_back()
            .transpose()
            .map_err(database_error)?;

        entry
            .map(|(key, _)| decode_epoch_id(key.value(), group_id))
            .transpose()
    }

    fn update_group_state(
        &self,
        group_id: &[u8],
        group_snapshot: &[u8],
        inserts: Vec<EpochRecord>,
        updates: Vec<EpochRecord>,
    ) -> Result<(), RedbDataStorageError> {
        let max_inserted_epoch = inserts.iter().map(|epoch| epoch.id).max();
        let transaction = self.database.begin_write().map_err(database_error)?;

        {
            let mut groups = transaction.open_table(GROUPS).map_err(database_error)?;
            groups
                .insert(group_id, group_snapshot)
                .map_err(database_error)?;
        }

        {
            let mut epochs = transaction.open_table(EPOCHS).map_err(database_error)?;

            for epoch in inserts {
                let key = epoch_key(group_id, epoch.id)?;
                if epochs
                    .get(key.as_slice())
                    .map_err(database_error)?
                    .is_some()
                {
                    return Err(RedbDataStorageError::DuplicateEpoch(epoch.id));
                }
                epochs
                    .insert(key.as_slice(), epoch.data.as_slice())
                    .map_err(database_error)?;
            }

            for epoch in updates {
                let key = epoch_key(group_id, epoch.id)?;
                if epochs
                    .get(key.as_slice())
                    .map_err(database_error)?
                    .is_none()
                {
                    return Err(RedbDataStorageError::MissingEpoch(epoch.id));
                }
                epochs
                    .insert(key.as_slice(), epoch.data.as_slice())
                    .map_err(database_error)?;
            }

            if let Some(max_epoch) = max_inserted_epoch {
                if max_epoch >= self.max_epoch_retention {
                    let delete_through = max_epoch - self.max_epoch_retention;
                    let start = epoch_key(group_id, 0)?;
                    let end = epoch_key(group_id, delete_through)?;
                    let keys = epochs
                        .range(start.as_slice()..=end.as_slice())
                        .map_err(database_error)?
                        .map(|entry| {
                            let (key, _) = entry.map_err(database_error)?;
                            Ok(key.value().to_vec())
                        })
                        .collect::<Result<Vec<_>, RedbDataStorageError>>()?;
                    for key in keys {
                        epochs.remove(key.as_slice()).map_err(database_error)?;
                    }
                }
            }
        }

        transaction.commit().map_err(database_error)
    }
}

fn epoch_prefix(group_id: &[u8]) -> Result<Vec<u8>, RedbDataStorageError> {
    let len = u32::try_from(group_id.len())
        .map_err(|_| RedbDataStorageError::GroupIdTooLong(group_id.len()))?;
    let mut key = Vec::with_capacity(4 + group_id.len());
    key.extend_from_slice(&len.to_be_bytes());
    key.extend_from_slice(group_id);
    Ok(key)
}

fn epoch_key(group_id: &[u8], epoch_id: u64) -> Result<Vec<u8>, RedbDataStorageError> {
    let mut key = epoch_prefix(group_id)?;
    key.extend_from_slice(&epoch_id.to_be_bytes());
    Ok(key)
}

fn epoch_range(group_id: &[u8]) -> Result<(Vec<u8>, Vec<u8>), RedbDataStorageError> {
    Ok((epoch_key(group_id, 0)?, epoch_key(group_id, u64::MAX)?))
}

fn decode_epoch_id(key: &[u8], group_id: &[u8]) -> Result<u64, RedbDataStorageError> {
    let prefix = epoch_prefix(group_id)?;
    let epoch = key
        .strip_prefix(prefix.as_slice())
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| RedbDataStorageError::CorruptRecord("invalid epoch key".into()))?;
    Ok(u64::from_be_bytes(epoch))
}
impl GroupStateStorage for RedbGroupStateStorage {
    type Error = RedbDataStorageError;

    fn write(
        &mut self,
        state: GroupState,
        inserts: Vec<EpochRecord>,
        updates: Vec<EpochRecord>,
    ) -> Result<(), Self::Error> {
        self.update_group_state(&state.id, &state.data, inserts, updates)
    }

    fn state(&self, group_id: &[u8]) -> Result<Option<Zeroizing<Vec<u8>>>, Self::Error> {
        Ok(self.get_snapshot_data(group_id)?.map(Into::into))
    }

    fn epoch(
        &self,
        group_id: &[u8],
        epoch_id: u64,
    ) -> Result<Option<Zeroizing<Vec<u8>>>, Self::Error> {
        Ok(self.get_epoch_data(group_id, epoch_id)?.map(Into::into))
    }

    fn max_epoch_id(&self, group_id: &[u8]) -> Result<Option<u64>, Self::Error> {
        self.get_max_epoch_id(group_id)
    }
}
