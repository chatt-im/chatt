mod dnn_weights;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const COMPACT_DNN_ARTIFACT: &str = "dnn-weights/dnn_weights.bin";

struct BuildOptions {
    use_system_lib: bool,
    dred_enabled: bool,
    presume_avx: bool,
    target_arch: String,
    avx_allowed: bool,
}

impl BuildOptions {
    fn from_env() -> Self {
        let use_system_lib = env::var("CARGO_FEATURE_SYSTEM_LIB").is_ok();
        let dred_enabled = env::var("CARGO_FEATURE_DRED").is_ok();
        let presume_avx = env::var("CARGO_FEATURE_PRESUME_AVX2").is_ok();
        let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
        let avx_allowed = presume_avx && matches!(target_arch.as_str(), "x86" | "x86_64");

        Self {
            use_system_lib,
            dred_enabled,
            presume_avx,
            target_arch,
            avx_allowed,
        }
    }
}

#[derive(Clone, Copy)]
struct PacketOpsCompatibility {
    rust_packet_ops: bool,
    frame_bounded_extensions: bool,
}

fn main() {
    emit_rerun_directives();
    let opts = BuildOptions::from_env();

    if opts.use_system_lib {
        println!("cargo:rustc-cfg=opus_codec_system_lib");

        #[cfg(feature = "system-lib")]
        handle_system_lib(&opts);

        #[cfg(not(feature = "system-lib"))]
        unreachable!("system-lib environment set without the Cargo feature");
    } else {
        build_bundled_and_link(&opts);
    }
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
    println!("cargo:rerun-if-changed=opus/opus_sources.mk");
    println!("cargo:rerun-if-changed=opus/celt_sources.mk");
    println!("cargo:rerun-if-changed=opus/silk_sources.mk");
    println!("cargo:rerun-if-changed=opus/lpcnet_sources.mk");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=dnn_weights.rs");
    println!("cargo:rerun-if-changed={COMPACT_DNN_ARTIFACT}");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SYSTEM_LIB");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_PRESUME_AVX2");
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_ENV");
}

