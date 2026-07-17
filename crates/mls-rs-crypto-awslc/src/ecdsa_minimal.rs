// Chatt's fixed MLS suite only needs Ed25519 signatures. Keeping this provider
// separate makes the NIST EC entry points unreachable from the final binary.

use std::ops::Deref;

use aws_lc_rs::{
    error::Unspecified,
    signature::{self, VerificationAlgorithm, ED25519_PUBLIC_KEY_LEN},
};
use mls_rs_core::crypto::{CipherSuite, SignaturePublicKey, SignatureSecretKey};
use mls_rs_crypto_traits::Curve;

use crate::{
    aws_lc_sys_impl::{
        ED25519_keypair, ED25519_sign, ED25519_PRIVATE_KEY_LEN, ED25519_SIGNATURE_LEN,
    },
    AwsLcCryptoError,
};

#[derive(Clone)]
pub struct AwsLcEcdsa(pub(crate) Curve);

impl Deref for AwsLcEcdsa {
    type Target = Curve;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AwsLcEcdsa {
    pub fn new(cipher_suite: CipherSuite) -> Option<Self> {
        let curve = Curve::from_ciphersuite(cipher_suite, true)?;
        (curve == Curve::Ed25519).then_some(Self(curve))
    }

    pub fn import_ec_der_private_key(
        &self,
        _bytes: &[u8],
    ) -> Result<SignatureSecretKey, AwsLcCryptoError> {
        Err(AwsLcCryptoError::UnsupportedCipherSuite)
    }

    pub fn import_ec_der_public_key(
        &self,
        _bytes: &[u8],
    ) -> Result<SignaturePublicKey, AwsLcCryptoError> {
        Err(AwsLcCryptoError::UnsupportedCipherSuite)
    }

    pub fn signature_key_generate(
        &self,
    ) -> Result<(SignatureSecretKey, SignaturePublicKey), AwsLcCryptoError> {
        let (secret, public) = ed25519_generate()?;
        Ok((secret.into(), public.into()))
    }

    pub fn signature_key_derive_public(
        &self,
        secret_key: &SignatureSecretKey,
    ) -> Result<SignaturePublicKey, AwsLcCryptoError> {
        Ok(ed25519_public_key(secret_key)?.into())
    }

    pub fn sign(
        &self,
        secret_key: &SignatureSecretKey,
        data: &[u8],
    ) -> Result<Vec<u8>, AwsLcCryptoError> {
        ed25519_sign(secret_key, data)
    }

    pub fn verify(
        &self,
        public_key: &SignaturePublicKey,
        signature: &[u8],
        data: &[u8],
    ) -> Result<(), AwsLcCryptoError> {
        signature::ED25519
            .verify_sig(public_key.as_ref(), data, signature)
            .map_err(|_| AwsLcCryptoError::InvalidSignature)
    }
}

fn ed25519_sign(secret_key: &SignatureSecretKey, data: &[u8]) -> Result<Vec<u8>, AwsLcCryptoError> {
    (secret_key.len() == ED25519_PRIVATE_KEY_LEN as usize)
        .then_some(())
        .ok_or(AwsLcCryptoError::InvalidKeyData)?;

    let mut signature = vec![0u8; ED25519_SIGNATURE_LEN as usize];
    let result = unsafe {
        ED25519_sign(
            signature.as_mut_ptr(),
            data.as_ptr(),
            data.len(),
            secret_key.as_ptr(),
        )
    };

    (result == 1).then_some(signature).ok_or(Unspecified.into())
}

fn ed25519_generate() -> Result<(Vec<u8>, Vec<u8>), AwsLcCryptoError> {
    let mut private_key = vec![0u8; ED25519_PRIVATE_KEY_LEN as usize];
    let mut public_key = vec![0u8; ED25519_PUBLIC_KEY_LEN];
    unsafe { ED25519_keypair(public_key.as_mut_ptr(), private_key.as_mut_ptr()) }
    Ok((private_key, public_key))
}

fn ed25519_public_key(secret_key: &SignatureSecretKey) -> Result<Vec<u8>, AwsLcCryptoError> {
    (secret_key.len() == 2 * ED25519_PUBLIC_KEY_LEN)
        .then_some(secret_key[ED25519_PUBLIC_KEY_LEN..].to_vec())
        .ok_or(AwsLcCryptoError::InvalidKeyData)
}
