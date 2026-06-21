# Latency and Bandwidth Effects of Live Audio Optimizations

This report quantifies the current live-audio latency features using the
sample-backed simulated-time tests in `src/audio.rs`. The tests never sleep and
drive the real capture gate, silence range tracking, playback silence skip,
adaptive playback stream, and mixer with synthetic time.

Speech input is decoded from `assets/sample-001.opus`; generated audio is only
used for silence. Scenario tests cycle through 10 ms frames from the sample,
while direct listening exports use the full 40.62 s decoded sample.

## Measures

- `sent packets`: 20 ms Opus packets queued for playback. This is the test
  bandwidth measure. It is a packet-rate and payload-work proxy, not an exact
  wire-byte count.
- `max queue`: maximum per-stream queued playback latency.
- `avg queue`: queue-area divided by simulated duration.
- `skip`: playback silence removed from an already queued backlog.
- `suppressed`: capture frames suppressed after the long-silence gate.
- `reordered`: delivered packets that arrived behind a higher sequence number.
- `late`: reordered packets rejected after the jitter-buffer deadline.
- `missing`: gaps emitted by the jitter buffer and recovered by DRED or PLC.

All reported runs had `0` non-finite samples and `0` clipped samples.

## Single-Stream Results

| Scenario | Features | Sent packets | Packet reduction | Max queue | Avg queue | Notes |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| Constant speech, 60 s | all on | 3003 | 0.0% | 97 ms | 63.4 ms | Optimizations stay effectively inert. |
| Constant speech, 60 s | all off | 3003 | baseline | 100 ms | 94.9 ms | No silence, no backlog, no bandwidth gain expected. |
| Alternating speech/silence, 45 s | capture gate on | 1554 | 31.0% | 97 ms | 20.6 ms | 1425 input frames suppressed. |
| Alternating speech/silence, 45 s | capture gate off | 2253 | baseline | 97 ms | 65.0 ms | Silence is still packetized. |
| Lossy alternating speech/silence, 60 s | capture gate on | 2185 | 27.2% | 150 ms | 55.2 ms | 1671 frames suppressed; 5 DRED and 319 PLC recoveries. |
| Lossy alternating speech/silence, 60 s | capture gate off | 3003 | baseline | 170 ms | 109.3 ms | 62 DRED and 485 PLC recoveries. |

Takeaway: capture silence gating is the main bandwidth win. It helps when a
speaker is silent for longer than the 2 s default gate threshold. It does not
change constant speech.

## Backlog Recovery

The backlog scenario starts playback with a silence-heavy queue, then resumes
sampled speech. This isolates receiver-side latency recovery.

| Features | Max queue | Avg queue | Skip | Resample corrections | Improvement vs all off |
| --- | ---: | ---: | ---: | ---: | --- |
| silence skip + adaptive catch-up | 182 ms | 91.6 ms | 355 ms / 2 cuts | 8 | max -66.3%, avg -82.9% |
| adaptive only | 536 ms | 199.1 ms | 0 ms | 23 | max -0.7%, avg -62.7% |
| silence skip only | 180 ms | 174.8 ms | 360 ms / 2 cuts | 0 | max -66.7%, avg -67.3% |
| both off | 540 ms | 534.3 ms | 0 ms | 0 | baseline |

Takeaway: playback silence skip is what quickly removes a silence backlog. The
adaptive resampler then reduces the remaining queue-area without dropping
speech. Together they turn a 540 ms queued delay into about 92 ms average queue
and 182 ms peak queue in this test.

## Packet Loss, Reordering, and Silence

The lossy alternating scenario drops packets deterministically through the same
simulated network/jitter-buffer path used by the realistic profiles. With
capture gating enabled, fewer silence packets are sent and therefore fewer
silence packets can be dropped: total recovery count fell from 547 to 324.
Capture gating also cut average queue from 109.3 ms to 55.2 ms in this case by
removing long silent stretches before the receiver ever buffers them.

For explicit dropped-silence backlog behavior, the unit test
`dropped_packets_can_extend_marked_silence_for_skip` queues PLC silence and
verifies that marked recovered silence can still be skipped when it contributes
to excess latency.

## Production Feedback and DRED Tuning

Live capture now starts with Opus DRED enabled instead of waiting for loss to be
observed first. The production encoder keeps the configured bitrate fixed,
enables in-band FEC, sets a 100 x 10 ms DRED duration, and starts with
`OPUS_SET_PACKET_LOSS_PERC` at 20%. Receiver feedback can raise only the Opus
expected-loss percentage to 35%, 50%, or 60%; it never raises or lowers the
configured bitrate as a congestion response.

