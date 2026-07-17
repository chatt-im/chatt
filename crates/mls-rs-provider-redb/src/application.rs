// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// Copyright by contributors to this project.
// SPDX-License-Identifier: (Apache-2.0 OR MIT)

use std::{fmt, sync::Arc};

use redb::{Database, ReadableDatabase, ReadableTable};

use crate::{database_error, RedbDataStorageError, APPLICATION_DATA};

#[derive(Clone, Debug)]
pub struct RedbApplicationStorage {
    database: Arc<Database>,
}

impl RedbApplicationStorage {
    pub(crate) fn new(database: Arc<Database>) -> Self {
        Self { database }
    }

    pub fn insert(&self, key: &str, value: &[u8]) -> Result<usize, RedbDataStorageError> {
        let transaction = self.database.begin_write().map_err(database_error)?;
        let modified = {
            let mut table = transaction
                .open_table(APPLICATION_DATA)
                .map_err(database_error)?;
            let unchanged = table
                .get(key)
                .map_err(database_error)?
                .is_some_and(|existing| existing.value() == value);
            if unchanged {
                0
            } else {
                table.insert(key, value).map_err(database_error)?;
                1
            }
        };
        if modified == 0 {
            transaction.abort().map_err(database_error)?;
        } else {
            transaction.commit().map_err(database_error)?;
        }
        Ok(modified)
    }

    pub fn transact_insert(&self, items: &[Item]) -> Result<usize, RedbDataStorageError> {
        let transaction = self.database.begin_write().map_err(database_error)?;
        let mut modified = 0usize;
        {
            let mut table = transaction
                .open_table(APPLICATION_DATA)
                .map_err(database_error)?;
            for item in items {
                let unchanged = table
                    .get(item.key.as_str())
                    .map_err(database_error)?
                    .is_some_and(|existing| existing.value() == item.value.as_slice());
                if !unchanged {
                    table
                        .insert(item.key.as_str(), item.value.as_slice())
                        .map_err(database_error)?;
                    modified = modified.checked_add(1).ok_or_else(|| {
                        RedbDataStorageError::CorruptRecord(
                            "application modification count exceeds usize::MAX".into(),
                        )
                    })?;
                }
            }
        }
        if modified == 0 {
            transaction.abort().map_err(database_error)?;
        } else {
            transaction.commit().map_err(database_error)?;
        }
        Ok(modified)
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, RedbDataStorageError> {
        let transaction = self.database.begin_read().map_err(database_error)?;
        let table = transaction
            .open_table(APPLICATION_DATA)
            .map_err(database_error)?;
        Ok(table
            .get(key)
            .map_err(database_error)?
            .map(|value| value.value().to_vec()))
    }

    pub fn delete(&self, key: &str) -> Result<usize, RedbDataStorageError> {
        let transaction = self.database.begin_write().map_err(database_error)?;
        let removed = {
            let mut table = transaction
                .open_table(APPLICATION_DATA)
                .map_err(database_error)?;
            let removed = table.remove(key).map_err(database_error)?.is_some();
            usize::from(removed)
        };
        if removed == 0 {
            transaction.abort().map_err(database_error)?;
        } else {
            transaction.commit().map_err(database_error)?;
        }
        Ok(removed)
    }

    pub fn get_by_prefix(&self, key_prefix: &str) -> Result<Vec<Item>, RedbDataStorageError> {
        let transaction = self.database.begin_read().map_err(database_error)?;
        let table = transaction
            .open_table(APPLICATION_DATA)
            .map_err(database_error)?;
        let mut items = Vec::new();
        for entry in table.range(key_prefix..).map_err(database_error)? {
            let (key, value) = entry.map_err(database_error)?;
            if !key.value().starts_with(key_prefix) {
                break;
            }
            items.push(Item::new(key.value().to_owned(), value.value().to_vec()));
        }
        Ok(items)
    }

    pub fn delete_by_prefix(&self, key_prefix: &str) -> Result<usize, RedbDataStorageError> {
        let transaction = self.database.begin_write().map_err(database_error)?;
        let removed = {
            let mut table = transaction
                .open_table(APPLICATION_DATA)
                .map_err(database_error)?;
            let mut keys = Vec::new();
            for entry in table.range(key_prefix..).map_err(database_error)? {
                let (key, _) = entry.map_err(database_error)?;
                if !key.value().starts_with(key_prefix) {
                    break;
                }
                keys.push(key.value().to_owned());
            }
            for key in &keys {
                table.remove(key.as_str()).map_err(database_error)?;
            }
            keys.len()
        };
        if removed == 0 {
            transaction.abort().map_err(database_error)?;
        } else {
            transaction.commit().map_err(database_error)?;
        }
        Ok(removed)
    }
}

#[derive(Clone, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Item {
    pub key: String,
    pub value: Vec<u8>,
}

impl Item {
    pub fn new(key: String, value: Vec<u8>) -> Self {
        Self { key, value }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

impl fmt::Debug for Item {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Item")
            .field("key", &self.key)
            .field("value", &mls_rs_core::debug::pretty_bytes(&self.value))
            .finish()
    }
}
