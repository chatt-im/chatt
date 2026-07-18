//! Disposable, capability-protected enrollment bundles for linking devices.

use jsony::Jsony;
use rpc::crypto::{
    KEY_LEN, KeyMaterial, derive_device_link_keys, encode_hex, open_in_place_with_aad,
    seal_in_place_append_tag,
};
use zeroize::Zeroizing;

use chatt_mls::E2eBootstrap;
use rpc::identity::SignedDeviceRoster;

const BUNDLE_VERSION: u8 = 3;
const AAD_LABEL: &[u8] = b"chatt disposable device enrollment v3";

pub(crate) struct PairingSecrets {
    pub redemption_secret: Zeroizing<String>,
    pub enrollment_key: Zeroizing<[u8; KEY_LEN]>,
}

#[derive(Clone, Debug, Jsony)]
#[jsony(Binary, version)]
pub(crate) struct MlsEnrollment {
    pub authority_seed: [u8; 32],
    pub current_roster: SignedDeviceRoster,
}

pub(crate) fn seal_enrollment(
    bootstrap: &E2eBootstrap,
    ticket_hash: &[u8],
    enrollment_key: &[u8; KEY_LEN],
) -> Result<Vec<u8>, String> {
    seal_plaintext(
        jsony::to_binary(&MlsEnrollment {
            authority_seed: bootstrap.authority_seed,
            current_roster: bootstrap.own_roster.clone(),
        }),
        &bootstrap.server_public_key,
        ticket_hash,
        enrollment_key,
    )
}

fn seal_plaintext(
    mut plaintext: Vec<u8>,
    server_public_key: &[u8],
    ticket_hash: &[u8],
    enrollment_key: &[u8; KEY_LEN],
) -> Result<Vec<u8>, String> {
    let aad = enrollment_aad(server_public_key, ticket_hash);
    seal_in_place_append_tag(
        &KeyMaterial {
            id: 1,
            bytes: *enrollment_key,
        },
        0,
        &aad,
        0,
        &mut plaintext,
    )
    .map_err(|error| error.to_string())?;
    let mut bundle = Vec::with_capacity(1 + plaintext.len());
    bundle.push(BUNDLE_VERSION);
    bundle.extend_from_slice(&plaintext);
    Ok(bundle)
}

pub(crate) fn open_enrollment(
    bundle: &[u8],
    enrollment_key: &[u8; KEY_LEN],
    server_public_key: &[u8],
    ticket_hash: &[u8],
) -> Result<MlsEnrollment, String> {
    if bundle.len() <= 1 || bundle[0] != BUNDLE_VERSION {
        return Err("device enrollment bundle has an unsupported format".to_string());
    }
    let aad = enrollment_aad(server_public_key, ticket_hash);
    let mut plaintext = Zeroizing::new(bundle[1..].to_vec());
    let len = open_in_place_with_aad(
        &KeyMaterial {
            id: 1,
            bytes: *enrollment_key,
        },
        0,
        &aad,
        &mut plaintext,
    )
    .map_err(|_| {
        "the pairing string is incorrect or the enrollment bundle was altered".to_string()
    })?;
    plaintext.truncate(len);
    jsony::from_binary(&plaintext)
        .map_err(|error| format!("invalid MLS device enrollment bundle: {error}"))
}

pub(crate) fn redemption_secret_hash(secret: &str) -> Vec<u8> {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, secret.as_bytes())
        .as_ref()
        .to_vec()
}

pub(crate) fn derive_pairing_secrets(
    pairing_secret: &[u8; KEY_LEN],
    server_public_key: &[u8],
) -> Result<PairingSecrets, String> {
    let derived = derive_device_link_keys(pairing_secret, server_public_key)
        .map_err(|error| format!("failed to derive device-link keys: {error}"))?;
    Ok(PairingSecrets {
        redemption_secret: Zeroizing::new(encode_hex(&derived.redemption_secret)),
        enrollment_key: Zeroizing::new(derived.enrollment_key),
    })
}

fn enrollment_aad(server_public_key: &[u8], ticket_hash: &[u8]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(AAD_LABEL.len() + server_public_key.len() + ticket_hash.len());
    aad.extend_from_slice(AAD_LABEL);
    aad.extend_from_slice(server_public_key);
    aad.extend_from_slice(ticket_hash);
    aad
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrollment_bundle_requires_the_pairing_capability_and_context() {
        let plaintext = b"ephemeral authority material".to_vec();
        let server_key = [7; 32];
        let ticket_hash = [9; 32];
        let pairing = [5; KEY_LEN];
        let secrets = derive_pairing_secrets(&pairing, &server_key).unwrap();
        let bundle = seal_plaintext(
            plaintext.clone(),
            &server_key,
            &ticket_hash,
            &secrets.enrollment_key,
        )
        .unwrap();

        let mut encrypted = Zeroizing::new(bundle[1..].to_vec());
        let len = open_in_place_with_aad(
            &KeyMaterial {
                id: 1,
                bytes: *secrets.enrollment_key,
            },
            0,
            &enrollment_aad(&server_key, &ticket_hash),
            &mut encrypted,
        )
        .unwrap();
        assert_eq!(&encrypted[..len], plaintext);
        assert!(open_enrollment(&bundle, &[6; KEY_LEN], &server_key, &ticket_hash).is_err());
        assert!(open_enrollment(&bundle, &secrets.enrollment_key, &[8; 32], &ticket_hash).is_err());
    }

    #[test]
    fn pairing_secret_derives_separate_redemption_and_enrollment_keys() {
        let pairing = [11; KEY_LEN];
        let secrets = derive_pairing_secrets(&pairing, &[7; 32]).unwrap();
        assert_ne!(
            secrets.redemption_secret.as_bytes(),
            &*secrets.enrollment_key
        );
        assert_eq!(secrets.redemption_secret.len(), KEY_LEN * 2);
    }
}