Feedback is generated at the receiver from jitter-buffer and playback state:
expected packets, missing packets, late packets, duplicates, reordered packets,
receiver queue, and inter-arrival jitter. The feedback window is emitted every
500 ms or 25 expected packets. Server-relayed feedback is forwarded only to the
active owner of the stream in the same room; direct P2P feedback is sent over
the matching authenticated peer connection.

Latency reporting intentionally does not assume synchronized clocks between
clients. The reported queue is receiver-local playout/mixer backlog, and
inter-arrival jitter is computed from local `Instant` deltas against the 20 ms
packet cadence:

```text
abs(actual_arrival_delta - sequence_delta * 20ms)
```

No sender wall-clock timestamp is used to estimate one-way latency.

## Packet Loss Profiles

The current simulation also supports named network profiles. `mild_random`,
`moderate_random`, `severe_random`, `random_30`, `random_45`, and `random_60`
are independent per-packet drops, while `bursty_wifi`, `congested_wifi`, and
`mobile_handoff` are Gilbert-Elliott profiles that alternate between a
mostly-good state and short bad bursts. All non-`none` named profiles also add
deterministic delivery delay variation, so they exercise both packet loss and
out-of-order arrival.

These rows use the 60 s lossy alternating speech/silence scenario with all
features enabled.

| Network profile | Effective loss | Reordered | Late | DRED | PLC | Max queue | Avg queue | RMS | Notes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| none | 0.00% | 0 | 0 | 0 | 0 | 97 ms | 21.0 ms | 0.02534 | Capture gate still dominates queue/bandwidth. |
| mild random | 2.01% | 83 | 0 | 0 | 44 | 110 ms | 46.9 ms | 0.02496 | Low loss, but delayed packets already exercise reordering. |
| moderate random | 6.13% | 136 | 21 | 0 | 155 | 140 ms | 55.6 ms | 0.02478 | More recovery and reorder work, still bounded. |
| severe random | 15.70% | 190 | 26 | 3 | 369 | 210 ms | 63.4 ms | 0.02394 | Adjacent losses start getting partial DRED recovery. |
| random 30% | 31.30% | 177 | 44 | 9 | 728 | 270 ms | 66.2 ms | 0.01930 | High independent loss with mixed DRED/PLC recovery. |
| random 45% | 46.45% | 156 | 31 | 13 | 1045 | 290 ms | 71.7 ms | 0.01778 | Very sparse delivery, output remains finite and unclipped. |
| random 60% | 61.28% | 122 | 29 | 13 | 1367 | 370 ms | 95.8 ms | 0.01506 | Covers the requested 60% loss point. |
| bursty Wi-Fi | 1.42% | 306 | 67 | 1 | 98 | 150 ms | 52.8 ms | 0.02408 | Low effective drop rate, high reorder pressure. |
| congested Wi-Fi | 9.29% | 446 | 90 | 2 | 293 | 210 ms | 63.6 ms | 0.02246 | Bursty impairment with adjacent-gap DRED recovery. |
| mobile handoff | 1.65% | 233 | 227 | 4 | 263 | 290 ms | 65.9 ms | 0.02318 | Rare longer delays produce many late arrivals. |

Takeaway: realistic network impairment changes both PLC work and jitter-buffer
behavior. Reordered packets are common even when the effective loss rate is low,
and long-delay profiles can turn delayed packets into late/missing frames. The
hard queue bound remained intact through 61.28% loss and the worst realistic
profile measured here (`mobile_handoff`, 290 ms max queue).

## Group Chat

| Scenario | Streams | Sent packets | Packet reduction | Active streams | Max queue | Avg queue | Peak |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Group chat, all on, 45 s | 3 | 6612 | 2.2% | 3 | 110 ms | 102.8 ms | 0.22666 |
| Group chat, gate off, 45 s | 3 | 6759 | baseline | 3 | 110 ms | 103.9 ms | 0.21767 |
| Group chat, all on, 45 s | 6 | 13171 | n/a | 6 | 170 ms | 115.9 ms | 0.26596 |

Takeaway: the mixer stays bounded and unclipped under multiple sampled inputs.
The bandwidth gain is small in this particular group pattern because most
per-user silent windows do not exceed the long-silence gate threshold by much.
The queue figures come from the scenario's default per-stream packet loss,
reordering, and jitter-buffer recovery; even with six sampled inputs, the queue
stayed well below the 1.5 s hard bound.

