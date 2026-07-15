//! Disposable, password-protected enrollment bundles for linking devices.

use argon2_kdf::{Algorithm, Hasher};
use ring::rand::SecureRandom;
use rpc::crypto::{KEY_LEN, KeyMaterial, open_in_place_with_aad, seal_in_place_append_tag};
use zeroize::Zeroizing;

use crate::e2e_store::LocalE2eIdentity;

const BUNDLE_VERSION: u8 = 2;
const SALT_LEN: usize = 16;
const WORD_COUNT: usize = 6;
const AAD_LABEL: &[u8] = b"chatt disposable device enrollment v2";
const WORDLIST: &str = include_str!("../assets/english.txt");

#[cfg(not(test))]
const KDF_MEMORY_KIB: u32 = 62_500;
#[cfg(test)]
const KDF_MEMORY_KIB: u32 = 32;
#[cfg(not(test))]
const KDF_ITERATIONS: u32 = 18;
#[cfg(test)]
const KDF_ITERATIONS: u32 = 1;

pub(crate) fn seal_enrollment(
    identity: &LocalE2eIdentity,
    ticket_hash: &[u8],
    rng: &dyn SecureRandom,
) -> Result<(Vec<u8>, String), String> {
    seal_plaintext(
        identity.enrollment_authority()?,
        identity.server_public_key(),
        ticket_hash,
        rng,
    )
}

fn seal_plaintext(
    mut plaintext: Vec<u8>,
    server_public_key: &[u8],
    ticket_hash: &[u8],
    rng: &dyn SecureRandom,
) -> Result<(Vec<u8>, String), String> {
    let password = generate_transfer_password(rng)?;
    let mut salt = [0u8; SALT_LEN];
    rng.fill(&mut salt)
        .map_err(|_| "failed to generate enrollment salt".to_string())?;
    let key = derive_key(&password, &salt)?;
    let aad = enrollment_aad(server_public_key, ticket_hash);
    seal_in_place_append_tag(
        &KeyMaterial { id: 1, bytes: *key },
        0,
        &aad,
        0,
        &mut plaintext,
    )
    .map_err(|error| error.to_string())?;
    let mut bundle = Vec::with_capacity(1 + SALT_LEN + plaintext.len());
    bundle.push(BUNDLE_VERSION);
    bundle.extend_from_slice(&salt);
    bundle.extend_from_slice(&plaintext);
    Ok((bundle, password))
}

pub(crate) fn open_enrollment(
    bundle: &[u8],
    password: &str,
    server_public_key: &[u8],
    ticket_hash: &[u8],
) -> Result<Zeroizing<Vec<u8>>, String> {
    if bundle.len() <= 1 + SALT_LEN || bundle[0] != BUNDLE_VERSION {
        return Err("device enrollment bundle has an unsupported format".to_string());
    }
    let salt = &bundle[1..1 + SALT_LEN];
    let key = derive_key(&normalize_password(password), salt)?;
    let aad = enrollment_aad(server_public_key, ticket_hash);
    let mut plaintext = Zeroizing::new(bundle[1 + SALT_LEN..].to_vec());
    let len = open_in_place_with_aad(
        &KeyMaterial { id: 1, bytes: *key },
        0,
        &aad,
        &mut plaintext,
    )
    .map_err(|_| "transfer password is incorrect or the enrollment bundle was altered".to_string())?;
    plaintext.truncate(len);
    Ok(plaintext)
}

pub(crate) fn redemption_secret_hash(secret: &str) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA256, secret.as_bytes())
        .as_ref()
        .to_vec()
}

fn derive_key(password: &str, salt: &[u8]) -> Result<Zeroizing<[u8; KEY_LEN]>, String> {
    let hash = Hasher::new()
        .algorithm(Algorithm::Argon2id)
        .memory_cost_kib(KDF_MEMORY_KIB)
        .iterations(KDF_ITERATIONS)
        .threads(1)
        .hash_length(KEY_LEN as u32)
        .custom_salt(salt)
        .hash(password.as_bytes())
        .map_err(|error| format!("failed to derive enrollment key: {error}"))?;
    let bytes: [u8; KEY_LEN] = hash
        .as_bytes()
        .try_into()
        .map_err(|_| "enrollment KDF returned the wrong key length".to_string())?;
    Ok(Zeroizing::new(bytes))
}

fn enrollment_aad(server_public_key: &[u8], ticket_hash: &[u8]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(AAD_LABEL.len() + server_public_key.len() + ticket_hash.len());
    aad.extend_from_slice(AAD_LABEL);
    aad.extend_from_slice(server_public_key);
    aad.extend_from_slice(ticket_hash);
    aad
}

fn generate_transfer_password(rng: &dyn SecureRandom) -> Result<String, String> {
    let words = WORDLIST.lines().collect::<Vec<_>>();
    debug_assert_eq!(words.len(), 2048);
    let mut selected = Vec::with_capacity(WORD_COUNT);
    while selected.len() < WORD_COUNT {
        let mut bytes = [0u8; 2];
        rng.fill(&mut bytes)
            .map_err(|_| "failed to generate transfer password".to_string())?;
        let sample = u16::from_le_bytes(bytes);
        selected.push(words[usize::from(sample % 2048)]);
    }
    Ok(selected.join("-"))
}

fn normalize_password(password: &str) -> String {
    password
        .split(|character: char| character.is_whitespace() || character == '-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_password_has_six_words() {
        let password = generate_transfer_password(&ring::rand::SystemRandom::new()).unwrap();
        assert_eq!(password.split('-').count(), WORD_COUNT);
    }

    #[test]
    fn enrollment_bundle_requires_the_transfer_password_and_context() {
        let rng = ring::rand::SystemRandom::new();
        let plaintext = b"ephemeral authority material".to_vec();
        let server_key = [7; 32];
        let ticket_hash = [9; 32];
        let (bundle, password) =
            seal_plaintext(plaintext.clone(), &server_key, &ticket_hash, &rng).unwrap();

        assert_eq!(
            open_enrollment(&bundle, &password, &server_key, &ticket_hash)
                .unwrap()
                .as_slice(),
            plaintext
        );
        assert!(open_enrollment(&bundle, "wrong-password", &server_key, &ticket_hash).is_err());
        assert!(open_enrollment(&bundle, &password, &[8; 32], &ticket_hash).is_err());
    }
}
