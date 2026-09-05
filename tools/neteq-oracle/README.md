# NetEQ reference verification

`network.cc` links the actual WebRTC arrival history, delay estimators, decision
logic, packet buffer, statistics and their dependencies. It is an input/output
driver, not a second implementation of the algorithms. The separate existing
`main.cc` driver exercises the reference DSP. Sources used for this audit are
in `/tmp/webrtc`, revision `247e4386801b9e2d9ded0674ace472ac770fb4e1`.

## Controller and packet ordering

Requires clang++, make, pkg-config and Abseil development headers/libraries
(`absl_strings`, `absl_str_format`). Run from the repository root:

```sh
make -C tools/neteq-oracle network-oracle-sanitized
CHATT_NETEQ_ORACLE="$PWD/tools/neteq-oracle/network-oracle-sanitized" \
  CHATT_NETEQ_SEEDS=10000 \
  cargo test -q -p chatt network_differential:: -- --ignored --nocapture
```

The sanitized executable uses AddressSanitizer and UndefinedBehaviorSanitizer,
with sanitizer recovery disabled. `make network-oracle` in the tool directory
builds the faster unsanitized executable. Set `WEBRTC=/path/to/webrtc` when
building against another checkout; a different revision may legitimately differ.

The Rust tests require the executable when explicitly invoked. Each case runs
both implementations on identical inputs and compares a checksum of their
integer outputs. A failure reports the first differing command and both outputs
for the first three failing cases. The complete input corpus, Rust checksums,
C++ checksums and C++ stderr are retained under the printed `/tmp/chatt-neteq-*`
directory. Replay an extracted failure with:

```sh
tools/neteq-oracle/network-oracle-sanitized --verbose < /tmp/chatt-neteq-EXAMPLE/case-17.txt
```

Coverage implemented:

- **80,640 bounded arrival-order cases:** all 8! orders for eight distinct
  timestamps with longer duplicates, and all 8! orders for four originals plus
  four longer duplicates. Includes RTP wrap and history-window eviction.
- **10,000 seeded network cases:** each simulates 60 seconds with 20 ms packets,
  5% loss, duplicates, delays from 0–149 ten-millisecond ticks, reorder bursts,
  and RTP wrap. Three quarters use 100–300 ms talkspurts each second with RTP
  advancing through silence; one quarter use continuous speech. This run
  compared 37,618,659 commands, including timer advances and observations.
- **1,000 controller-status cases:** 2,500,000 commands covering normal,
  concealment, merge, successful/failed time scaling, missing/future/obsolete
  packets, varying buffer depth, time-scale memory and up to 10 seconds of
  generated-noise time during concealment. These are controller
  input tests, not a coupled audio-device simulation.
- **1,000 packet-buffer cases:** 2,700,906 commands comparing actual insertion
  outcomes and extracted packet metadata through reordering, timestamp wrap,
  primary/FEC/DRED priorities, collisions and 200-packet capacity flushes.
- **Six-hour clock-drift case:** a longer duplicate followed by about 104 ppm
  sender/receiver clock skew and a 10 ms increase in network delay. The
  comparison followed history-window expiry across 1,080,000 further packets:
  2,160,003 commands matched, with no sanitizer findings.

For one uninterrupted six-hour arrival timeline:

```sh
CHATT_NETEQ_ORACLE="$PWD/tools/neteq-oracle/network-oracle-sanitized" \
  CHATT_NETEQ_SEEDS=1 CHATT_NETEQ_TICKS=2160000 \
  cargo test -q -p chatt upstream_seeded_network_arrivals -- --ignored --nocapture
```

This compared 676,491 commands with no mismatches or sanitizer findings. The
long timeline is a controller/history check, not six hours of audio-device I/O.

The driver protocol is deliberately plain text: `R id` starts a case; `T ticks`
advances time; `H timestamp duration` observes standalone history;
`A timestamp duration update flush` observes controller arrival; `D` supplies
controller status (field order is in `network.cc`); `F` resets history/controller;
`I timestamp sequence codec_priority red_priority` inserts a packet; `G` extracts
one. Durations and timestamps use 48 kHz samples. Timers use 10 ms ticks.

## Real audio

The repository sample is actual audio; the timing/loss profiles are synthetic.
Generate fresh DSP vectors outside the tracked fixture directory:

```sh
make -C tools/neteq-oracle oracle
mkdir -p /tmp/chatt-neteq-real-audio
python3 - <<'PY'
import array, pathlib, subprocess
raw = subprocess.check_output(['ffmpeg', '-v', 'error', '-i',
    'assets/sample-001.opus', '-f', 's16le', '-ar', '48000', '-ac', '1', '-'])
a = array.array('h')
a.frombytes(raw)
pathlib.Path('/tmp/chatt-neteq-real-audio/pipeline_audio_in.txt').write_text(
    '\n'.join(map(str, a)) + '\n')
PY
tools/neteq-oracle/oracle /tmp/chatt-neteq-real-audio
CHATT_NETEQ_VECTORS=/tmp/chatt-neteq-real-audio \
  cargo test -q -p chatt pipeline_matches_oracle -- --ignored --nocapture
```

