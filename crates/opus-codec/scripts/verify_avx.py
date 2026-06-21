#!/usr/bin/env python3
"""
Build and verify AVX-presume gating for the bundled opus library.

- Builds a generic target (no presume feature) and expects no AVX flag/instructions.
- Builds a presume target (presume-avx2 feature) and expects AVX flag/instructions.
"""

from pathlib import Path
from typing import Optional, Tuple
import ci_utils


PRESUME_FLAG = "OPUS_X86_PRESUME_AVX2:BOOL=ON"


def newest_build_dir(target_dir: Path) -> Optional[Path]:
    build_root = target_dir / "release" / "build"
    if not build_root.exists():
        return None
    # Find all opus-codec-* directories
    candidates = [p for p in build_root.glob("opus-codec-*") if p.is_dir()]
    if not candidates:
        return None
    # Return the most recently modified one
    return max(candidates, key=lambda p: p.stat().st_mtime)


def find_artifacts(base: Path) -> Tuple[Optional[Path], Optional[Path]]:
    # Recursive search for artifacts
    caches = list(base.rglob("CMakeCache.txt"))
    objs = list(base.rglob("bands.c.o"))
    return (caches[0] if caches else None), (objs[0] if objs else None)


def verify(target_dir: str, features: str, expect_flag: bool, expect_avx: bool) -> None:
    # Build
    cmd = ["cargo", "build", "--release"]
    if features:
        cmd += ["--features", features]
    
    ci_utils.run(cmd, env={"CARGO_TARGET_DIR": target_dir})

    # Verify
    target = Path(target_dir)
    build_dir = newest_build_dir(target)
    if not build_dir:
        ci_utils.fail(f"build dir not found under {target_dir}")
    
    print(f"Checking build dir: {build_dir}")

    cache, obj = find_artifacts(target)
    if not cache or not obj:
        print(f"Artifacts missing for {target_dir}")
        print("Found CMakeCache.txt:", [str(p) for p in target.rglob("CMakeCache.txt")])
        print("Found bands.c.o:", [str(p) for p in target.rglob("bands.c.o")])
        ci_utils.fail("Missing required build artifacts")

    # Check CMake cache for flag
    cache_content = cache.read_text()
    flag_present = PRESUME_FLAG in cache_content
    if flag_present != expect_flag:
        ci_utils.fail(
            f"AVX presume flag mismatch in {cache}: expected={expect_flag}, got={flag_present}"
        )

    # Check object file for AVX instructions
    disasm = ci_utils.run(
        ["objdump", "-d", str(obj)], capture_output=True
    ).stdout
    
    has_avx = "ymm" in disasm
    if has_avx != expect_avx:
        ci_utils.fail(
            f"AVX instructions mismatch in {obj}: expected={expect_avx}, got={has_avx}"
        )


def main() -> None:
    verify("target/ci-generic", "", False, False)
    verify("target/ci-presume", "presume-avx2", True, True)


if __name__ == "__main__":
    main()
