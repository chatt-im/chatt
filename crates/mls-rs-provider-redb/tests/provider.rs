use assert_matches::assert_matches;
use mls_rs_core::{
    crypto::HpkeSecretKey,
    group::{EpochRecord, GroupState, GroupStateStorage},
    key_package::{KeyPackageData, KeyPackageStorage},
    psk::PreSharedKey,
};
use mls_rs_provider_redb::{RedbDataStorageEngine, RedbDataStorageError};
use redb::{
    Database, MultimapTableDefinition, ReadableDatabase, ReadableTableMetadata,
    TableDefinition,
};
use tempfile::{tempdir, NamedTempFile};

const GROUPS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("mls_groups");
const EPOCHS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("mls_epochs");
const KEY_PACKAGES: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("mls_key_packages");
const KEY_PACKAGE_EXPIRY: MultimapTableDefinition<u64, &[u8]> =
    MultimapTableDefinition::new("mls_key_package_expiry");
const PROVIDER_METADATA: TableDefinition<&str, u64> =
    TableDefinition::new("provider_metadata");

fn engine() -> RedbDataStorageEngine {
    RedbDataStorageEngine::open(NamedTempFile::new().unwrap().path()).unwrap()
}

fn group_state(id: &[u8], data: &[u8]) -> GroupState {
    GroupState {
        id: id.to_vec(),
        data: data.to_vec().into(),
    }
}

fn epoch(id: u64, byte: u8) -> EpochRecord {
    EpochRecord::new(id, vec![byte; 16].into())
}

fn key_package(expiration: u64, byte: u8) -> KeyPackageData {
    KeyPackageData::new(
        vec![byte; 32],
        HpkeSecretKey::from(vec![byte; 32]),
        HpkeSecretKey::from(vec![byte.wrapping_add(1); 32]),
        expiration,
    )
}

#[test]
fn groups_upsert_update_and_retain_the_actual_highest_insert() {
    let engine = engine();
    let mut storage = engine
        .group_state_storage()
        .with_max_epoch_retention(3);

    storage
        .write(group_state(b"group", b"snapshot-1"), vec![epoch(0, 0)], vec![])
        .unwrap();
    storage
        .write(
            group_state(b"group", b"snapshot-2"),
            vec![epoch(10, 10), epoch(5, 5), epoch(12, 12), epoch(11, 11)],
            vec![],
        )
        .unwrap();

    assert_eq!(storage.state(b"group").unwrap().unwrap().as_slice(), b"snapshot-2");
    assert!(storage.epoch(b"group", 5).unwrap().is_none());
    for id in 10..=12 {
        assert!(storage.epoch(b"group", id).unwrap().is_some());
    }
    assert_eq!(storage.max_epoch_id(b"group").unwrap(), Some(12));
}

#[test]
fn groups_list_multiple_ids_and_update_existing_epochs() {
    let engine = engine();
    let mut storage = engine.group_state_storage();
    assert_eq!(storage.max_epoch_id(b"missing").unwrap(), None);

    storage
        .write(group_state(b"one", b"first"), vec![epoch(0, 1)], vec![])
        .unwrap();
    storage
        .write(group_state(b"two", b"second"), vec![], vec![])
        .unwrap();
    assert_eq!(storage.max_epoch_id(b"two").unwrap(), None);

    storage
        .write(
            group_state(b"one", b"updated"),
            vec![],
            vec![epoch(0, 9)],
        )
        .unwrap();
    assert_eq!(storage.epoch(b"one", 0).unwrap().unwrap().as_slice(), &[9; 16]);

    let mut ids = storage.group_ids().unwrap();
    ids.sort();
    assert_eq!(ids, vec![b"one".to_vec(), b"two".to_vec()]);
}

#[test]
fn epoch_updates_require_existing_records_and_support_full_u64() {
    let engine = engine();
    let mut storage = engine.group_state_storage();
    storage
        .write(
            group_state(b"group", b"snapshot"),
            vec![epoch(u64::MAX, 1)],
            vec![],
        )
        .unwrap();
    assert_eq!(storage.max_epoch_id(b"group").unwrap(), Some(u64::MAX));

    let error = storage
        .write(
            group_state(b"group", b"changed"),
            vec![],
            vec![epoch(4, 4)],
        )
        .unwrap_err();
    assert_matches!(error, RedbDataStorageError::MissingEpoch(4));
    assert_eq!(storage.state(b"group").unwrap().unwrap().as_slice(), b"snapshot");
}

#[test]
fn duplicate_epoch_rolls_back_the_complete_group_write() {
    let engine = engine();
    let mut storage = engine.group_state_storage();
    storage
        .write(
            group_state(b"group", b"original"),
            vec![epoch(0, 0)],
            vec![],
        )
        .unwrap();

    let error = storage
        .write(
            group_state(b"group", b"must-roll-back"),
            vec![epoch(1, 1), epoch(0, 9)],
            vec![],
        )
        .unwrap_err();
    assert_matches!(error, RedbDataStorageError::DuplicateEpoch(0));
    assert_eq!(storage.state(b"group").unwrap().unwrap().as_slice(), b"original");
    assert!(storage.epoch(b"group", 1).unwrap().is_none());
}

