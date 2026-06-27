use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::env;
use std::path::{Path, PathBuf};

const DRED_MODEL_SHA256: &str = "a5177ec6fb7d15058e99e57029746100121f68e4890b1467d4094aa336b6013e";
const DRED_MODEL_ARCHIVE: &str =
    "opus_data-a5177ec6fb7d15058e99e57029746100121f68e4890b1467d4094aa336b6013e.tar.gz";

const BUNDLED_PACKET_OPS_FINGERPRINTS: &[SourceFingerprint] = &[
    SourceFingerprint {
        path: "src/opus.c",
        len: 11_274,
        sha256: "1cee175636295bbd576ff384b77c31663b04fe09619598a452e067229205ade1",
    },
    SourceFingerprint {
        path: "src/repacketizer.c",
        len: 13_751,
        sha256: "f05394de7387cd1264cad6ab77bfeacbc126ebce5bae41ef8a63fd13380a35af",
    },
    SourceFingerprint {
        path: "src/extensions.c",
        len: 23_462,
        sha256: "82a896f6067d32cbb489b694f2e90f2dec8208f0a68ca40e1d123e312a1408b5",
    },
];

struct BuildOptions {
    use_system_lib: bool,
    dred_enabled: bool,
    presume_avx: bool,
    target_arch: String,
    avx_allowed: bool,
    msvc_runtime: Option<MsvcRuntime>,
}

impl BuildOptions {
    fn from_env() -> Self {
        let use_system_lib = env::var("CARGO_FEATURE_SYSTEM_LIB").is_ok();
        let dred_enabled = env::var("CARGO_FEATURE_DRED").is_ok();
        let presume_avx = env::var("CARGO_FEATURE_PRESUME_AVX2").is_ok();
        let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
        let avx_allowed = presume_avx && matches!(target_arch.as_str(), "x86" | "x86_64");
        let msvc_runtime = MsvcRuntime::from_cargo();

        Self {
            use_system_lib,
            dred_enabled,
            presume_avx,
            target_arch,
            avx_allowed,
            msvc_runtime,
        }
    }
}

#[derive(Clone, Copy)]
enum MsvcRuntime {
    Dynamic,
    Static,
}

#[derive(Clone, Copy)]
struct PacketOpsCompatibility {
    rust_packet_ops: bool,
    frame_bounded_extensions: bool,
}

#[derive(Clone, Copy)]
struct SourceFingerprint {
    path: &'static str,
    len: u64,
    sha256: &'static str,
}

impl MsvcRuntime {
    fn from_cargo() -> Option<Self> {
        if !target_is_windows_msvc() {
            return None;
        }

        let uses_static_runtime = target_feature_enabled("crt-static");

        Some(if uses_static_runtime {
            Self::Static
        } else {
            Self::Dynamic
        })
    }

    fn is_static(self) -> bool {
        matches!(self, Self::Static)
    }

    fn opus_static_runtime(self) -> &'static str {
        match self {
            Self::Dynamic => "OFF",
            Self::Static => "ON",
        }
    }
}

fn main() {
    emit_rerun_directives();
    let opts = BuildOptions::from_env();

    if opts.use_system_lib {
        println!("cargo:rustc-cfg=opus_codec_system_lib");
    }

    if opts.use_system_lib {
        handle_system_lib(&opts);
    } else {
        build_bundled_and_link(&opts);
    }

    generate_bindings();
}

fn emit_rerun_directives() {
    println!("cargo:rustc-check-cfg=cfg(opus_codec_system_lib)");
    println!("cargo:rustc-check-cfg=cfg(opus_codec_rust_packet_ops)");
    println!("cargo:rustc-check-cfg=cfg(opus_codec_frame_bounded_extensions)");
    println!("cargo:rerun-if-changed=opus/include/opus.h");
    println!("cargo:rerun-if-changed=opus/include/opus_defines.h");
    println!("cargo:rerun-if-changed=opus/include/opus_types.h");
    println!("cargo:rerun-if-changed=opus/include/opus_multistream.h");
    println!("cargo:rerun-if-changed=opus/include/opus_projection.h");
    println!("cargo:rerun-if-changed=opus/src/opus.c");
    println!("cargo:rerun-if-changed=opus/src/repacketizer.c");
    println!("cargo:rerun-if-changed=opus/src/extensions.c");
    println!("cargo:rerun-if-changed=build.rs");
    let cached_dred_archive = Path::new("opus").join(DRED_MODEL_ARCHIVE);
    if cached_dred_archive.exists() {
        println!("cargo:rerun-if-changed={}", cached_dred_archive.display());
    }
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SYSTEM_LIB");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_PRESUME_AVX2");
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_ENV");
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_FAMILY");
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_FEATURE");
}

