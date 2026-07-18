use std::hint::black_box;

use aws_lc_rs::{
    aead::{self, Aad, LessSafeKey, Nonce, UnboundKey},
    agreement, digest, hkdf, hmac,
    rand::SystemRandom,
    signature::{self, KeyPair},
};
use jsony_bench::{Bench, BenchParameters, DEFAULT_BURN_IN_SAMPLES};

const BENCH_PARAMS: BenchParameters = BenchParameters {
    sample_target_duration_ns: 2_000_000,
    max_sample_iterations: 250_000,
    min_sample_iterations: 1,
    max_samples: 64,
    min_samples: 20,
    target_duration_ns: 150_000_000,
    burn_in_samples: DEFAULT_BURN_IN_SAMPLES,
};
const PAYLOAD_SIZES: [&str; 5] = ["64", "256", "1200", "16384", "196608"];
const HASH_SIZES: [&str; 4] = ["64", "1200", "16384", "196608"];
const SIGNATURE_MESSAGE_SIZES: [&str; 2] = ["64", "1200"];
const PROFILE_PAYLOAD_SIZE: &str = "1200";
const PROFILE_HASH_SIZE: &str = "1200";
const PROFILE_SIGNATURE_MESSAGE_SIZE: &str = "64";
const SYMMETRIC_PROFILE_ITERATIONS: u64 = 500_000;
const HASH_PROFILE_ITERATIONS: u64 = 1_000_000;
const PUBLIC_KEY_PROFILE_ITERATIONS: u64 = 20_000;

pub(super) fn bench_crypto(bench: &mut Bench<'_>) {
    let mut bench = bench.with_parameters(BENCH_PARAMS);

    bench_aead(&mut bench, "chacha20_poly1305", &aead::CHACHA20_POLY1305);
    bench_aead(&mut bench, "aes_128_gcm", &aead::AES_128_GCM);
    bench_hash(&mut bench);
    bench_hmac(&mut bench);
    bench_hkdf(&mut bench);
    bench_x25519(&mut bench);
    bench_ed25519(&mut bench);
}

fn bench_aead(bench: &mut Bench<'_>, name: &str, algorithm: &'static aead::Algorithm) {
    let key_bytes = vec![0x42; algorithm.key_len()];

    bench
        .named(name)
        .named("seal")
        .with_profile_iterations(SYMMETRIC_PROFILE_ITERATIONS)
        .param_str_profile_defaults(
            "bytes",
            PAYLOAD_SIZES,
            [PROFILE_PAYLOAD_SIZE],
            |bench, size| {
                let size = parse_size(&size);
                let measured_key = key_bytes.clone();
                let mut counter = 0u64;

                bench.generated(
                    move || {
                        let nonce = nonce_from_counter(counter);
                        counter = counter.wrapping_add(1);
                        (nonce, vec![0x5a; size])
                    },
                    move |(nonce, mut payload)| {
                        let key = LessSafeKey::new(
                            UnboundKey::new(algorithm, black_box(&measured_key)).unwrap(),
                        );
                        let tag = key
                            .seal_in_place_separate_tag(
                                nonce,
                                Aad::from(black_box(&b"chatt benchmark aad"[..])),
                                black_box(&mut payload),
                            )
                            .unwrap();
                        black_box(payload);
                        let _ = black_box(tag);
                    },
                );
            },
        );

    bench
        .named(name)
        .named("open")
        .with_profile_iterations(SYMMETRIC_PROFILE_ITERATIONS)
        .param_str_profile_defaults(
            "bytes",
            PAYLOAD_SIZES,
            [PROFILE_PAYLOAD_SIZE],
            |bench, size| {
                let size = parse_size(&size);
                let generator_key = key_bytes.clone();
                let measured_key = key_bytes.clone();
                let mut counter = 0u64;

                bench.generated(
                    move || {
                        let nonce_bytes = nonce_bytes_from_counter(counter);
                        counter = counter.wrapping_add(1);
                        let key =
                            LessSafeKey::new(UnboundKey::new(algorithm, &generator_key).unwrap());
                        let mut payload = vec![0x5a; size];
                        key.seal_in_place_append_tag(
                            Nonce::assume_unique_for_key(nonce_bytes),
                            Aad::from(&b"chatt benchmark aad"[..]),
                            &mut payload,
                        )
                        .unwrap();
                        (nonce_bytes, payload)
                    },
                    move |(nonce_bytes, mut payload)| {
                        let key = LessSafeKey::new(
                            UnboundKey::new(algorithm, black_box(&measured_key)).unwrap(),
                        );
                        let plaintext = key
                            .open_in_place(
                                Nonce::assume_unique_for_key(nonce_bytes),
                                Aad::from(black_box(&b"chatt benchmark aad"[..])),
                                black_box(&mut payload),
                            )
                            .unwrap();
                        black_box(plaintext);
                    },
                );
            },
        );
}

