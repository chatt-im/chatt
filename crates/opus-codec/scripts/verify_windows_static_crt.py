#!/usr/bin/env python3
"""
Build Windows static-CRT test executables and verify they do not import
dynamic CRT DLLs.
"""

import json
import os
import shutil
from pathlib import Path
from typing import Iterable, List

import ci_utils


TARGET = os.environ.get("CRT_VERIFY_TARGET", "x86_64-pc-windows-msvc")
TARGET_DIR = Path(os.environ.get("CARGO_TARGET_DIR", "target/ci-windows-static-crt"))
FORBIDDEN_DLL_PREFIXES = (
    "api-ms-win-crt-",
    "msvcp",
    "msvcr",
    "ucrtbase",
    "vcruntime",
)


def find_dumpbin() -> Path:
    dumpbin = shutil.which("dumpbin")
    if dumpbin:
        return Path(dumpbin)

    program_files_x86 = Path(
        os.environ.get("ProgramFiles(x86)", r"C:\Program Files (x86)")
    )
    vswhere = (
        program_files_x86
        / "Microsoft Visual Studio"
        / "Installer"
        / "vswhere.exe"
    )
    if not vswhere.exists():
        ci_utils.fail("vswhere.exe not found and dumpbin.exe is not on PATH")

    install_path = ci_utils.run(
        [
            str(vswhere),
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
        ],
        capture_output=True,
    ).stdout.strip()
    if not install_path:
        ci_utils.fail("Could not locate a Visual Studio installation with VC tools")

    candidates = sorted(
        Path(install_path).glob("VC/Tools/MSVC/*/bin/Hostx64/x64/dumpbin.exe")
    )
    if not candidates:
        ci_utils.fail("dumpbin.exe not found inside the Visual Studio installation")
    return candidates[-1]


def build_test_executables() -> List[Path]:
    result = ci_utils.run(
        [
            "cargo",
            "test",
            "--no-run",
            "--message-format=json",
            "--target",
            TARGET,
            "--target-dir",
            str(TARGET_DIR),
        ],
        capture_output=True,
    )

    target_root = (TARGET_DIR / TARGET).resolve()
    executables: List[Path] = []
    for line in result.stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        message = json.loads(line)
        if message.get("reason") != "compiler-artifact":
            continue

        executable = message.get("executable")
        if not executable:
            continue

        path = Path(executable).resolve()
        if path.suffix.lower() != ".exe":
            continue
        if target_root not in path.parents:
            continue
        executables.append(path)

    deduped = list(dict.fromkeys(executables))
    if not deduped:
        ci_utils.fail("No target test executables were produced for import inspection")
    return deduped


def imported_dlls(dumpbin: Path, executable: Path) -> List[str]:
    output = ci_utils.run(
        [str(dumpbin), "/dependents", str(executable)],
        capture_output=True,
    ).stdout
    imports = []
    for line in output.splitlines():
        candidate = line.strip().lower()
        if candidate.endswith(".dll"):
            imports.append(candidate)
    return imports


def forbidden_imports(imports: Iterable[str]) -> List[str]:
    return sorted(
        {
            dll
            for dll in imports
            if dll.startswith(FORBIDDEN_DLL_PREFIXES)
        }
    )


def main() -> None:
    if os.name != "nt":
        ci_utils.fail("verify_windows_static_crt.py must run on Windows")

    dumpbin = find_dumpbin()
    print(f"Using dumpbin at: {dumpbin}")

    executables = build_test_executables()
    print(f"Inspecting {len(executables)} test executable(s)")

    failures = []
    for executable in executables:
        with ci_utils.group(f"Inspect {executable.name}"):
            imports = imported_dlls(dumpbin, executable)
            for dll in imports:
                print(f"  {dll}")
            forbidden = forbidden_imports(imports)
            if forbidden:
                failures.append((executable, forbidden))

    if failures:
        for executable, forbidden in failures:
            print(f"{executable} imports forbidden CRT DLLs: {', '.join(forbidden)}")
        ci_utils.fail("Static CRT verification failed")

    print("Static CRT verification passed: no dynamic CRT DLL imports found.")


if __name__ == "__main__":
    main()
