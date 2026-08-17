use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_PRESUME_AVX2");

    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
    );
    let weights = manifest_dir.join("rnnoise_weights.bin");
    let target = env::var("TARGET").unwrap_or_default();
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    println!("cargo:rerun-if-changed={}", weights.display());
    build_weight_blob(&weights, &out_dir, &target);

    let src = "c-rnnoise/src";
    let include = "c-rnnoise/include";
    let generic_files = [
        "denoise.c",
        "rnn.c",
        "pitch.c",
        "kiss_fft.c",
        "celt_lpc.c",
        "nnet.c",
        "nnet_default.c",
        "parse_lpcnet_weights.c",
        "rnnoise_data.c",
        "rnnoise_tables.c",
    ];

    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let x86_target = matches!(target_arch.as_str(), "x86" | "x86_64");
    let presume_avx2 = env::var_os("CARGO_FEATURE_PRESUME_AVX2").is_some();
    let x86_rtcd = x86_target && !presume_avx2;

    if presume_avx2 && !x86_target {
        println!(
            "cargo:warning=presume-avx2 feature only applies to x86/x86_64 targets; ignoring for {}",
            target_arch
        );
    }

    let mut generic = base_build(src, include);
    generic.define("USE_WEIGHTS_FILE", None);
    generic.define("RNNOISE_BUILD", None);
    if x86_rtcd {
        generic.define("RNN_ENABLE_X86_RTCD", None);
        generic.define("CPU_INFO_BY_C", None);
    } else if presume_avx2 && x86_target {
        add_avx2_flags(&mut generic, &target);
    }
    for file in generic_files {
        println!("cargo:rerun-if-changed={src}/{file}");
        generic.file(format!("{src}/{file}"));
    }
    if x86_rtcd {
        for file in ["x86/x86_dnn_map.c", "x86/x86cpu.c"] {
            println!("cargo:rerun-if-changed={src}/{file}");
            generic.file(format!("{src}/{file}"));
        }
    }
    add_header_reruns(src, include, x86_rtcd);

    if !target.contains("msvc") {
        generic.flag_if_supported("-std=c99");
    }

    generic.compile("rnnoise_v2");

    if x86_rtcd {
        let mut sse = base_build(src, include);
        sse.define("RNN_ENABLE_X86_RTCD", None)
            .define("OPUS_X86_MAY_HAVE_SSE4_1", None)
            .file(format!("{src}/x86/nnet_sse4_1.c"));
        // MSVC has no per-feature SSE switch: SSE through SSE4.1 intrinsics are
        // always available on x86-64, and `-msse4.1` would be rejected.
        if !target.contains("msvc") {
            sse.flag_if_supported("-msse4.1");
            sse.flag_if_supported("-std=c99");
        }
        sse.compile("rnnoise_v2_sse4_1");

        let mut avx = base_build(src, include);
        avx.define("RNN_ENABLE_X86_RTCD", None)
            .define("OPUS_X86_MAY_HAVE_AVX2", None)
            .file(format!("{src}/x86/nnet_avx2.c"));
        // This translation unit hard-errors unless the compiler is actually in
        // AVX2 mode, so it needs the flag its own compiler spells: `/arch:AVX2`
        // for MSVC, the three `-m` flags for everyone else.
        add_avx2_flags(&mut avx, &target);
        if !target.contains("msvc") {
            avx.flag_if_supported("-std=c99");
        }
        avx.compile("rnnoise_v2_avx2");
    }
}