fn bench_hash(bench: &mut Bench<'_>) {
    bench
        .named("sha256")
        .with_profile_iterations(HASH_PROFILE_ITERATIONS)
        .param_str_profile_defaults("bytes", HASH_SIZES, [PROFILE_HASH_SIZE], |bench, size| {
            let input = vec![0xa5; parse_size(&size)];
            bench.func(move || {
                black_box(digest::digest(&digest::SHA256, black_box(&input)));
            });
        });
}

fn bench_hmac(bench: &mut Bench<'_>) {
    bench
        .named("hmac_sha256")
        .with_profile_iterations(HASH_PROFILE_ITERATIONS)
        .param_str_profile_defaults("bytes", HASH_SIZES, [PROFILE_HASH_SIZE], |bench, size| {
            let input = vec![0xa5; parse_size(&size)];
            let key = hmac::Key::new(hmac::HMAC_SHA256, &[0x42; 32]);
            bench.func(move || {
                black_box(hmac::sign(&key, black_box(&input)));
            });
        });
}

fn bench_hkdf(bench: &mut Bench<'_>) {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, &[0x24; 32]);
    let input_key_material = [0x42; 32];
    let mut output = [0u8; 32];

    bench
        .named("hkdf_sha256_extract_expand_32")
        .with_profile_iterations(HASH_PROFILE_ITERATIONS)
        .func(move || {
            let prk = salt.extract(black_box(&input_key_material));
            let info = [black_box(&b"chatt benchmark context"[..])];
            let okm = prk.expand(&info, hkdf::HKDF_SHA256).unwrap();
            okm.fill(black_box(&mut output)).unwrap();
            black_box(output);
        });
}

fn bench_x25519(bench: &mut Bench<'_>) {
    let rng = SystemRandom::new();

    bench
        .named("x25519")
        .named("generate_public")
        .with_profile_iterations(PUBLIC_KEY_PROFILE_ITERATIONS)
        .func(move || {
            let private =
                agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng).unwrap();
            black_box(private.compute_public_key().unwrap());
        });

    let rng = SystemRandom::new();
    let peer_private = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng).unwrap();
    let peer_public = peer_private.compute_public_key().unwrap().as_ref().to_vec();

    bench
        .named("x25519")
        .named("agree")
        .with_profile_iterations(PUBLIC_KEY_PROFILE_ITERATIONS)
        .generated(
            move || agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng).unwrap(),
            move |private| {
                let peer = agreement::UnparsedPublicKey::new(&agreement::X25519, &peer_public);
                agreement::agree_ephemeral(private, &peer, (), |secret| {
                    black_box(secret);
                    Ok(())
                })
                .unwrap();
            },
        );
}

fn bench_ed25519(bench: &mut Bench<'_>) {
    const SEED: [u8; 32] = [0x42; 32];

    bench
        .named("ed25519")
        .named("key_from_seed")
        .with_profile_iterations(PUBLIC_KEY_PROFILE_ITERATIONS)
        .func(|| {
            let key_pair =
                signature::Ed25519KeyPair::from_seed_unchecked(black_box(&SEED)).unwrap();
            black_box(key_pair.public_key());
        });

    bench
        .named("ed25519")
        .named("sign")
        .with_profile_iterations(PUBLIC_KEY_PROFILE_ITERATIONS)
        .param_str_profile_defaults(
            "bytes",
            SIGNATURE_MESSAGE_SIZES,
            [PROFILE_SIGNATURE_MESSAGE_SIZE],
            |bench, size| {
                let message = vec![0xa5; parse_size(&size)];
                let key_pair = signature::Ed25519KeyPair::from_seed_unchecked(&SEED).unwrap();
                bench.func(move || {
                    black_box(key_pair.sign(black_box(&message)));
                });
            },
        );

    bench
        .named("ed25519")
        .named("verify")
        .with_profile_iterations(PUBLIC_KEY_PROFILE_ITERATIONS)
        .param_str_profile_defaults(
            "bytes",
            SIGNATURE_MESSAGE_SIZES,
            [PROFILE_SIGNATURE_MESSAGE_SIZE],
            |bench, size| {
                let message = vec![0xa5; parse_size(&size)];
                let key_pair = signature::Ed25519KeyPair::from_seed_unchecked(&SEED).unwrap();
                let public_key = key_pair.public_key().as_ref().to_vec();
                let signed = key_pair.sign(&message).as_ref().to_vec();
                bench.func(move || {
                    signature::UnparsedPublicKey::new(&signature::ED25519, &public_key)
                        .verify(black_box(&message), black_box(&signed))
                        .unwrap();
                });
            },
        );
}

fn nonce_from_counter(counter: u64) -> Nonce {
    Nonce::assume_unique_for_key(nonce_bytes_from_counter(counter))
}

fn nonce_bytes_from_counter(counter: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&counter.to_le_bytes());
    nonce
}

fn parse_size(value: &str) -> usize {
    value.parse().unwrap()
}
