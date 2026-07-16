// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// Copyright by contributors to this project.
// SPDX-License-Identifier: (Apache-2.0 OR MIT)

use std::sync::Arc;

use mls_rs_core::{
    key_package::{KeyPackageData, KeyPackageStorage},
    mls_rs_codec::{MlsDecode, MlsEncode},
    time::MlsTime,
};
use redb::{
    Database, ReadableDatabase, ReadableMultimapTable, ReadableTable, ReadableTableMetadata,
};

use crate::{
    database_error, RedbDataStorageError, KEY_PACKAGES, KEY_PACKAGE_EXPIRY,
};

#[derive(Clone, Debug)]
pub struct RedbKeyPackageStorage {
    database: Arc<Database>,
}

impl RedbKeyPackageStorage {
    pub(crate) fn new(database: Arc<Database>) -> Self {
        Self { database }
    }

    fn insert_package(
        &self,
        id: &[u8],
        key_package: KeyPackageData,
    ) -> Result<(), RedbDataStorageError> {
        let expiration = key_package.expiration;
        let encoded = key_package.mls_encode_to_vec()?;
        let record = encode_record(expiration, &encoded);
        let transaction = self.database.begin_write().map_err(database_error)?;

        {
            let mut packages = transaction
                .open_table(KEY_PACKAGES)
                .map_err(database_error)?;
            if packages.get(id).map_err(database_error)?.is_some() {
                return Err(RedbDataStorageError::DuplicateKeyPackage);
            }
            packages
                .insert(id, record.as_slice())
                .map_err(database_error)?;
        }

        {
            let mut expiry = transaction
                .open_multimap_table(KEY_PACKAGE_EXPIRY)
                .map_err(database_error)?;
            expiry.insert(expiration, id).map_err(database_error)?;
        }

        transaction.commit().map_err(database_error)
    }

    fn get_package(&self, id: &[u8]) -> Result<Option<KeyPackageData>, RedbDataStorageError> {
        let transaction = self.database.begin_read().map_err(database_error)?;
        let packages = transaction
            .open_table(KEY_PACKAGES)
            .map_err(database_error)?;
        let Some(record) = packages.get(id).map_err(database_error)? else {
            return Ok(None);
        };
        let record = record.value();
        let (expiration, encoded) = decode_record(record)?;
        let package = decode_package(encoded)?;
        if package.expiration != expiration {
            return Err(RedbDataStorageError::CorruptRecord(
                "key-package expiration does not match encoded package".into(),
            ));
        }

        let expiry = transaction
            .open_multimap_table(KEY_PACKAGE_EXPIRY)
            .map_err(database_error)?;
        if !index_contains(&expiry, expiration, id)? {
            return Err(RedbDataStorageError::SecondaryIndexInconsistency(
                "primary record is absent from expiry index".into(),
            ));
        }

        Ok(Some(package))
    }

    pub fn delete(&self, id: &[u8]) -> Result<(), RedbDataStorageError> {
        let transaction = self.database.begin_write().map_err(database_error)?;
        let expiration = {
            let packages = transaction
                .open_table(KEY_PACKAGES)
                .map_err(database_error)?;
            let expiration = packages
                .get(id)
                .map_err(database_error)?
                .map(|record| decode_record(record.value()).map(|(expiration, _)| expiration))
                .transpose()?;
            expiration
        };

        let Some(expiration) = expiration else {
            transaction.commit().map_err(database_error)?;
            return Ok(());
        };

        {
            let mut expiry = transaction
                .open_multimap_table(KEY_PACKAGE_EXPIRY)
                .map_err(database_error)?;
            if !expiry.remove(expiration, id).map_err(database_error)? {
                return Err(RedbDataStorageError::SecondaryIndexInconsistency(
                    "primary record is absent from expiry index".into(),
                ));
            }
        }

        {
            let mut packages = transaction
                .open_table(KEY_PACKAGES)
                .map_err(database_error)?;
            packages.remove(id).map_err(database_error)?;
        }

        transaction.commit().map_err(database_error)
    }

    pub fn delete_expired(&self) -> Result<(), RedbDataStorageError> {
        self.delete_expired_by_time(MlsTime::now().seconds_since_epoch())
    }