## When Each Feature Helps

- Capture silence gate: helps bandwidth when local input has long silence
  stretches. It reduced sent packets by 31.0% in alternating speech/silence and
  27.2% under the same pattern with packet drops, while cutting lossy average
  queue from 109.3 ms to 55.2 ms.
- Playback silence skip: helps latency when already-buffered marked silence is
  part of a backlog. It removed 355 ms of queued silence and cut max queue from
  540 ms to 182 ms when combined with adaptive catch-up.
- Adaptive catch-up: helps drain non-silent backlog smoothly. Alone it reduced
  average queue from 534.3 ms to 199.1 ms, but it did not materially reduce peak
  queue without silence skip.
- Packet loss and reordering: bounded by the hard queue limit. The worst
  measured realistic profile reached 290 ms max queue; independent 61.28% loss
  reached 370 ms max queue with 13 DRED recoveries and 1367 PLC fallbacks in
  the silence-heavy lossy scenario.
- Constant speech: no meaningful bandwidth improvement is expected or observed;
  latency stays bounded near the 60 ms target without loss.
- Group chat: latency remains bounded per stream and the soft limiter prevents
  clipping with 3 and 6 sampled inputs.

## Direct Sample Listening Exports

The exporter also runs the full decoded `assets/sample-001.opus` PCM directly
through the live encoder, network simulator, client jitter buffer, Opus/DRED
decoder, and playback mixer. This path does not insert artificial silence or
pre-chop the source. It writes the pre-network input and reconstructed client
audio so the results can be inspected by ear.

| Export | Lost | Reordered | Late | DRED | PLC | Max queue | Avg queue | RMS | Peak | Max delta |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `direct-client-reconstructed-none.wav` | 0 | 0 | 0 | 0 | 0 | 50 ms | 45.0 ms | 0.05421 | 0.50677 | 0.09070 |
| `direct-client-reconstructed-congested-wifi.wav` | 197 | 418 | 84 | 181 | 281 | 210 ms | 142.7 ms | 0.04991 | 0.49890 | 0.27692 |
| `direct-client-reconstructed-random-60.wav` | 1247 | 109 | 24 | 481 | 1270 | 410 ms | 213.4 ms | 0.03614 | 0.49338 | 0.42798 |

For the direct full-file export, 20 ms Opus packets and current DRED settings
usually expose 720 samples of DRED for an adjacent missing packet. The client
therefore reconstructs that gap as a 240-sample PLC prefix plus a 720-sample
DRED suffix. The JSONL traces record `requested_offset_samples`,
`parsed_offset_samples`, `status: "partial"`, `plc_decode`, `dred_decode`,
and every output window so artifact timestamps can be correlated to receiver
decisions.

## Reproduction

Validation commands used:

```sh
cargo check -q --workspace
cargo test -q --workspace
cargo test -p benchmark -- --nocapture profiles_encode_packets
cargo run -q -p benchmark -- live/call_sim --param scenario=constant_speech --param feature=all_on --param loss=none
cargo run -q -p benchmark -- live/call_sim --param scenario=lossy_speech --param feature=all_on --param loss=random_60
cargo run --release -q -p benchmark --example export_live_audio
```

The live profiling routes are suitable for `samply`:

```sh
samply record cargo run --release -p benchmark -- live/call_sim --param scenario=lossy_speech --param loss=congested_wifi
samply record cargo run --release -p benchmark -- live/call_sim --param scenario=lossy_speech --param loss=random_60
samply record cargo run --release -p benchmark -- live/group_call_sim --param streams=3 --param loss=bursty_wifi
```

To listen to the same sample-backed pipeline, export WAVs with:

```sh
cargo run --release -p benchmark --example export_live_audio
```

This writes `target/live-audio/input-pre-network.wav`,
`target/live-audio/client-reconstructed-none.wav`,
`target/live-audio/client-reconstructed-congested-wifi.wav`,
`target/live-audio/client-reconstructed-random-60.wav`,
`target/live-audio/direct-input-pre-network.wav`,
`target/live-audio/direct-client-reconstructed-none.wav`,
`target/live-audio/direct-client-reconstructed-congested-wifi.wav`,
`target/live-audio/direct-client-reconstructed-random-60.wav`, and the matching
`target/live-audio/direct-trace-*.jsonl` files.