#[test]
fn deleting_a_group_explicitly_deletes_all_epochs() {
    let engine = engine();
    let mut storage = engine
        .group_state_storage()
        .with_max_epoch_retention(u64::MAX);
    storage
        .write(
            group_state(b"group", b"snapshot"),
            vec![epoch(0, 0), epoch(1, 1), epoch(2, 2)],
            vec![],
        )
        .unwrap();

    storage.delete_group(b"group").unwrap();
    assert!(storage.state(b"group").unwrap().is_none());
    assert_eq!(storage.max_epoch_id(b"group").unwrap(), None);

    let transaction = engine.database().begin_read().unwrap();
    let epochs = transaction.open_table(EPOCHS).unwrap();
    assert_eq!(epochs.len().unwrap(), 0);
}

#[test]
fn file_data_survives_close_and_reopen() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("state.redb");
    {
        let engine = RedbDataStorageEngine::open(&path).unwrap();
        let mut groups = engine.group_state_storage();
        groups
            .write(
                group_state(b"persistent", b"snapshot"),
                vec![epoch(7, 7)],
                vec![],
            )
            .unwrap();
        engine
            .application_data_storage()
            .insert("persistent-key", b"value")
            .unwrap();
    }

    let reopened = RedbDataStorageEngine::open(&path).unwrap();
    assert_eq!(
        reopened
            .group_state_storage()
            .state(b"persistent")
            .unwrap()
            .unwrap()
            .as_slice(),
        b"snapshot"
    );
    assert_eq!(
        reopened
            .application_data_storage()
            .get("persistent-key")
            .unwrap(),
        Some(b"value".to_vec())
    );
}

#[test]
fn key_packages_reject_duplicates_and_corruption_is_an_error() {
    let engine = engine();
    let mut storage = engine.key_package_storage();
    storage
        .insert(b"id".to_vec(), key_package(100, 1))
        .unwrap();
    let error = storage
        .insert(b"id".to_vec(), key_package(200, 2))
        .unwrap_err();
    assert_matches!(error, RedbDataStorageError::DuplicateKeyPackage);

    let transaction = engine.database().begin_write().unwrap();
    {
        let mut packages = transaction.open_table(KEY_PACKAGES).unwrap();
        packages.insert(b"corrupt".as_slice(), b"bad".as_slice()).unwrap();
    }
    transaction.commit().unwrap();

    let error = storage.get(b"corrupt").unwrap_err();
    assert_matches!(error, RedbDataStorageError::CorruptRecord(_));

    let transaction = engine.database().begin_write().unwrap();
    {
        let mut packages = transaction.open_table(KEY_PACKAGES).unwrap();
        let mut invalid_codec = vec![0; 8];
        invalid_codec.push(0xff);
        packages
            .insert(b"invalid-codec".as_slice(), invalid_codec.as_slice())
            .unwrap();
    }
    transaction.commit().unwrap();
    let error = storage.get(b"invalid-codec").unwrap_err();
    assert_matches!(error, RedbDataStorageError::MlsCodec(_));
}

#[test]
fn key_packages_get_count_and_delete() {
    let engine = engine();
    let mut storage = engine.key_package_storage();
    let package = key_package(100, 1);
    storage.insert(b"one".to_vec(), package.clone()).unwrap();
    storage
        .insert(b"two".to_vec(), key_package(200, 2))
        .unwrap();
    assert_eq!(storage.get(b"one").unwrap(), Some(package));
    assert_eq!(storage.count().unwrap(), 2);

    storage.delete(b"one").unwrap();
    assert!(storage.get(b"one").unwrap().is_none());
    assert_eq!(storage.count().unwrap(), 1);
    storage.delete(b"not-present").unwrap();
}

#[test]
fn expiration_boundary_and_u64_max_are_supported() {
    let engine = engine();
    let mut storage = engine.key_package_storage();
    storage.insert(vec![1], key_package(29, 1)).unwrap();
    storage.insert(vec![2], key_package(30, 2)).unwrap();
    storage
        .insert(vec![3], key_package(u64::MAX, 3))
        .unwrap();

    storage.delete_expired_by_time(30).unwrap();
    assert!(storage.get(&[1]).unwrap().is_none());
    assert!(storage.get(&[2]).unwrap().is_some());
    assert!(storage.get(&[3]).unwrap().is_some());
    assert_eq!(storage.count_at_time(30).unwrap(), 2);
    assert_eq!(storage.count_at_time(u64::MAX).unwrap(), 1);
}