    pub fn delete_expired_by_time(&self, time: u64) -> Result<(), RedbDataStorageError> {
        let transaction = self.database.begin_write().map_err(database_error)?;

        let indexed = {
            let expiry = transaction
                .open_multimap_table(KEY_PACKAGE_EXPIRY)
                .map_err(database_error)?;
            let mut indexed = Vec::new();
            for entry in expiry.range(..time).map_err(database_error)? {
                let (expiration, ids) = entry.map_err(database_error)?;
                for id in ids {
                    indexed.push((
                        expiration.value(),
                        id.map_err(database_error)?.value().to_vec(),
                    ));
                }
            }
            indexed
        };

        {
            let packages = transaction
                .open_table(KEY_PACKAGES)
                .map_err(database_error)?;
            for (expiration, id) in &indexed {
                let record = packages
                    .get(id.as_slice())
                    .map_err(database_error)?
                    .ok_or_else(|| {
                        RedbDataStorageError::SecondaryIndexInconsistency(
                            "expiry index points to a missing primary record".into(),
                        )
                    })?;
                let (stored_expiration, _) = decode_record(record.value())?;
                if stored_expiration != *expiration {
                    return Err(RedbDataStorageError::SecondaryIndexInconsistency(
                        "expiry index timestamp differs from primary record".into(),
                    ));
                }
            }

            for entry in packages.iter().map_err(database_error)? {
                let (id, record) = entry.map_err(database_error)?;
                let (expiration, _) = decode_record(record.value())?;
                if expiration < time
                    && !indexed.iter().any(|(indexed_expiration, indexed_id)| {
                        *indexed_expiration == expiration && indexed_id == id.value()
                    })
                {
                    return Err(RedbDataStorageError::SecondaryIndexInconsistency(
                        "expired primary record is absent from expiry index".into(),
                    ));
                }
            }
        }

        {
            let mut expiry = transaction
                .open_multimap_table(KEY_PACKAGE_EXPIRY)
                .map_err(database_error)?;
            for (expiration, id) in &indexed {
                if !expiry
                    .remove(*expiration, id.as_slice())
                    .map_err(database_error)?
                {
                    return Err(RedbDataStorageError::SecondaryIndexInconsistency(
                        "expiry entry disappeared during transaction".into(),
                    ));
                }
            }
        }

        {
            let mut packages = transaction
                .open_table(KEY_PACKAGES)
                .map_err(database_error)?;
            for (_, id) in indexed {
                packages.remove(id.as_slice()).map_err(database_error)?;
            }
        }

        transaction.commit().map_err(database_error)
    }

    pub fn count(&self) -> Result<usize, RedbDataStorageError> {
        let transaction = self.database.begin_read().map_err(database_error)?;
        let table = transaction
            .open_table(KEY_PACKAGES)
            .map_err(database_error)?;
        let count = table.len().map_err(database_error)?;
        usize::try_from(count).map_err(|_| {
            RedbDataStorageError::CorruptRecord("key-package count exceeds usize::MAX".into())
        })
    }

    pub fn count_at_time(&self, time: u64) -> Result<usize, RedbDataStorageError> {
        let transaction = self.database.begin_read().map_err(database_error)?;
        let packages = transaction
            .open_table(KEY_PACKAGES)
            .map_err(database_error)?;
        let expiry = transaction
            .open_multimap_table(KEY_PACKAGE_EXPIRY)
            .map_err(database_error)?;
        let mut count = 0usize;

        for entry in packages.iter().map_err(database_error)? {
            let (id, record) = entry.map_err(database_error)?;
            let (expiration, _) = decode_record(record.value())?;
            if !index_contains(&expiry, expiration, id.value())? {
                return Err(RedbDataStorageError::SecondaryIndexInconsistency(
                    "primary record is absent from expiry index".into(),
                ));
            }
            if expiration >= time {
                count = count.checked_add(1).ok_or_else(|| {
                    RedbDataStorageError::CorruptRecord(
                        "key-package count exceeds usize::MAX".into(),
                    )
                })?;
            }
        }

        Ok(count)
    }
}

fn encode_record(expiration: u64, encoded: &[u8]) -> Vec<u8> {
    let mut record = Vec::with_capacity(8 + encoded.len());
    record.extend_from_slice(&expiration.to_be_bytes());
    record.extend_from_slice(encoded);
    record
}

fn decode_record(record: &[u8]) -> Result<(u64, &[u8]), RedbDataStorageError> {
    let expiration = record
        .get(..8)
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| {
            RedbDataStorageError::CorruptRecord("key-package record is shorter than 8 bytes".into())
        })?;
    Ok((u64::from_be_bytes(expiration), &record[8..]))
}

fn decode_package(encoded: &[u8]) -> Result<KeyPackageData, RedbDataStorageError> {
    let mut reader = encoded;
    let package = KeyPackageData::mls_decode(&mut reader)?;
    if !reader.is_empty() {
        return Err(RedbDataStorageError::CorruptRecord(
            "key-package record has trailing data".into(),
        ));
    }
    Ok(package)
}

fn index_contains<T: ReadableMultimapTable<u64, &'static [u8]>>(
    expiry: &T,
    expiration: u64,
    id: &[u8],
) -> Result<bool, RedbDataStorageError> {
    for value in expiry.get(expiration).map_err(database_error)? {
        if value.map_err(database_error)?.value() == id {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg_attr(not(mls_build_async), maybe_async::must_be_sync)]
#[cfg_attr(mls_build_async, maybe_async::must_be_async)]
impl KeyPackageStorage for RedbKeyPackageStorage {
    type Error = RedbDataStorageError;

    async fn insert(&mut self, id: Vec<u8>, pkg: KeyPackageData) -> Result<(), Self::Error> {
        self.insert_package(&id, pkg)
    }

    async fn get(&self, id: &[u8]) -> Result<Option<KeyPackageData>, Self::Error> {
        self.get_package(id)
    }

    async fn delete(&mut self, id: &[u8]) -> Result<(), Self::Error> {
        RedbKeyPackageStorage::delete(self, id)
    }
}