fn handle_system_lib(opts: &BuildOptions) {
    if opts.dred_enabled {
        println!(
            "cargo:warning=system-lib feature enabled; ensure the system libopus includes DRED support"
        );
    }
    if opts.presume_avx {
        println!(
            "cargo:warning=presume-avx2 feature enabled; ensure the system libopus was built with OPUS_X86_PRESUME_AVX2"
        );
    }
    let lib = link_system_lib();
    emit_system_libopus_cfg(&lib.version);
}

fn build_bundled_and_link(opts: &BuildOptions) {
    if opts.presume_avx && !opts.avx_allowed {
        println!(
            "cargo:warning=presume-avx2 feature only applies to x86/x86_64 targets; ignoring for {}",
            opts.target_arch
        );
    }

    let opus_source = bundled_opus_source(opts);
    let dst = build_bundled(opts, &opus_source);
    emit_bundled_libopus_cfg(&opus_source);
    println!("cargo:rustc-link-search=native={}/lib", dst.display());
    println!("cargo:rustc-link-search=native={}/lib64", dst.display());
    println!("cargo:rustc-link-lib=static=opus");
}

fn bundled_opus_source(opts: &BuildOptions) -> PathBuf {
    if opts.dred_enabled {
        prepare_dred_opus_source()
    } else {
        PathBuf::from("opus")
    }
}

fn build_bundled(opts: &BuildOptions, opus_source: &Path) -> std::path::PathBuf {
    let mut config = cmake::Config::new(opus_source);

    config.profile("Release");

    if let Some(runtime) = opts.msvc_runtime {
        config.static_crt(runtime.is_static());
        config.define("OPUS_STATIC_RUNTIME", runtime.opus_static_runtime());
    }

    config
        .define("OPUS_BUILD_SHARED_LIBRARY", "OFF")
        .define("OPUS_BUILD_TESTING", "OFF")
        .define("OPUS_BUILD_PROGRAMS", "OFF")
        .define("OPUS_DRED", if opts.dred_enabled { "ON" } else { "OFF" })
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("OPUS_DISABLE_INTRINSICS", "OFF")
        .define("CMAKE_POSITION_INDEPENDENT_CODE", "ON");

    if opts.presume_avx {
        config
            .define("OPUS_X86_PRESUME_AVX2", "ON")
            .define("OPUS_X86_MAY_HAVE_AVX2", "ON");
    }

    config.build()
}

fn link_system_lib() -> pkg_config::Library {
    pkg_config::Config::new()
        .atleast_version("1.6.1")
        .probe("opus")
        .expect("system-lib feature requested but pkg-config couldn't find libopus")
}

fn emit_system_libopus_cfg(version: &str) {
    match version {
        "1.6.1" => emit_packet_ops_cfg(PacketOpsCompatibility {
            rust_packet_ops: true,
            frame_bounded_extensions: true,
        }),
        _ => println!(
            "cargo:warning=system libopus {version} is not one of the exact packet-op versions \
             supported by opus-codec (1.6.1); packet padding and repacketizer emission \
             will delegate to the linked C libopus"
        ),
    }
}

fn emit_bundled_libopus_cfg(opus_source: &Path) {
    if bundled_packet_ops_match(opus_source) {
        emit_packet_ops_cfg(PacketOpsCompatibility {
            rust_packet_ops: true,
            frame_bounded_extensions: true,
        });
    } else {
        println!(
            "cargo:warning=vendored libopus packet-op sources do not match the audited \
             compatibility fingerprints; packet padding and repacketizer emission will \
             delegate to bundled C libopus"
        );
    }
}