#[test]
fn expiration_index_inconsistency_is_reported_without_partial_delete() {
    let engine = engine();
    let mut storage = engine.key_package_storage();
    storage.insert(vec![1], key_package(10, 1)).unwrap();
    storage.insert(vec![2], key_package(11, 2)).unwrap();

    let transaction = engine.database().begin_write().unwrap();
    {
        let mut expiry = transaction
            .open_multimap_table(KEY_PACKAGE_EXPIRY)
            .unwrap();
        assert!(expiry.remove(11, [2].as_slice()).unwrap());
    }
    transaction.commit().unwrap();

    let error = storage.delete_expired_by_time(20).unwrap_err();
    assert_matches!(
        error,
        RedbDataStorageError::SecondaryIndexInconsistency(_)
    );

    let transaction = engine.database().begin_read().unwrap();
    let packages = transaction.open_table(KEY_PACKAGES).unwrap();
    assert_eq!(packages.len().unwrap(), 2);
}

#[test]
fn psk_and_application_values_are_upserts_and_clones_share_the_database() {
    let engine = engine();
    let psk_storage = engine.pre_shared_key_storage();
    psk_storage
        .insert(b"psk", &PreSharedKey::new(vec![1, 2, 3]))
        .unwrap();
    psk_storage
        .insert(b"psk", &PreSharedKey::new(vec![4, 5, 6]))
        .unwrap();
    assert_eq!(
        psk_storage.get(b"psk").unwrap(),
        Some(PreSharedKey::new(vec![4, 5, 6]))
    );

    let first = engine.application_data_storage();
    let second = first.clone();
    assert_eq!(first.insert("key", b"one").unwrap(), 1);
    assert_eq!(first.insert("key", b"one").unwrap(), 0);
    assert_eq!(first.insert("key", b"two").unwrap(), 1);
    assert_eq!(second.get("key").unwrap(), Some(b"two".to_vec()));

    psk_storage.delete(b"psk").unwrap();
    assert!(psk_storage.get(b"psk").unwrap().is_none());
    assert_eq!(first.delete("key").unwrap(), 1);
    assert_eq!(first.delete("key").unwrap(), 0);
}

#[test]
fn application_batch_insert_counts_only_changes() {
    use mls_rs_provider_redb::storage::Item;

    let engine = engine();
    let storage = engine.application_data_storage();
    let items = vec![
        Item::new("one".into(), vec![1]),
        Item::new("two".into(), vec![2]),
        Item::new("three".into(), vec![3]),
    ];
    assert_eq!(storage.transact_insert(&items).unwrap(), 3);
    assert_eq!(storage.transact_insert(&items).unwrap(), 0);

    let changed = vec![
        Item::new("one".into(), vec![9]),
        Item::new("four".into(), vec![4]),
    ];
    assert_eq!(storage.transact_insert(&changed).unwrap(), 2);
    assert_eq!(storage.get("one").unwrap(), Some(vec![9]));
}

#[test]
fn application_prefixes_are_literal_unicode_prefixes() {
    let engine = engine();
    let storage = engine.application_data_storage();
    for key in ["%_heart", "%_hearth", "%Xheart", "other", "\u{2764}a", "\u{2764}b"] {
        storage.insert(key, key.as_bytes()).unwrap();
    }

    let keys = storage
        .get_by_prefix("%_heart")
        .unwrap()
        .into_iter()
        .map(|item| item.key)
        .collect::<Vec<_>>();
    assert_eq!(keys, vec!["%_heart", "%_hearth"]);
    assert_eq!(storage.delete_by_prefix("\u{2764}").unwrap(), 2);
    assert!(storage.get_by_prefix("\u{2764}").unwrap().is_empty());
}

#[test]
fn unsupported_schema_versions_are_rejected() {
    let file = NamedTempFile::new().unwrap();
    let database = Database::create(file.path()).unwrap();
    let transaction = database.begin_write().unwrap();
    {
        let mut metadata = transaction.open_table(PROVIDER_METADATA).unwrap();
        metadata.insert("schema_version", 2).unwrap();
    }
    transaction.commit().unwrap();

    let error = RedbDataStorageEngine::new(database).unwrap_err();
    assert_matches!(
        error,
        RedbDataStorageError::UnsupportedSchemaVersion {
            found: 2,
            supported: 1
        }
    );
}

#[test]
fn table_type_mismatches_are_rejected() {
    let file = NamedTempFile::new().unwrap();
    let database = Database::create(file.path()).unwrap();
    let wrong_groups: TableDefinition<u64, u64> = TableDefinition::new("mls_groups");
    let transaction = database.begin_write().unwrap();
    transaction.open_table(wrong_groups).unwrap();
    transaction.commit().unwrap();

    let error = RedbDataStorageEngine::new(database).unwrap_err();
    assert_matches!(error, RedbDataStorageError::Database(_));
}

#[test]
fn initialized_schema_contains_all_primary_tables() {
    let engine = engine();
    let transaction = engine.database().begin_read().unwrap();
    transaction.open_table(GROUPS).unwrap();
    transaction.open_table(EPOCHS).unwrap();
    transaction.open_table(KEY_PACKAGES).unwrap();
}
