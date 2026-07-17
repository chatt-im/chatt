// Chatt's fixed MLS suite only needs X25519 key agreement.

use crate::{
    aws_lc_sys_impl::{X25519, X25519_keypair, X25519_public_from_private},
    AwsLcCryptoError,
};
use aws_lc_rs::error::Unspecified;
use mls_rs_core::crypto::{CipherSuite, HpkePublicKey, HpkeSecretKey};
use mls_rs_crypto_traits::{Curve, SamplingMethod};

#[derive(Clone, Copy)]
pub struct Ecdh {
    curve: Curve,
    sampling_method: SamplingMethod,
}

impl Ecdh {
    pub fn new(cipher_suite: CipherSuite) -> Option<Self> {
        let curve = Curve::from_ciphersuite(cipher_suite, false)?;
        (curve == Curve::X25519).then_some(Self {
            curve,
            sampling_method: curve.hpke_sampling_method(),
        })
    }

    pub fn with_sampling_method(self, sampling_method: SamplingMethod) -> Self {
        Self {
            sampling_method,
            ..self
        }
    }
}

#[cfg_attr(not(mls_build_async), maybe_async::must_be_sync)]
#[cfg_attr(all(target_arch = "wasm32", mls_build_async), maybe_async::must_be_async(?Send))]
#[cfg_attr(
    all(not(target_arch = "wasm32"), mls_build_async),
    maybe_async::must_be_async
)]
impl mls_rs_crypto_traits::DhType for Ecdh {
    type Error = AwsLcCryptoError;

    async fn dh(
        &self,
        secret_key: &HpkeSecretKey,
        public_key: &HpkePublicKey,
    ) -> Result<Vec<u8>, Self::Error> {
        x25519(secret_key, public_key)
    }

    async fn generate(&self) -> Result<(HpkeSecretKey, HpkePublicKey), Self::Error> {
        let (secret, public) = x25519_generate()?;
        Ok((secret.into(), public.into()))
    }

    async fn to_public(&self, secret_key: &HpkeSecretKey) -> Result<HpkePublicKey, Self::Error> {
        Ok(x25519_public_key(secret_key)?.into())
    }

    fn bitmask_for_rejection_sampling(&self) -> SamplingMethod {
        self.sampling_method
    }

    fn secret_key_size(&self) -> usize {
        self.curve.secret_key_size()
    }

    fn public_key_validate(&self, _key: &HpkePublicKey) -> Result<(), Self::Error> {
        Ok(())
    }

    fn public_key_size(&self) -> usize {
        self.curve.public_key_size()
    }
}

pub fn x25519(
    secret_key: &HpkeSecretKey,
    public_key: &HpkePublicKey,
) -> Result<Vec<u8>, AwsLcCryptoError> {
    let curve = Curve::X25519;
    (secret_key.len() == curve.secret_key_size() && public_key.len() == curve.public_key_size())
        .then_some(())
        .ok_or(AwsLcCryptoError::InvalidKeyData)?;

    let mut secret = vec![0u8; curve.secret_key_size()];
    let result = unsafe {
        X25519(
            secret.as_mut_ptr(),
            secret_key.as_ptr(),
            public_key.as_ptr(),
        )
    };

    (result == 1).then_some(secret).ok_or(Unspecified.into())
}

pub fn x25519_generate() -> Result<(Vec<u8>, Vec<u8>), AwsLcCryptoError> {
    let curve = Curve::X25519;
    let mut private_key = vec![0u8; curve.secret_key_size()];
    let mut public_key = vec![0u8; curve.public_key_size()];
    unsafe { X25519_keypair(public_key.as_mut_ptr(), private_key.as_mut_ptr()) }
    Ok((private_key, public_key))
}

pub fn x25519_public_key(secret_key: &[u8]) -> Result<Vec<u8>, AwsLcCryptoError> {
    if secret_key.len() != Curve::X25519.secret_key_size() {
        return Err(AwsLcCryptoError::InvalidKeyData);
    }

    let mut public_key = vec![0u8; Curve::X25519.public_key_size()];
    unsafe { X25519_public_from_private(public_key.as_mut_ptr(), secret_key.as_ptr()) }
    Ok(public_key)
}