fn emit_packet_ops_cfg(compatibility: PacketOpsCompatibility) {
    if compatibility.rust_packet_ops {
        emit_rust_packet_ops_cfg();
    }
    if compatibility.frame_bounded_extensions {
        emit_frame_bounded_extensions_cfg();
    }
}

fn emit_rust_packet_ops_cfg() {
    println!("cargo:rustc-cfg=opus_codec_rust_packet_ops");
}

fn emit_frame_bounded_extensions_cfg() {
    println!("cargo:rustc-cfg=opus_codec_frame_bounded_extensions");
}

fn bundled_packet_ops_match(opus_source: &Path) -> bool {
    BUNDLED_PACKET_OPS_FINGERPRINTS
        .iter()
        .all(|fingerprint| source_fingerprint_matches(opus_source, *fingerprint))
}

fn source_fingerprint_matches(opus_source: &Path, fingerprint: SourceFingerprint) -> bool {
    let path = opus_source.join(fingerprint.path);
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    let bytes = normalize_source_line_endings(&bytes);
    let actual_len = u64::try_from(bytes.len()).expect("source file length does not fit in u64");
    let actual_hash = sha256_hex_bytes(&bytes);
    if actual_len == fingerprint.len && actual_hash == fingerprint.sha256 {
        return true;
    }

    println!(
        "cargo:warning=vendored libopus packet-op source fingerprint mismatch for {}: \
         expected normalized len {}, sha256 {}; got normalized len {}, sha256 {}",
        path.display(),
        fingerprint.len,
        fingerprint.sha256,
        actual_len,
        actual_hash
    );
    false
}

fn normalize_source_line_endings(bytes: &[u8]) -> Cow<'_, [u8]> {
    if !bytes.windows(2).any(|window| window == b"\r\n") {
        return Cow::Borrowed(bytes);
    }

    let mut normalized = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            normalized.push(b'\n');
            index += 2;
        } else {
            normalized.push(bytes[index]);
            index += 1;
        }
    }
    Cow::Owned(normalized)
}

fn prepare_dred_opus_source() -> PathBuf {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is not set by Cargo"));
    let opus_source = out_dir.join("opus-dred-src");
    if opus_source.exists() {
        std::fs::remove_dir_all(&opus_source)
            .unwrap_or_else(|err| panic!("failed to remove {}: {err}", opus_source.display()));
    }
    copy_opus_source_tree(Path::new("opus"), &opus_source)
        .unwrap_or_else(|err| panic!("failed to copy vendored opus source: {err}"));
    ensure_dred_assets(&opus_source, &out_dir);
    opus_source
}

