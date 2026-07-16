use mls_rs::{
    client_builder::MlsConfig,
    identity::{basic::{BasicCredential, BasicIdentityProvider}, SigningIdentity},
    CipherSuite, CipherSuiteProvider, Client, CryptoProvider,
};
use mls_rs_core::crypto::SignatureSecretKey;
use mls_rs_crypto_rustcrypto::RustCryptoProvider;
use mls_rs_provider_redb::RedbDataStorageEngine;
use tempfile::tempdir;

fn client(
    engine: &RedbDataStorageEngine,
    identity: SigningIdentity,
    secret_key: SignatureSecretKey,
) -> Client<impl MlsConfig> {
    Client::builder()
        .crypto_provider(RustCryptoProvider::default())
        .identity_provider(BasicIdentityProvider::new())
        .key_package_repo(engine.key_package_storage())
        .psk_store(engine.pre_shared_key_storage())
        .group_state_storage(engine.group_state_storage())
        .signing_identity(identity, secret_key, CipherSuite::CURVE25519_AES128)
        .build()
}

#[test]
fn client_can_persist_reopen_and_resume_a_group() {
    let directory = tempdir().unwrap();
    let path = directory.path().join("client.redb");
    let crypto = RustCryptoProvider::default();
    let suite = crypto
        .cipher_suite_provider(CipherSuite::CURVE25519_AES128)
        .unwrap();
    let (secret_key, public_key) = suite.signature_key_generate().unwrap();
    let identity = SigningIdentity::new(
        BasicCredential::new(b"alice".to_vec()).into_credential(),
        public_key,
    );
    let group_id = b"persistent-mls-group".to_vec();
    let original_authenticator;

    {
        let engine = RedbDataStorageEngine::open(&path).unwrap();
        let client = client(&engine, identity.clone(), secret_key.clone());
        let mut group = client
            .create_group_with_id(
                group_id.clone(),
                Default::default(),
                Default::default(),
                None,
            )
            .unwrap();
        original_authenticator = group.epoch_authenticator().unwrap();
        group.write_to_storage().unwrap();
    }

    {
        let engine = RedbDataStorageEngine::open(&path).unwrap();
        let client = client(&engine, identity.clone(), secret_key.clone());
        let mut group = client.load_group(&group_id).unwrap();
        assert!(group.epoch_authenticator().unwrap() == original_authenticator);
        group
            .encrypt_application_message(b"message after reopen", Vec::new())
            .unwrap();
        group.write_to_storage().unwrap();
    }

    let engine = RedbDataStorageEngine::open(&path).unwrap();
    let client = client(&engine, identity, secret_key);
    let group = client.load_group(&group_id).unwrap();
    assert_eq!(group.group_id(), group_id);
}