The 40.616-second sample produces 2,106,720 output samples plus an 8,640-sample
final sync buffer; the Rust DSP matched both byte for byte. The operation tape
scripts expansion, merge, acceleration, fast acceleration and preemptive
expansion. It does not compare two complete NetEQ/Opus implementations. This
test is explicitly ignored without prerequisites rather than silently passing.
`CHATT_NETEQ_VECTORS` also selects the directory for the smaller DSP vector tests.

The full Chatt capture/Opus/DRED/playback simulation additionally runs real
100–300 ms speech excerpts separated by silence. Three brief utterances are
followed by three quiet seconds, so the real two-second capture gate suppresses
packets and resumes with its normal preroll and flags:

```sh
CHATT_AUDIO_SIM_SEEDS=8 cargo test -q -p chatt \
  seeded_short_real_talkspurts_preserve_output_integrity -- --ignored --nocapture
```

The 32 runs cover LAN, regional Ethernet, mobile handoff and random 60% loss.
Assertions require actual suppression, reordering in the lossy profiles,
continued output and finite, non-clipped audio. Measurements are printed per
seed; the test does not invent a latency ceiling for a lossy link. The observed
maximum playout estimates were 61 ms on LAN/regional Ethernet, 171–393 ms under
mobile handoff, and 231–322 ms under 60% loss. Higher delays and NetEQ's own late
packet discards under loss are not evidence of runaway buffering by themselves.

## Findings and limits

The first differential run exposed two Rust differences and retained their
counterexamples before correction:

1. A longer same-timestamp arrival replaced the previous history entry. The
   reference's `map::emplace` keeps that entry. With `H 0 480`, `T 1`, `H 0 960`,
   Rust raised maximum delay to 10 ms while C++ kept it at zero. Replacement
   also leaves the original minimum entry behind after history-window expiry:
   append `T 250`, `H 120000 960` to the example and the old baseline survives
   despite no longer being in history. Clock drift can then accumulate against
   that expired baseline instead of just the two-second window. Rust now preserves
   the original entry and upstream's return value.
2. The pre-existing receive-gap reset hid measured network delay in 17/1,000
   initial network cases. It inferred a source/timing discontinuity from a gap
   of at least 1 s and excess delay of at least 250 ms. Ordinary reordered short
   talkspurts triggered it. The C++ controller has no such reset. It is removed;
   a codec-reset flag alone does not erase media-clock delay evidence.

These are demonstrated differences, not proof of the reported incident's cause.
No 200 ms speech cutoff, emergency prefix trim, silence-driven histogram aging,
or extra estimator reset was added to match the tests.

There is an audio tradeoff to retain in the evidence: the existing 30-second
DRED handoff simulation changes from 25 acceleration operations / 3,636 removed
samples with the old replacement behavior to 83 operations / 11,849 removed
samples with upstream behavior. Peak playout estimates are 168 and 166 ms,
respectively, and both end at an 80 ms target. The old test's 45-operation ceiling
was a proxy for whether the reorder optimizer received late primaries; it also
passed with defective history. That proxy is replaced by a direct late-primary
observation regression (220 ms arrival delay, checked against C++), a history
expiry/clock-drift invariant, and an end-to-end target-adaptation check. The
simulation still prints the time-scaling measurements. These changes do not
establish perceptually improved audio quality under handoff.
As a negative control, temporarily restoring the old replacement behavior made
the clock-drift regression fail at packet 201; restoring upstream behavior passes.

Finite seeded testing cannot prove all possible reorderings. Exhaustiveness is
limited to the stated eight-item permutations. The seeded tests are deterministic
property/differential stress tests, not coverage-guided fuzzing. C++ sanitizers do
not instrument Rust or Chatt's native Opus/device pipeline. Inputs are within
Chatt's supported 48 kHz speech domain; CNG/DTMF, unbounded history cardinality,
and sequence-number width differences are outside this comparison. The
controller tests align Chatt's supplied arrival clock with upstream ticks and
use upstream's 80 ms startup target. Status tests accumulate generated-noise
samples during Expand and clear them for decode modes. They compare decisions,
not the independently exposed playout-delay diagnostic. Chatt's existing wall-clock adaptation, noise timestamp accounting,
silence-resume filtering and source-timeline handling are not thereby certified
as upstream-identical. The DRED-specific lower-timestamp lookup has no equivalent
in the standard upstream packet-buffer API; its rollover regression is a
separate Chatt test.

The network oracle uses no algorithm stubs. The separate DSP oracle retains its
existing telemetry and unused RFC3389 comfort-noise stubs in `stubs.cc`; its
scripted speech operation tape never enters the comfort-noise branch.

No packet trace from the affected wired session was available. See
[the capture guide](../../docs/audio-latency.md) to collect one before resetting.
The existing `replay_repro` harness can replay a report's packet fixture with
`CHATT_PACKET_FIXTURE_IN` and `CHATT_REPLAY_OUT` to compare receiver changes using
fixed captured packet timing and Opus payloads.
