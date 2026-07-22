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
cargo run --release -p benchmark -- bench live/output_contention --progress
cargo run --release -p benchmark -- bench live/output_callback --progress
cargo run --release -p benchmark -- bench live/ingest_contention --progress
cargo run --release -p benchmark -- bench crypto --progress
```

Measure persistent local Unix-socket bulk RPC delivery latency, including raw
wire framing and borrowed decoding, for 64 KiB, the former 192 KiB chunk size,
and the current 1 MiB chunk size:

```sh
cargo run --release -p benchmark --bin local_rpc
```

The report includes p50 and p95 wall latency plus p95 expressed as a fraction
of a 120 Hz frame. It deliberately excludes disk I/O and image decoding.

`crypto/*` isolates every AWS-LC primitive used by Chatt's fixed cipher choices:
ChaCha20-Poly1305 for transport, AES-128-GCM for MLS, SHA-256, HMAC-SHA256,
HKDF-SHA256, X25519, and Ed25519. AEAD, hash, and MAC routes cover small control
messages, media-sized datagrams, and large file chunks. Use these routes when
comparing workspace-wide AWS-LC build flags such as `OPENSSL_SMALL`; a whole-call
audio benchmark can hide a public-key regression behind codec and DSP work.

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
cargo run --release -p benchmark -- bench live/output_callback --param condition=normal --progress
cargo run --release -p benchmark -- bench live/ingest_contention --param condition=extreme --progress
```

Profile a route with calibrated default iterations:

```sh
cargo run --release -p benchmark -- profile opus/encode
samply record cargo run --release -p benchmark -- profile live/call_sim
samply record cargo run --release -p benchmark -- profile live/group_call_sim
samply record cargo run --release -p benchmark -- profile live/output_contention
samply record cargo run --release -p benchmark -- profile live/output_callback
samply record cargo run --release -p benchmark -- profile live/ingest_contention
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

For latency-critical contention work, capture five baseline runs, then compare
five current runs per route and condition. Each saved run includes raw samples
and summary statistics for wall time, CPU cycles, instructions, and branches;
the compare report classifies changes from the cycle posterior and uses
instructions as a grounding signal.

```sh
for i in 1 2 3 4 5; do
  cargo run --release -p benchmark -- bench live/output_callback --param condition=extreme --save /tmp/chatt-output-callback-base-$i.json
  cargo run --release -p benchmark -- bench live/ingest_contention --param condition=extreme --save /tmp/chatt-ingest-contention-base-$i.json
done

for i in 1 2 3 4 5; do
  cargo run --release -p benchmark -- bench live/output_callback --param condition=extreme --compare /tmp/chatt-output-callback-base-$i.json --save /tmp/chatt-output-callback-current-$i.json
  cargo run --release -p benchmark -- bench live/ingest_contention --param condition=extreme --compare /tmp/chatt-ingest-contention-base-$i.json --save /tmp/chatt-ingest-contention-current-$i.json
done
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
- `live/output_callback`, `live/output_contention`, and
  `live/ingest_contention` prebuild six sample-derived speakers, prime their
  playback streams outside measurement, then run a playback-output thread
  against an off-thread packet-ingestion loop for a fixed 30 s simulated call.
  `condition=normal` uses clean/light-loss links; `condition=extreme` gives the
  six speakers different seeded high-loss profiles (`random_30`, `random_45`,
  `random_60`, `bursty_wifi`, `congested_wifi`, `mobile_handoff`).
  `live/output_callback` runs the production callback body for 48 kHz stereo
  f32 output, including event drain, mix adapter, per-sample pull, sample
  conversion/fanout, staged-sample accounting, and callback metrics. It leaves
  optional debug/non-default paths off: no callback observer, no playback WAV
  recorder, no echo-reference writer, and no non-48 kHz resampler.
  `live/output_contention` keeps the narrower direct `mix_10ms` callback-side
  mutex/core path for comparison.

For packet-size context, run:

```sh
cargo test -p benchmark -- --nocapture profiles_encode_packets
```
