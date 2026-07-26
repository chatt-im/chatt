# Building Chatt

Chatt currently builds on macOS and Linux. Windows is not supported.

## Prerequisites

Install a current Rust toolchain with [rustup](https://rustup.rs/). Chatt
requires Rust 1.91 or newer.

On macOS, install the Xcode command line tools:

```sh
xcode-select --install
```

On Debian or Ubuntu, install the native build dependencies:

```sh
sudo apt update
sudo apt install build-essential git pkg-config libasound2-dev
```

On Arch Linux:

```sh
sudo pacman -S --needed base-devel git rustup pkgconf alsa-lib
```

Other Linux distributions need the equivalent C toolchain, `git`, `pkg-config`,
and ALSA development headers. Opus, Opus DRED model data, RNNoise, RNNoise
weights, and the production web view are included in the repository.

## Build

```sh
git clone https://gitlab.com/chatt-im/chatt.git
cd chatt
cargo build --release -p chatt -p server
```

The resulting applications are:

```text
target/release/chatt
target/release/chatt-server
```

Run them from the repository, copy them somewhere in `PATH`, or install them in
your user binary directory:

```sh
install -d "$HOME/.local/bin"
install -m 0755 target/release/chatt target/release/chatt-server "$HOME/.local/bin/"
```

## Start a local server

Start a server by giving it a private instance directory:

```sh
chatt-server serve --dir ./chatt-server
```

The directory is created when absent. On the first run, the server writes
`./chatt-server/chatt-server.toml` with a private identity seed and uses
`./chatt-server/chatt-server-data/` for runtime state. Keep the directory and
configuration private.

Scalar settings under `network`, `security`, and `storage` can be overridden
on the command line, except for `security.server-identity-seed`, which remains
configuration-file-only so the private seed is never exposed through process
arguments. Settings below an array, such as `rooms`, must be edited in the TOML
file:

```sh
chatt-server serve \
  --dir ./chatt-server \
  --network.tcp-addr=127.0.0.1:42000 \
  --network.p2p=false
```

Precedence is built-in defaults, then `chatt-server.toml`, then command-line
overrides. Command-line overrides apply only to that process and do not rewrite
the configuration file. Run `chatt-server --help` for the complete option list.

The explicit configuration-file flow remains available when a separate
instance directory is not wanted:

```sh
chatt-server init-config chatt-server.toml
chatt-server serve chatt-server.toml
```

The generated file's comments describe bind addresses, public endpoints,
rooms, storage, and open pairing options.

In another terminal, create an invite:

```sh
chatt-server invite alice
```

Pair the client with the printed invite string:

```sh
chatt pair tcj1_...
```

Pairing writes the client configuration. Run `chatt` afterward to open the
terminal client.

## Optional integrations

- Screen capture needs `ffmpeg` by default. A custom capture command can be
  used instead.
- Clipboard integration uses `wl-paste` on Wayland, `xclip` or `xsel` on X11,
  and `pbpaste` on macOS. Image paste on macOS also uses `pngpaste`.
- Native PipeWire audio is available with `--features pipewire` and requires
  PipeWire development files discoverable through `pkg-config`.

The default Linux audio build requests real-time callback scheduling. Chatt
continues at normal priority when the operating system does not grant it.
