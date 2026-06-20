# tomchat benchmarks

This crate uses `jsony_bench` to quantify the hot audio paths against
`assets/sample-001.opus`.

Run benchmarks in release mode:

```sh
cargo run --release -p benchmark -- opus/encode
cargo run --release -p benchmark -- opus/decode
cargo run --release -p benchmark -- dred/parse
cargo run --release -p benchmark -- dred/recover_available
cargo run --release -p benchmark -- rnnoise/process
cargo run --release -p benchmark -- pipeline/rnnoise_then_encode
```

Filter a profile:

```sh
cargo run --release -p benchmark -- opus/encode --param profile=dred_32k_1000ms_loss20
```

Save and compare runs with `jsony_bench`:

```sh
cargo run --release -p benchmark -- opus/encode --save /tmp/tomchat-opus-base.json
cargo run --release -p benchmark -- opus/encode --compare /tmp/tomchat-opus-base.json
```

Notes:

- `ffmpeg` must be available in `PATH`; it decodes the provided Opus sample to
  mono 48 kHz float PCM at benchmark startup.
- Opus uses 20 ms frames, VOIP mode, VBR, wideband, and complexity 9 so the
  analysis path needed by DRED activity gating is enabled.
- DRED profiles set DRED duration, packet loss percentage, and in-band FEC.
- `dred/recover_available` benchmarks the first redundancy offset reported by
  `opus_dred_parse`; it only registers profiles that emit parseable DRED
  recovery data for the current corpus.
- `rnnoise/process` uses 10 ms RNNoise frames scaled to the i16 range expected
  by `nnnoiseless`.

For packet-size context, run:

```sh
cargo test -p benchmark -- --nocapture profiles_encode_packets
```
