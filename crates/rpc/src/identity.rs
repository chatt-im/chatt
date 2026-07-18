//! Stable Chatt account authority and current device-roster types.
//!
//! MLS authenticates leaves inside a group. These types bind those leaves to
//! Chatt accounts and are deliberately independent of MLS serialization.

use aws_lc_rs::{
    digest,
    signature::{self, KeyPair, VerificationAlgorithm},
};
use jsony::Jsony;

use crate::ids::{AccountId, DeviceId, UserId};

pub const AUTHORITY_KEY_LEN: usize = 32;
pub const SIGNATURE_KEY_LEN: usize = 32;
pub const SIGNATURE_LEN: usize = 64;
pub const MAX_ACTIVE_DEVICES: usize = 16;
pub const MAX_DEVICE_NAME_BYTES: usize = 64;
pub const ACCOUNT_ID_LEN: usize = 32;
pub const MAX_MLS_CLIENT_ID_BYTES: usize = 256;

const ACCOUNT_ID_LABEL: &[u8] = b"chatt account identity v1";
const CLIENT_ID_LABEL: &[u8] = b"chatt mls client id v1";
const CERTIFICATE_SIGNATURE_LABEL: &[u8] = b"chatt device certificate v1";
const ROSTER_SIGNATURE_LABEL: &[u8] = b"chatt device roster v1";
const DEVICE_BINDING_LABEL: &[u8] = b"chatt mls device binding v1";

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct DeviceCertificateBody {
    pub user_id: UserId,
    pub account_id: AccountId,
    pub authority_public_key: [u8; AUTHORITY_KEY_LEN],
    pub device_id: DeviceId,
    pub device_name: String,
    pub mls_client_id: Vec<u8>,
    pub mls_signature_public_key: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct SignedDeviceCertificate {
    pub body: DeviceCertificateBody,
    pub authority_signature: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct DeviceRosterBody {
    pub user_id: UserId,
    pub account_id: AccountId,
    pub authority_public_key: [u8; AUTHORITY_KEY_LEN],
    pub revision: u64,
    pub active_devices: Vec<SignedDeviceCertificate>,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct SignedDeviceRoster {
    pub body: DeviceRosterBody,
    pub authority_signature: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Jsony)]
#[jsony(Binary, version)]
pub struct RosterCheckpoint {
    pub account_id: AccountId,
    pub revision: u64,
    pub digest: [u8; 32],
}

pub fn account_id(
    server_public_key: &[u8],
    user_id: UserId,
    authority_public_key: &[u8; AUTHORITY_KEY_LEN],
) -> AccountId {
    let mut input = Vec::with_capacity(
        ACCOUNT_ID_LABEL.len() + server_public_key.len() + 8 + authority_public_key.len(),
    );
    input.extend_from_slice(ACCOUNT_ID_LABEL);
    input.extend_from_slice(server_public_key);
    input.extend_from_slice(&user_id.0.to_le_bytes());
    input.extend_from_slice(authority_public_key);
    AccountId(sha256(&input))
}

/// Canonical, length-delimited Basic credential identity used by MLS.
pub fn mls_client_id(
    server_id: &[u8],
    account_id: AccountId,
    device_id: DeviceId,
) -> Result<Vec<u8>, String> {
    let server_len = u16::try_from(server_id.len())
        .map_err(|_| "server identity is too long for an MLS client id".to_string())?;
    let mut id = Vec::with_capacity(CLIENT_ID_LABEL.len() + 2 + server_id.len() + 32 + 16);
    id.extend_from_slice(CLIENT_ID_LABEL);
    id.extend_from_slice(&server_len.to_be_bytes());
    id.extend_from_slice(server_id);
    id.extend_from_slice(&account_id.0);
    id.extend_from_slice(&device_id.0);
    if id.len() > MAX_MLS_CLIENT_ID_BYTES {
        return Err("MLS client id is too long".to_string());
    }
    Ok(id)
}

/// Parses the account and device routing identity embedded in a canonical MLS
/// Basic credential. Certification is still required when adding a leaf; this
/// parser is for attributing an already-authenticated historical group member.
pub fn parse_mls_client_id(
    server_id: &[u8],
    client_id: &[u8],
) -> Result<(AccountId, DeviceId), String> {
    let header_len = CLIENT_ID_LABEL.len() + 2;
    let Some(header) = client_id.get(..header_len) else {
        return Err("MLS client id is truncated".to_string());
    };
    if !header.starts_with(CLIENT_ID_LABEL) {
        return Err("MLS client id has the wrong domain".to_string());
    }
    let encoded_server_len = u16::from_be_bytes(
        header[CLIENT_ID_LABEL.len()..]
            .try_into()
            .map_err(|_| "MLS client id has an invalid server length".to_string())?,
    ) as usize;
    let expected_len = header_len
        .checked_add(encoded_server_len)
        .and_then(|len| len.checked_add(ACCOUNT_ID_LEN + 16))
        .ok_or_else(|| "MLS client id length overflow".to_string())?;
    if client_id.len() != expected_len || encoded_server_len != server_id.len() {
        return Err("MLS client id has an invalid length".to_string());
    }
    let encoded_server = &client_id[header_len..header_len + encoded_server_len];
    if encoded_server != server_id {
        return Err("MLS client id belongs to another server".to_string());
    }
    let account_start = header_len + encoded_server_len;
    let account = AccountId(
        client_id[account_start..account_start + ACCOUNT_ID_LEN]
            .try_into()
            .map_err(|_| "MLS client id account is truncated".to_string())?,
    );
    let device = DeviceId(
        client_id[account_start + ACCOUNT_ID_LEN..]
            .try_into()
            .map_err(|_| "MLS client id device is truncated".to_string())?,
    );
    Ok((account, device))
}

pub fn authority_public_key(seed: &[u8; AUTHORITY_KEY_LEN]) -> Result<[u8; 32], String> {
    let key = signature::Ed25519KeyPair::from_seed_unchecked(seed)
        .map_err(|_| "account authority seed is invalid".to_string())?;
    Ok(key.public_key().as_ref().try_into().unwrap())
}

pub fn sign_device_certificate(
    body: DeviceCertificateBody,
    authority_seed: &[u8; AUTHORITY_KEY_LEN],
) -> Result<SignedDeviceCertificate, String> {
    let key = authority_key(authority_seed)?;
    let signature = key.sign(&signing_bytes(CERTIFICATE_SIGNATURE_LABEL, &body));
    Ok(SignedDeviceCertificate {
        body,
        authority_signature: signature.as_ref().to_vec(),
    })
}

pub fn validate_device_certificate(
    certificate: &SignedDeviceCertificate,
    server_id: &[u8],
    user_id: UserId,
    account_id: AccountId,
    authority_public_key: &[u8; AUTHORITY_KEY_LEN],
) -> Result<(), String> {
    let body = &certificate.body;
    if body.user_id != user_id
        || body.account_id != account_id
        || &body.authority_public_key != authority_public_key
    {
        return Err("device certificate account context does not match".to_string());
    }
    if account_id != self::account_id(server_id, user_id, authority_public_key) {
        return Err("device certificate account id is invalid".to_string());
    }
    validate_device_name(&body.device_name)?;
    if body.mls_client_id != mls_client_id(server_id, account_id, body.device_id)? {
        return Err("device certificate MLS client id is invalid".to_string());
    }
    if body.mls_signature_public_key.len() != SIGNATURE_KEY_LEN {
        return Err("MLS signature public key has the wrong length".to_string());
    }
    verify(
        authority_public_key,
        &signing_bytes(CERTIFICATE_SIGNATURE_LABEL, body),
        &certificate.authority_signature,
        "device certificate",
    )
}

pub fn sign_device_roster(
    body: DeviceRosterBody,
    authority_seed: &[u8; AUTHORITY_KEY_LEN],
) -> Result<SignedDeviceRoster, String> {
    let key = authority_key(authority_seed)?;
    let signature = key.sign(&signing_bytes(ROSTER_SIGNATURE_LABEL, &body));
    Ok(SignedDeviceRoster {
        body,
        authority_signature: signature.as_ref().to_vec(),
    })
}

pub fn validate_device_roster(
    roster: &SignedDeviceRoster,
    server_id: &[u8],
    user_id: UserId,
) -> Result<(), String> {
    let body = &roster.body;
    if body.user_id != user_id {
        return Err("device roster user does not match".to_string());
    }
    if body.account_id != account_id(server_id, user_id, &body.authority_public_key) {
        return Err("device roster account id is invalid".to_string());
    }
    if body.revision == 0 {
        return Err("device roster revision is zero".to_string());
    }
    if body.active_devices.is_empty() || body.active_devices.len() > MAX_ACTIVE_DEVICES {
        return Err("device roster has an invalid active device count".to_string());
    }

    let mut previous = None;
    let mut client_ids: Vec<&[u8]> = Vec::with_capacity(body.active_devices.len());
    let mut signature_keys: Vec<&[u8]> = Vec::with_capacity(body.active_devices.len());
    for certificate in &body.active_devices {
        validate_device_certificate(
            certificate,
            server_id,
            user_id,
            body.account_id,
            &body.authority_public_key,
        )?;
        if previous.is_some_and(|device_id| device_id >= certificate.body.device_id) {
            return Err("device roster is not uniquely sorted by device id".to_string());
        }
        previous = Some(certificate.body.device_id);
        if client_ids
            .iter()
            .any(|known| *known == certificate.body.mls_client_id.as_slice())
        {
            return Err("device roster contains a duplicate MLS client id".to_string());
        }
        if signature_keys
            .iter()
            .any(|known| *known == certificate.body.mls_signature_public_key.as_slice())
        {
            return Err("device roster contains a duplicate MLS signature key".to_string());
        }
        client_ids.push(&certificate.body.mls_client_id);
        signature_keys.push(&certificate.body.mls_signature_public_key);
    }

    verify(
        &body.authority_public_key,
        &signing_bytes(ROSTER_SIGNATURE_LABEL, body),
        &roster.authority_signature,
        "device roster",
    )
}

pub fn roster_checkpoint(roster: &SignedDeviceRoster) -> RosterCheckpoint {
    RosterCheckpoint {
        account_id: roster.body.account_id,
        revision: roster.body.revision,
        digest: sha256(&jsony::to_binary(roster)),
    }
}

/// Session-bound proof input used to establish that the authenticated
/// transport currently possesses an active MLS credential signing key.
pub fn mls_device_binding_message(
    session_id: crate::ids::SessionId,
    device_id: DeviceId,
    roster: RosterCheckpoint,
) -> Vec<u8> {
    let mut message = Vec::with_capacity(DEVICE_BINDING_LABEL.len() + 8 + 16 + 32 + 8 + 32);
    message.extend_from_slice(DEVICE_BINDING_LABEL);
    message.extend_from_slice(&session_id.0.to_be_bytes());
    message.extend_from_slice(&device_id.0);
    message.extend_from_slice(&roster.account_id.0);
    message.extend_from_slice(&roster.revision.to_be_bytes());
    message.extend_from_slice(&roster.digest);
    message
}

/// Validates an exact compare-and-swap transition to the next current roster.
pub fn validate_roster_transition(
    current: Option<&SignedDeviceRoster>,
    next: &SignedDeviceRoster,
    expected: Option<RosterCheckpoint>,
    server_id: &[u8],
    user_id: UserId,
) -> Result<(), String> {
    validate_device_roster(next, server_id, user_id)?;
    match current {
        None => {
            if expected.is_some() || next.body.revision != 1 {
                return Err("initial device roster must be revision one".to_string());
            }
        }
        Some(current) => {
            validate_device_roster(current, server_id, user_id)?;
            if expected != Some(roster_checkpoint(current)) {
                return Err("device roster checkpoint is stale".to_string());
            }
            if next.body.account_id != current.body.account_id
                || next.body.authority_public_key != current.body.authority_public_key
            {
                return Err("device roster authority changed".to_string());
            }
            let expected_revision = current
                .body
                .revision
                .checked_add(1)
                .ok_or_else(|| "device roster revision overflow".to_string())?;
            if next.body.revision != expected_revision {
                return Err("device roster revision is not the next revision".to_string());
            }
        }
    }
    Ok(())
}

fn validate_device_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > MAX_DEVICE_NAME_BYTES
        || name.trim() != name
        || name.chars().any(char::is_control)
    {
        return Err(
            "device name must be trimmed, 1-64 bytes, and contain no control characters"
                .to_string(),
        );
    }
    Ok(())
}

fn authority_key(seed: &[u8; AUTHORITY_KEY_LEN]) -> Result<signature::Ed25519KeyPair, String> {
    signature::Ed25519KeyPair::from_seed_unchecked(seed)
        .map_err(|_| "account authority seed is invalid".to_string())
}

fn signing_bytes<T: jsony::ToBinary>(label: &[u8], value: &T) -> Vec<u8> {
    let encoded = jsony::to_binary(value);
    let mut bytes = Vec::with_capacity(label.len() + encoded.len());
    bytes.extend_from_slice(label);
    bytes.extend_from_slice(&encoded);
    bytes
}

fn verify(
    public_key: &[u8; 32],
    message: &[u8],
    signature_bytes: &[u8],
    kind: &str,
) -> Result<(), String> {
    if signature_bytes.len() != SIGNATURE_LEN {
        return Err(format!("{kind} signature has the wrong length"));
    }
    signature::ED25519
        .verify_sig(public_key, message, signature_bytes)
        .map_err(|_| format!("{kind} signature is invalid"))
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    digest::digest(&digest::SHA256, bytes)
        .as_ref()
        .try_into()
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn certificate(
        server: &[u8],
        user_id: UserId,
        seed: &[u8; 32],
        device_byte: u8,
        signature_byte: u8,
    ) -> SignedDeviceCertificate {
        let authority_public_key = authority_public_key(seed).unwrap();
        let account_id = account_id(server, user_id, &authority_public_key);
        sign_device_certificate(
            DeviceCertificateBody {
                user_id,
                account_id,
                authority_public_key,
                device_id: DeviceId([device_byte; 16]),
                device_name: format!("device {device_byte}"),
                mls_client_id: mls_client_id(server, account_id, DeviceId([device_byte; 16]))
                    .unwrap(),
                mls_signature_public_key: vec![signature_byte; 32],
            },
            seed,
        )
        .unwrap()
    }

    #[test]
    fn roster_requires_canonical_unique_devices_and_exact_cas() {
        let server = b"server";
        let user_id = UserId(7);
        let seed = [9; 32];
        let authority_public_key = authority_public_key(&seed).unwrap();
        let account_id = account_id(server, user_id, &authority_public_key);
        let first = sign_device_roster(
            DeviceRosterBody {
                user_id,
                account_id,
                authority_public_key,
                revision: 1,
                active_devices: vec![certificate(server, user_id, &seed, 1, 11)],
            },
            &seed,
        )
        .unwrap();
        validate_roster_transition(None, &first, None, server, user_id).unwrap();

        let second = sign_device_roster(
            DeviceRosterBody {
                revision: 2,
                active_devices: vec![
                    certificate(server, user_id, &seed, 1, 11),
                    certificate(server, user_id, &seed, 2, 12),
                ],
                ..first.body.clone()
            },
            &seed,
        )
        .unwrap();
        validate_roster_transition(
            Some(&first),
            &second,
            Some(roster_checkpoint(&first)),
            server,
            user_id,
        )
        .unwrap();
        assert!(
            validate_roster_transition(Some(&first), &second, None, server, user_id)
                .unwrap_err()
                .contains("stale")
        );
    }

    #[test]
    fn canonical_mls_client_id_round_trips_and_is_server_scoped() {
        let account = AccountId([7; 32]);
        let device = DeviceId([8; 16]);
        let encoded = mls_client_id(b"server-a", account, device).unwrap();
        assert_eq!(
            parse_mls_client_id(b"server-a", &encoded).unwrap(),
            (account, device)
        );
        assert!(parse_mls_client_id(b"server-b", &encoded).is_err());
        assert!(parse_mls_client_id(b"server-a", &encoded[..encoded.len() - 1]).is_err());
    }

    #[test]
    fn certificate_binds_client_id_and_mls_signature_key() {
        let server = b"server";
        let user_id = UserId(7);
        let seed = [3; 32];
        let mut certificate = certificate(server, user_id, &seed, 1, 4);
        let authority_public_key = authority_public_key(&seed).unwrap();
        let account_id = account_id(server, user_id, &authority_public_key);
        validate_device_certificate(
            &certificate,
            server,
            user_id,
            account_id,
            &authority_public_key,
        )
        .unwrap();
        certificate.body.mls_signature_public_key[0] ^= 1;
        assert!(
            validate_device_certificate(
                &certificate,
                server,
                user_id,
                account_id,
                &authority_public_key,
            )
            .is_err()
        );
    }
}
