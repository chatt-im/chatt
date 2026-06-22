# chatt benchmarks

This crate uses `jsony_bench` to quantify the hot audio paths against
`assets/sample-001.opus`.

Run benchmarks in release mode:

```sh
cargo run --release -p benchmark -- bench opus/encode --progress
cargo run --release -p benchmark -- bench opus/decode --progress
cargo run --release -p benchmark -- bench dred/parse --progress
cargo run --release -p benchmark -- bench dred/recover_available --progress
cargo run --release -p benchmark -- bench rnnoise/process --progress
cargo run --release -p benchmark -- bench pipeline/rnnoise_then_encode --progress
cargo run --release -p benchmark -- bench pipeline/aec_then_encode --progress
cargo run --release -p benchmark -- bench live/call_sim --progress
cargo run --release -p benchmark -- bench live/group_call_sim --progress
```

`pipeline/aec_then_encode` mirrors `pipeline/rnnoise_then_encode` but runs the
`sonora` AEC3 echo canceller (render plus capture) as the per-frame DSP step
ahead of the Opus encode, isolating the echo cancellation overhead. `live/call_sim`
takes an `aec=off|on` parameter that toggles echo cancellation end to end so the
on/off wall-clock delta measures the full-pipeline cost.

Filter benchmark parameters:

```sh
cargo run --release -p benchmark -- bench opus/encode --param profile=dred_32k_1000ms_loss20 --progress
cargo run --release -p benchmark -- bench live/call_sim --param scenario=lossy_speech --param feature=all_on --param loss=congested_wifi --param aec=on --progress
cargo run --release -p benchmark -- bench live/playback_mixer --param feature=skip_off --progress
```

Profile a route with calibrated default iterations:

```sh
cargo run --release -p benchmark -- profile opus/encode
samply record cargo run --release -p benchmark -- profile live/call_sim
samply record cargo run --release -p benchmark -- profile live/group_call_sim
```

The `profile` subcommand is quiet by default so profiler output stays compact;
pass `--progress` only when debugging benchmark selection. Parameterized routes
use representative profile defaults when a parameter is not explicitly filtered;
pass `--param key=value` to profile a different case. `--iterations N` remains
available to override the route default.

Export listenable live-audio simulation WAVs:

```sh
cargo run --release -p benchmark --example export_live_audio
```

The exporter writes the synthetic pre-network input, the full direct sample
input, no-loss, congested-Wi-Fi, and 60% random-loss client reconstructions
through the live Opus/DRED decoder and playback mixer. Direct-sample runs also
write `direct-trace-*.jsonl` with capture, packet, DRED/PLC, mixer, and output
window events.

Save and compare runs with `jsony_bench`:

```sh
cargo run --release -p benchmark -- bench opus/encode --save /tmp/chatt-opus-base.json --progress
cargo run --release -p benchmark -- bench opus/encode --compare /tmp/chatt-opus-base.json --progress
```

Notes:

- `ffmpeg` and `ffprobe` must be available in `PATH`; they decode and inspect
  audio samples during benchmark and codec-test workflows. Install them with
  `sudo apt install -y ffmpeg` on Debian/Ubuntu or
  `sudo pacman -S --needed ffmpeg` on Arch.
- Opus uses 20 ms frames, VOIP mode, VBR, wideband, and complexity 9 so the
  analysis path needed by DRED activity gating is enabled.
- DRED profiles set DRED duration, packet loss percentage, and in-band FEC.
- `dred/recover_available` benchmarks the first redundancy offset reported by
  `opus_dred_parse`; it only registers profiles that emit parseable DRED
  recovery data for the current corpus.
- `rnnoise/process` uses 10 ms RNNoise frames scaled to the i16 range expected
  by `nnnoiseless`.
- `live/*` routes reuse decoded speech frames from `assets/sample-001.opus` and
  run deterministic simulated-time capture/playback scenarios without sleeping.
  `live/call_sim` and `live/group_call_sim` accept `loss=none`,
  `mild_random`, `moderate_random`, `severe_random`, `random_30`,
  `random_45`, `random_60`, `bursty_wifi`, `congested_wifi`,
  `mobile_handoff`, or `scenario_default`. Non-`none` named profiles combine
  packet drops with delivery jitter so they exercise out-of-order playback too.

For packet-size context, run:

```sh
cargo test -p benchmark -- --nocapture profiles_encode_packets
```