fn copy_opus_source_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if should_skip_dred_generated_path(&src_path) {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            copy_opus_source_tree(&src_path, &dst_path)?;
        } else if metadata.is_file() {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

fn should_skip_dred_generated_path(path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix("opus") else {
        return false;
    };
    let rel = rel.to_string_lossy().replace('\\', "/");
    matches!(
        rel.as_str(),
        DRED_MODEL_ARCHIVE
            | "dnn/bbwenet_data.c"
            | "dnn/bbwenet_data.h"
            | "dnn/dred_rdovae_constants.h"
            | "dnn/dred_rdovae_dec_data.c"
            | "dnn/dred_rdovae_dec_data.h"
            | "dnn/dred_rdovae_enc_data.c"
            | "dnn/dred_rdovae_enc_data.h"
            | "dnn/dred_rdovae_stats_data.c"
            | "dnn/dred_rdovae_stats_data.h"
            | "dnn/fargan_data.c"
            | "dnn/fargan_data.h"
            | "dnn/lace_data.c"
            | "dnn/lace_data.h"
            | "dnn/lossgen_data.c"
            | "dnn/lossgen_data.h"
            | "dnn/nolace_data.c"
            | "dnn/nolace_data.h"
            | "dnn/pitchdnn_data.c"
            | "dnn/pitchdnn_data.h"
            | "dnn/plc_data.c"
            | "dnn/plc_data.h"
            | "dnn/models"
    ) || rel.starts_with("dnn/models/")
}

fn ensure_dred_assets(opus_source: &Path, out_dir: &Path) {
    use std::path::Component;
    use std::process::Command;

    const REQUIRED_FILE: &str = "dnn/fargan_data.h";
    if opus_source.join(REQUIRED_FILE).exists() {
        return;
    }

    let cached_archive_path = Path::new("opus").join(DRED_MODEL_ARCHIVE);
    let archive_path = if cached_archive_path.exists() {
        std::fs::canonicalize(&cached_archive_path).unwrap_or_else(|err| {
            panic!(
                "failed to canonicalize cached DRED archive {}: {err}",
                cached_archive_path.display()
            )
        })
    } else {
        out_dir.join(DRED_MODEL_ARCHIVE)
    };
    if !archive_path.exists() {
        let url = format!("https://media.xiph.org/opus/models/{DRED_MODEL_ARCHIVE}");
        let status = Command::new("wget")
            .arg("-O")
            .arg(&archive_path)
            .arg(&url)
            .status()
            .or_else(|_| {
                Command::new("curl")
                    .arg("-L")
                    .arg("-o")
                    .arg(&archive_path)
                    .arg(&url)
                    .status()
            })
            .expect("failed to spawn wget or curl for DRED model download");

        if !status.success() {
            panic!("downloading DRED model assets failed (exit status: {status})");
        }
    }

    let actual = sha256_hex(&archive_path);
    if actual != DRED_MODEL_SHA256 {
        panic!(
            "DRED model archive checksum mismatch for {}: expected {}, got {}",
            archive_path.display(),
            DRED_MODEL_SHA256,
            actual
        );
    }

    let listing = Command::new("tar")
        .arg("tf")
        .arg(&archive_path)
        .output()
        .expect("failed to list DRED model archive");
    if !listing.status.success() {
        panic!(
            "listing DRED model archive failed (exit status: {})",
            listing.status
        );
    }
    for entry in String::from_utf8_lossy(&listing.stdout).lines() {
        let path = Path::new(entry);
        if path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            panic!("DRED model archive contains unsafe path: {entry}");
        }
    }

    let status = Command::new("tar")
        .arg("xvomf")
        .arg(&archive_path)
        .current_dir(opus_source)
        .status()
        .expect("failed to extract DRED model archive");
    if !status.success() {
        panic!("extracting DRED model assets failed (exit status: {status})");
    }

    if !opus_source.join(REQUIRED_FILE).exists() {
        panic!("DRED model download completed but {REQUIRED_FILE} is still missing");
    }
}

fn generate_bindings() {
    let bindings_path = std::path::Path::new("src/bindings.rs");

    if bindings_path.exists() {
        eprintln!("Using existing src/bindings.rs. Delete this file to force regeneration.");
        return;
    }

    let bindings = bindgen::Builder::default()
        .header("opus/include/opus.h")
        .header("opus/include/opus_defines.h")
        .header("opus/include/opus_types.h")
        .header("opus/include/opus_multistream.h")
        .header("opus/include/opus_projection.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    bindings
        .write_to_file(bindings_path)
        .expect("Couldn't write bindings!");
}

fn target_is_windows_msvc() -> bool {
    matches!(
        env::var("CARGO_CFG_TARGET_FAMILY").as_deref(),
        Ok("windows")
    ) && matches!(env::var("CARGO_CFG_TARGET_ENV").as_deref(), Ok("msvc"))
}

fn target_feature_enabled(feature_name: &str) -> bool {
    match env::var("CARGO_CFG_TARGET_FEATURE") {
        Ok(features) => features
            .split(',')
            .map(str::trim)
            .any(|feature| feature == feature_name),
        Err(_) => false,
    }
}

fn sha256_hex(path: &Path) -> String {
    let bytes = std::fs::read(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    sha256_hex_bytes(&bytes)
}

fn sha256_hex_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("writing to String should not fail");
    }
    hex
}