#[cfg(feature = "system-lib")]
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
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        panic!(
            "opus-codec no longer builds the bundled libopus for MSVC targets; \
             enable the system-lib feature and provide libopus via pkg-config"
        );
    }
    if opts.presume_avx && !opts.avx_allowed {
        println!(
            "cargo:warning=presume-avx2 feature only applies to x86/x86_64 targets; ignoring for {}",
            opts.target_arch
        );
    }

    let dnn_root = opts.dred_enabled.then(expand_dnn_weights);
    let is_x86 = matches!(opts.target_arch.as_str(), "x86" | "x86_64");
    let is_aarch64 = opts.target_arch == "aarch64";
    // With SSE and SSE2 presumed by the x86_64 baseline and SSE4.1/AVX2 by the
    // presume-avx2 feature, every call site resolves to a direct call and the
    // RTCD sources preprocess to empty objects, so skip them entirely.
    let rtcd_needed = is_x86 && !(opts.target_arch == "x86_64" && opts.avx_allowed);

    let mut template = cc::Build::new();
    template
        .warnings(false)
        // Match the cmake Release profile the previous build used regardless of
        // the cargo profile: cc otherwise inherits cargo's OPT_LEVEL and never
        // defines NDEBUG, leaving assert() live in the DNN hot paths.
        .opt_level(3)
        .define("NDEBUG", None)
        .include("opus/include")
        .include("opus/celt")
        .include("opus/silk")
        .include("opus/silk/float")
        .include("opus")
        .define("OPUS_BUILD", None)
        .define("VAR_ARRAYS", None)
        .define("HAVE_LRINTF", None)
        .define("HAVE_LRINT", None)
        .define("ENABLE_HARDENING", None)
        .define("DISABLE_DEBUG_FLOAT", None)
        .flag_if_supported("-fstack-protector-strong");

    if opts.dred_enabled {
        template
            .include("opus/dnn")
            .define("ENABLE_DRED", None)
            .define("ENABLE_DEEP_PLC", None);
    }

    if is_x86 {
        template
            .define("OPUS_X86_MAY_HAVE_SSE", None)
            .define("OPUS_X86_MAY_HAVE_SSE2", None)
            .define("OPUS_X86_MAY_HAVE_SSE4_1", None)
            .define("OPUS_X86_MAY_HAVE_AVX2", None);
        if rtcd_needed {
            template
                .define("OPUS_HAVE_RTCD", None)
                .define("CPU_INFO_BY_ASM", None);
        }
        if opts.target_arch == "x86_64" {
            template
                .define("OPUS_X86_PRESUME_SSE", None)
                .define("OPUS_X86_PRESUME_SSE2", None);
        }
        if opts.avx_allowed {
            template
                .define("OPUS_X86_PRESUME_SSE4_1", None)
                .define("OPUS_X86_PRESUME_AVX2", None);
        }
    } else if is_aarch64 {
        template
            .include("opus/silk/fixed")
            .define("OPUS_ARM_MAY_HAVE_NEON", None)
            .define("OPUS_ARM_MAY_HAVE_NEON_INTR", None)
            .define("OPUS_ARM_PRESUME_NEON", None)
            .define("OPUS_ARM_PRESUME_NEON_INTR", None);
    }

    let mut lib = template.clone();
    if opts.avx_allowed {
        for flag in AVX2_FLAGS {
            lib.flag(flag);
        }
    }
    lib.files(mk_sources("opus/opus_sources.mk", "OPUS_SOURCES"));
    lib.files(mk_sources("opus/opus_sources.mk", "OPUS_SOURCES_FLOAT"));
    lib.files(mk_sources("opus/celt_sources.mk", "CELT_SOURCES"));
    lib.files(mk_sources("opus/silk_sources.mk", "SILK_SOURCES"));
    lib.files(mk_sources("opus/silk_sources.mk", "SILK_SOURCES_FLOAT"));
    if let Some(dnn_root) = &dnn_root {
        for list in ["DEEP_PLC_SOURCES", "DRED_SOURCES"] {
            for source in mk_sources("opus/lpcnet_sources.mk", list) {
                if source.exists() {
                    lib.file(source);
                } else {
                    lib.file(dnn_root.join(source.strip_prefix("opus").unwrap()));
                }
            }
        }
    }

    if is_x86 {
        if rtcd_needed {
            lib.files(mk_sources("opus/celt_sources.mk", "CELT_SOURCES_X86_RTCD"));
            lib.files(mk_sources("opus/silk_sources.mk", "SILK_SOURCES_X86_RTCD"));
            if opts.dred_enabled {
                lib.files(mk_sources("opus/lpcnet_sources.mk", "DNN_SOURCES_X86_RTCD"));
            }
        }

        let mut simd_groups = vec![
            (
                vec!["-msse"],
                vec![mk_sources("opus/celt_sources.mk", "CELT_SOURCES_SSE")],
            ),
            (
                vec!["-msse2"],
                vec![mk_sources("opus/celt_sources.mk", "CELT_SOURCES_SSE2")],
            ),
            (
                vec!["-msse4.1"],
                vec![
                    mk_sources("opus/celt_sources.mk", "CELT_SOURCES_SSE4_1"),
                    mk_sources("opus/silk_sources.mk", "SILK_SOURCES_SSE4_1"),
                ],
            ),
            (
                AVX2_FLAGS.to_vec(),
                vec![
                    mk_sources("opus/celt_sources.mk", "CELT_SOURCES_AVX2"),
                    mk_sources("opus/silk_sources.mk", "SILK_SOURCES_AVX2"),
                    mk_sources("opus/silk_sources.mk", "SILK_SOURCES_FLOAT_AVX2"),
                ],
            ),
        ];
        if opts.dred_enabled {
            simd_groups[1]
                .1
                .push(mk_sources("opus/lpcnet_sources.mk", "DNN_SOURCES_SSE2"));
            simd_groups[2]
                .1
                .push(mk_sources("opus/lpcnet_sources.mk", "DNN_SOURCES_SSE4_1"));
            simd_groups[3]
                .1
                .push(mk_sources("opus/lpcnet_sources.mk", "DNN_SOURCES_AVX2"));
        }

        for (flags, source_lists) in simd_groups {
            let mut group = template.clone();
            for flag in flags {
                group.flag(flag);
            }
            for sources in source_lists {
                group.files(sources);
            }
            lib.objects(group.compile_intermediates());
        }
    } else if is_aarch64 {
        lib.files(mk_sources(
            "opus/celt_sources.mk",
            "CELT_SOURCES_ARM_NEON_INTR",
        ));
        lib.files(mk_sources(
            "opus/silk_sources.mk",
            "SILK_SOURCES_ARM_NEON_INTR",
        ));
        if opts.dred_enabled {
            lib.files(mk_sources("opus/lpcnet_sources.mk", "DNN_SOURCES_NEON"));
        }
    }

    lib.compile("opus");
    emit_bundled_libopus_cfg();
}

fn expand_dnn_weights() -> PathBuf {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let dnn_root = out_dir.join("dred-gen");
    dnn_weights::expand_into(&dnn_root, Path::new(COMPACT_DNN_ARTIFACT))
        .unwrap_or_else(|err| panic!("failed to expand compact DNN weights: {err}"));
    dnn_root
}

fn mk_sources(mk_file: &str, var: &str) -> Vec<PathBuf> {
    let content = fs::read_to_string(mk_file)
        .unwrap_or_else(|err| panic!("failed to read source list {mk_file}: {err}"));
    let mut sources = Vec::new();
    let mut in_var = false;
    for line in content.lines() {
        let mut line = line.trim();
        if !in_var {
            let Some(rest) = line.strip_prefix(var) else {
                continue;
            };
            let Some(rest) = rest.trim_start().strip_prefix('=') else {
                continue;
            };
            in_var = true;
            line = rest;
        }
        let (entries, continues) = match line.strip_suffix('\\') {
            Some(entries) => (entries, true),
            None => (line, false),
        };
        for entry in entries.split_whitespace() {
            sources.push(Path::new("opus").join(entry));
        }
        if !continues {
            break;
        }
    }
    if !in_var {
        panic!("source list variable {var} not found in {mk_file}");
    }
    sources
}

const AVX2_FLAGS: &[&str] = &["-mavx", "-mfma", "-mavx2"];

#[cfg(feature = "system-lib")]
fn link_system_lib() -> pkg_config::Library {
    pkg_config::Config::new()
        .atleast_version("1.6.1")
        .probe("opus")
        .expect("system-lib feature requested but pkg-config couldn't find libopus")
}

#[cfg(feature = "system-lib")]
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

fn emit_bundled_libopus_cfg() {
    emit_packet_ops_cfg(PacketOpsCompatibility {
        rust_packet_ops: true,
        frame_bounded_extensions: true,
    });
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
