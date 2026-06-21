#!/usr/bin/env python3
"""
Verify system libopus usage:

- Ensures pkg-config reports libopus 1.6.1.
- Builds and tests with the `system-lib` feature.
"""

import subprocess
from pathlib import Path
from typing import Optional

import ci_utils

DEB_URLS = {
    "dev": [
        "https://deb.debian.org/debian/pool/main/o/opus/libopus-dev_1.6.1-1+b1_amd64.deb",
        "https://mirrors.edge.kernel.org/debian/pool/main/o/opus/libopus-dev_1.6.1-1+b1_amd64.deb",
    ],
    "runtime": [
        "https://deb.debian.org/debian/pool/main/o/opus/libopus0_1.6.1-1+b1_amd64.deb",
        "https://mirrors.edge.kernel.org/debian/pool/main/o/opus/libopus0_1.6.1-1+b1_amd64.deb",
    ],
}

EXPECTED_VERSION = "1.6.1"


def pkg_config_version() -> Optional[str]:
    try:
        out = ci_utils.run(
            ["pkg-config", "--modversion", "opus"], capture_output=True
        ).stdout
        return out.strip()
    except subprocess.CalledProcessError as exc:
        print(f"pkg-config failed: {exc}")
        return None


def download_first(urls, dest: Path) -> bool:
    for u in urls:
        try:
            ci_utils.run(["curl", "-fLsS", u, "-o", str(dest)])
            print(f"Downloaded {u}")
            return True
        except subprocess.CalledProcessError:
            print(f"Download failed from {u}, trying next mirror...")
    return False


def install_debs_if_needed() -> None:
    ver = pkg_config_version()
    if ver == EXPECTED_VERSION:
        print(f"libopus already at {EXPECTED_VERSION}")
        return

    # Fast path: try the packaged version first.
    with ci_utils.group("Install libopus-dev from apt"):
        try:
            ci_utils.run(["sudo", "apt-get", "update"])
            ci_utils.run(["sudo", "apt-get", "install", "-y", "libopus-dev"])
        except subprocess.CalledProcessError:
            print("apt-get install libopus-dev failed, will try deb mirrors")

    ver_after_apt = pkg_config_version()
    if ver_after_apt == EXPECTED_VERSION:
        print(f"libopus at {EXPECTED_VERSION} after apt install")
        return

    # Try downloading Debian packages on Ubuntu runners.
    # Only proceed if /etc/os-release indicates Ubuntu.
    os_release = Path("/etc/os-release")
    if os_release.exists():
        data = os_release.read_text().lower()
        if "ubuntu" not in data:
            ci_utils.fail(
                f"Expected libopus {EXPECTED_VERSION} but found {ver_after_apt}; not on Ubuntu, aborting"
            )
    else:
        ci_utils.fail(
            f"Expected libopus {EXPECTED_VERSION} but found {ver_after_apt}; /etc/os-release missing"
        )

    runtime_deb = Path("/tmp/libopus0.deb")
    dev_deb = Path("/tmp/libopus-dev.deb")

    with ci_utils.group("Download libopus debs"):
        ok = download_first(DEB_URLS["runtime"], runtime_deb) and download_first(
            DEB_URLS["dev"], dev_deb
        )
        if not ok:
            ci_utils.fail("Failed to download libopus debs from all mirrors")

    with ci_utils.group("Install libopus debs"):
        try:
            ci_utils.run(["sudo", "dpkg", "-i", str(runtime_deb), str(dev_deb)])
        except subprocess.CalledProcessError:
            print("dpkg failed, trying apt-get install -f")
            ci_utils.run(["sudo", "apt-get", "install", "-f", "-y"])

    ver_after = pkg_config_version()
    if ver_after != EXPECTED_VERSION:
        ci_utils.fail(
            f"After deb install, expected libopus {EXPECTED_VERSION} but found {ver_after}"
        )


def main() -> None:
    install_debs_if_needed()
    ver = pkg_config_version()
    print(f"pkg-config opus version: {ver}")
    if ver != EXPECTED_VERSION:
        ci_utils.fail(f"Expected libopus {EXPECTED_VERSION} but found {ver}")

    ci_utils.run(["cargo", "build", "--features", "system-lib"])
    ci_utils.run(["cargo", "test", "--features", "system-lib"])


if __name__ == "__main__":
    main()