fn build_weight_blob(weights: &Path, out_dir: &Path, target: &str) {
    println!("cargo:rustc-check-cfg=cfg(rnnoise_weights_in_rust)");

    // The GNU assembler's `.incbin` is what embeds the blob everywhere else.
    // MSVC's assembler has no equivalent directive, so on that target the blob
    // is embedded from Rust instead and the crate reads it through a generated
    // static rather than a pair of linker symbols.
    if target.contains("msvc") {
        emit_rust_weight_blob(weights, out_dir);
        println!("cargo:rustc-cfg=rnnoise_weights_in_rust");
        return;
    }

    let symbol_prefix = if target.contains("apple") { "_" } else { "" };
    let section = if target.contains("apple") {
        ".section __DATA,__const"
    } else if target.contains("windows") {
        ".section .rdata,\"dr\""
    } else {
        ".section .rodata.rnnoise_v2_weights,\"a\",@progbits"
    };
    let asm = out_dir.join("rnnoise_v2_weights.S");
    let source = format!(
        "{section}\n\
         .globl {symbol_prefix}rnnoise_v2_weights\n\
         .globl {symbol_prefix}rnnoise_v2_weights_end\n\
         .balign 32\n\
         {symbol_prefix}rnnoise_v2_weights:\n\
         .incbin \"{}\"\n\
         {symbol_prefix}rnnoise_v2_weights_end:\n\
         .byte 0\n",
        escaped_path_literal(weights),
    );
    fs::write(&asm, source).expect("write RNNoise V2 weight assembly");

    let mut build = cc::Build::new();
    build.file(&asm).warnings(false);
    build.compile("rnnoise_v2_weights");
}

/// Writes a Rust source file that embeds the weight blob as an over-aligned
/// static, for targets whose assembler cannot `.incbin`.
///
/// The alignment matters: the DNN kernels load the weights with 256-bit AVX
/// instructions, so the blob is aligned to 32 bytes exactly as the assembly
/// path's `.balign 32` does.
fn emit_rust_weight_blob(weights: &Path, out_dir: &Path) {
    let len = fs::metadata(weights)
        .expect("stat RNNoise weight blob")
        .len();
    let source = format!(
        "#[repr(C, align(32))]\n\
         pub(crate) struct AlignedWeights(pub(crate) [u8; {len}]);\n\
         pub(crate) static RNNOISE_V2_WEIGHTS: AlignedWeights =\n\
         \x20   AlignedWeights(*include_bytes!(\"{}\"));\n",
        escaped_path_literal(weights),
    );
    fs::write(out_dir.join("rnnoise_v2_weights.rs"), source)
        .expect("write RNNoise V2 weight module");
}

/// Escapes `path` for a double-quoted string literal.
///
/// Rust source and GNU-as `.incbin` want the same two escapes, which matters on
/// Windows, where every generated path is full of backslashes.
fn escaped_path_literal(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

fn base_build(src: &str, include: &str) -> cc::Build {
    let mut build = cc::Build::new();
    build.include(include).include(src).warnings(false);
    build
}

fn add_avx2_flags(build: &mut cc::Build, target: &str) {
    if target.contains("msvc") {
        build.flag_if_supported("/arch:AVX2");
    } else {
        build
            .flag_if_supported("-mavx")
            .flag_if_supported("-mfma")
            .flag_if_supported("-mavx2");
    }
}

fn add_header_reruns(src: &str, include: &str, x86_rtcd: bool) {
    for header in [
        "arch.h",
        "celt_lpc.h",
        "common.h",
        "cpu_support.h",
        "denoise.h",
        "kiss_fft.h",
        "nnet.h",
        "nnet_arch.h",
        "opus_types.h",
        "pitch.h",
        "rnn.h",
        "rnnoise_data.h",
        "vec.h",
        "vec_avx.h",
        "vec_neon.h",
        "_kiss_fft_guts.h",
        "x86/x86_arch_macros.h",
        "x86/x86cpu.h",
        "x86/dnn_x86.h",
    ] {
        println!("cargo:rerun-if-changed={src}/{header}");
    }
    if x86_rtcd {
        for file in ["x86/nnet_sse4_1.c", "x86/nnet_avx2.c"] {
            println!("cargo:rerun-if-changed={src}/{file}");
        }
    }
    println!("cargo:rerun-if-changed={include}/rnnoise.h");
}
