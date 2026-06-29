from __future__ import annotations

import argparse
import csv
import json
import math
import shutil
from collections import Counter
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path
from typing import Any

import matplotlib

matplotlib.use("Agg")

import matplotlib.pyplot as plt
import numpy as np
import soundfile as sf
from scipy import ndimage, signal


SR = 48_000
START_MSG = "live playback WAV recording started"
STOP_MSG = "live playback WAV recording stopped"
AUDIO_PREFIX = "audio pop"
EVENT_WINDOW_MS = 100.0
CLIP_PRE_MS = 100.0
CLIP_POST_MS = 150.0
PLOT_PRE_MS = 80.0
PLOT_POST_MS = 120.0


@dataclass(frozen=True)
class Source:
    name: str
    wav: Path
    log: Path


@dataclass
class Candidate:
    source: str
    rank: int
    sample_index: int
    time_s: float
    timestamp: datetime
    score: float
    hp_z: float
    d1_z: float
    d2_z: float
    residual_z: float
    sample: float
    abs_sample: float
    local_peak: float
    local_rms: float
    local_crest: float
    max_delta_5ms: float
    nearest_event_source: str
    nearest_event_msg: str
    nearest_event_dt_ms: float
    event_counts_100ms: str


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Detect transient pop candidates in the Soundboard playback WAV and correlate JSON logs."
    )
    parser.add_argument("--out", type=Path, default=Path("/tmp/pop-analysis"))
    parser.add_argument("--top", type=int, default=80)
    parser.add_argument("--plots", type=int, default=20)
    parser.add_argument("--clips", type=int, default=20)
    parser.add_argument(
        "--score-floor",
        type=float,
        default=6.0,
        help="Minimum robust-z score for candidate peaks.",
    )
    parser.add_argument(
        "--min-distance-ms",
        type=float,
        default=20.0,
        help="Minimum spacing between candidate peaks after grouping.",
    )
    return parser.parse_args()


def parse_timestamp(value: str) -> datetime:
    if value.endswith("Z"):
        value = value[:-1] + "+00:00"
    parsed = datetime.fromisoformat(value)
    if parsed.tzinfo is None:
        return parsed.replace(tzinfo=UTC)
    return parsed.astimezone(UTC)


def fmt_timestamp(value: datetime) -> str:
    return value.astimezone(UTC).isoformat(timespec="microseconds").replace("+00:00", "Z")


def load_events(path: Path) -> list[dict[str, Any]]:
    events: list[dict[str, Any]] = []
    with path.open("r", encoding="utf-8") as handle:
        for line in handle:
            if line.strip():
                event = json.loads(line)
                event["_dt"] = parse_timestamp(event["timestamp"])
                events.append(event)
    return events


def recording_start(events: list[dict[str, Any]], wav: Path) -> datetime:
    wav_str = str(wav)
    for event in events:
        if event.get("msg") == START_MSG and event.get("path") == wav_str:
            return event["_dt"]
    for event in events:
        if event.get("msg") == START_MSG:
            return event["_dt"]
    raise RuntimeError(f"no '{START_MSG}' event found for {wav}")


def recording_stop(events: list[dict[str, Any]], wav: Path) -> dict[str, Any] | None:
    wav_str = str(wav)
    for event in events:
        if event.get("msg") == STOP_MSG and event.get("path") == wav_str:
            return event
    for event in events:
        if event.get("msg") == STOP_MSG:
            return event
    return None


def read_mono_wav(path: Path) -> tuple[np.ndarray, int]:
    data, sample_rate = sf.read(path, dtype="float32", always_2d=False)
    if data.ndim == 2:
        data = np.mean(data, axis=1, dtype=np.float32)
    return np.asarray(data, dtype=np.float32), int(sample_rate)


def robust_z(values: np.ndarray) -> np.ndarray:
    median = np.median(values)
    mad = np.median(np.abs(values - median))
    # Sparse/quiet recordings can make the MAD nearly zero and inflate every
    # high-frequency blip into a multi-million score. Use upper quantiles as a
    # conservative floor so the score remains comparable within a recording.
    q95 = np.quantile(values, 0.95)
    q99 = np.quantile(values, 0.99)
    scale = max(1.4826 * mad, q95 / 3.0, q99 / 6.0)
    if not math.isfinite(scale) or scale < 1e-12:
        scale = np.std(values)
    if not math.isfinite(scale) or scale < 1e-12:
        scale = 1.0
    return np.maximum((values - median) / scale, 0.0)


def transient_features(samples: np.ndarray, sample_rate: int) -> dict[str, np.ndarray]:
    sos = signal.butter(6, 1_500.0, btype="highpass", fs=sample_rate, output="sos")
    highpass = signal.sosfiltfilt(sos, samples).astype(np.float32)
    d1 = np.empty_like(samples)
    d1[0] = 0.0
    d1[1:] = np.abs(np.diff(samples))
    d2 = np.zeros_like(samples)
    d2[2:] = np.abs(samples[2:] - 2.0 * samples[1:-1] + samples[:-2])
    median = ndimage.median_filter(samples, size=31, mode="reflect")
    residual = np.abs(samples - median)
    return {
        "hp_abs": np.abs(highpass),
        "d1": d1,
        "d2": d2,
        "residual": residual,
        "highpass": highpass,
    }


def build_score(features: dict[str, np.ndarray]) -> tuple[np.ndarray, dict[str, np.ndarray]]:
    z = {
        "hp_z": robust_z(features["hp_abs"]),
        "d1_z": robust_z(features["d1"]),
        "d2_z": robust_z(features["d2"]),
        "residual_z": robust_z(features["residual"]),
    }
    score = np.maximum.reduce([z["hp_z"], z["d1_z"], z["d2_z"], z["residual_z"]])
    return score, z


def local_metrics(samples: np.ndarray, index: int, sample_rate: int) -> dict[str, float]:
    radius_10ms = max(1, round(sample_rate * 0.010))
    radius_5ms = max(1, round(sample_rate * 0.005))
    left = max(0, index - radius_10ms)
    right = min(len(samples), index + radius_10ms + 1)
    window = samples[left:right]
    peak = float(np.max(np.abs(window))) if len(window) else 0.0
    rms = float(np.sqrt(np.mean(np.square(window, dtype=np.float64)))) if len(window) else 0.0
    crest = peak / max(rms, 1e-12)
    d_left = max(1, index - radius_5ms)
    d_right = min(len(samples), index + radius_5ms + 1)
    max_delta = (
        float(np.max(np.abs(np.diff(samples[d_left - 1 : d_right]))))
        if d_right > d_left
        else 0.0
    )
    return {
        "local_peak": peak,
        "local_rms": rms,
        "local_crest": crest,
        "max_delta_5ms": max_delta,
    }


def event_summary(
    timestamp: datetime,
    all_events: dict[str, list[dict[str, Any]]],
    window_ms: float,
) -> tuple[str, str, str, float, list[dict[str, Any]]]:
    window = timedelta(milliseconds=window_ms)
    nearest: tuple[float, str, dict[str, Any]] | None = None
    counts: Counter[str] = Counter()
    selected: list[dict[str, Any]] = []
    for source_name, events in all_events.items():
        for event in events:
            delta_ms = (event["_dt"] - timestamp).total_seconds() * 1000.0
            abs_delta = abs(delta_ms)
            if abs_delta <= window_ms:
                msg = str(event.get("msg", ""))
                counts[f"{source_name}:{msg}"] += 1
                if msg.startswith(AUDIO_PREFIX):
                    compact = {
                        "source": source_name,
                        "dt_ms": round(delta_ms, 3),
                        "timestamp": event["timestamp"],
                        "msg": msg,
                    }
                    for key in (
                        "sequence",
                        "stream_id",
                        "transition_id",
                        "flags",
                        "flag_mute",
                        "flag_opus_reset",
                        "flag_silence_hint",
                        "flag_silence_resume",
                        "muted",
                        "media_muted",
                        "control_muted",
                        "sender_muted",
                        "mute_gain",
                        "mute_target",
                        "playout_delay_ms",
                        "packet_max_delta",
                        "output_max_delta",
                        "first_sample",
                        "last_sample",
                        "peak",
                        "rms",
                        "missing",
                        "covered",
                        "span_len",
                    ):
                        if key in event:
                            compact[key] = event[key]
                    selected.append(compact)
            if nearest is None or abs_delta < nearest[0]:
                nearest = (abs_delta, source_name, event)
    selected.sort(key=lambda item: abs(float(item["dt_ms"])))
    selected = selected[:30]
    if nearest is None:
        return "", "", "", math.nan, selected
    _, nearest_source, nearest_event = nearest
    nearest_dt_ms = (nearest_event["_dt"] - timestamp).total_seconds() * 1000.0
    counts_json = json.dumps(dict(counts.most_common(16)), sort_keys=True)
    return (
        counts_json,
        nearest_source,
        str(nearest_event.get("msg", "")),
        nearest_dt_ms,
        selected,
    )


def find_candidates(
    source: Source,
    events: list[dict[str, Any]],
    all_events: dict[str, list[dict[str, Any]]],
    top: int,
    score_floor: float,
    min_distance_ms: float,
) -> tuple[list[Candidate], dict[str, Any], np.ndarray, dict[str, np.ndarray]]:
    samples, sample_rate = read_mono_wav(source.wav)
    if sample_rate != SR:
        raise RuntimeError(f"{source.wav} sample rate is {sample_rate}, expected {SR}")
    start = recording_start(events, source.wav)
    stop = recording_stop(events, source.wav)
    features = transient_features(samples, sample_rate)
    score, z = build_score(features)

    ignore = round(sample_rate * 0.200)
    score[:ignore] = 0.0
    score[-ignore:] = 0.0
    distance = max(1, round(sample_rate * min_distance_ms / 1000.0))
    peaks, props = signal.find_peaks(score, height=score_floor, distance=distance)
    order = np.argsort(props["peak_heights"])[::-1]
    peaks = peaks[order[:top]]

    candidates: list[Candidate] = []
    windows_json: list[dict[str, Any]] = []
    for rank, index in enumerate(peaks, start=1):
        time_s = index / sample_rate
        timestamp = start + timedelta(seconds=time_s)
        metrics = local_metrics(samples, int(index), sample_rate)
        counts_json, nearest_source, nearest_msg, nearest_dt_ms, selected_events = event_summary(
            timestamp,
            all_events,
            EVENT_WINDOW_MS,
        )
        candidate = Candidate(
            source=source.name,
            rank=rank,
            sample_index=int(index),
            time_s=time_s,
            timestamp=timestamp,
            score=float(score[index]),
            hp_z=float(z["hp_z"][index]),
            d1_z=float(z["d1_z"][index]),
            d2_z=float(z["d2_z"][index]),
            residual_z=float(z["residual_z"][index]),
            sample=float(samples[index]),
            abs_sample=float(abs(samples[index])),
            local_peak=metrics["local_peak"],
            local_rms=metrics["local_rms"],
            local_crest=metrics["local_crest"],
            max_delta_5ms=metrics["max_delta_5ms"],
            nearest_event_source=nearest_source,
            nearest_event_msg=nearest_msg,
            nearest_event_dt_ms=nearest_dt_ms,
            event_counts_100ms=counts_json,
        )
        candidates.append(candidate)
        windows_json.append(
            {
                **candidate_to_json(candidate),
                "nearby_audio_events": selected_events,
            }
        )

    summary = {
        "source": source.name,
        "wav": str(source.wav),
        "log": str(source.log),
        "sample_rate": sample_rate,
        "samples": int(len(samples)),
        "duration_s": len(samples) / sample_rate,
        "recording_start": fmt_timestamp(start),
        "recording_stop": fmt_timestamp(stop["_dt"]) if stop else None,
        "recorder_reported_samples": stop.get("samples") if stop else None,
        "recorder_dropped_samples": stop.get("dropped_samples") if stop else None,
        "candidate_count": len(candidates),
        "score_floor": score_floor,
        "windows": windows_json,
    }
    return candidates, summary, samples, {"score": score, **features, **z}


def candidate_to_json(candidate: Candidate) -> dict[str, Any]:
    return {
        "source": candidate.source,
        "rank": candidate.rank,
        "sample_index": candidate.sample_index,
        "time_s": round(candidate.time_s, 9),
        "timestamp": fmt_timestamp(candidate.timestamp),
        "score": candidate.score,
        "hp_z": candidate.hp_z,
        "d1_z": candidate.d1_z,
        "d2_z": candidate.d2_z,
        "residual_z": candidate.residual_z,
        "sample": candidate.sample,
        "abs_sample": candidate.abs_sample,
        "local_peak": candidate.local_peak,
        "local_rms": candidate.local_rms,
        "local_crest": candidate.local_crest,
        "max_delta_5ms": candidate.max_delta_5ms,
        "nearest_event_source": candidate.nearest_event_source,
        "nearest_event_msg": candidate.nearest_event_msg,
        "nearest_event_dt_ms": candidate.nearest_event_dt_ms,
        "event_counts_100ms": json.loads(candidate.event_counts_100ms)
        if candidate.event_counts_100ms
        else {},
    }


def write_candidates_csv(path: Path, candidates: list[Candidate]) -> None:
    fieldnames = [
        "source",
        "rank",
        "sample_index",
        "time_s",
        "timestamp",
        "score",
        "hp_z",
        "d1_z",
        "d2_z",
        "residual_z",
        "sample",
        "abs_sample",
        "local_peak",
        "local_rms",
        "local_crest",
        "max_delta_5ms",
        "nearest_event_source",
        "nearest_event_msg",
        "nearest_event_dt_ms",
        "event_counts_100ms",
    ]
    with path.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=fieldnames)
        writer.writeheader()
        for candidate in candidates:
            writer.writerow(
                {
                    **candidate.__dict__,
                    "timestamp": fmt_timestamp(candidate.timestamp),
                }
            )


def write_review_clip(path: Path, samples: np.ndarray, index: int, sample_rate: int) -> None:
    pre = round(sample_rate * CLIP_PRE_MS / 1000.0)
    post = round(sample_rate * CLIP_POST_MS / 1000.0)
    left = max(0, index - pre)
    right = min(len(samples), index + post)
    sf.write(path, samples[left:right], sample_rate, subtype="FLOAT")


def write_plot(
    path: Path,
    samples: np.ndarray,
    diagnostics: dict[str, np.ndarray],
    candidate: Candidate,
    sample_rate: int,
) -> None:
    pre = round(sample_rate * PLOT_PRE_MS / 1000.0)
    post = round(sample_rate * PLOT_POST_MS / 1000.0)
    left = max(0, candidate.sample_index - pre)
    right = min(len(samples), candidate.sample_index + post)
    window = samples[left:right]
    t_ms = (np.arange(left, right) - candidate.sample_index) / sample_rate * 1000.0

    fig, axes = plt.subplots(3, 1, figsize=(12, 8), constrained_layout=True)
    axes[0].plot(t_ms, window, linewidth=0.8)
    axes[0].axvline(0.0, color="red", linewidth=0.8)
    axes[0].set_title(
        f"{candidate.source} candidate {candidate.rank}: "
        f"sample={candidate.sample_index} score={candidate.score:.1f}"
    )
    axes[0].set_ylabel("sample")

    axes[1].plot(t_ms, diagnostics["highpass"][left:right], linewidth=0.7, label="highpass")
    axes[1].plot(t_ms, diagnostics["score"][left:right], linewidth=0.7, label="score")
    axes[1].axvline(0.0, color="red", linewidth=0.8)
    axes[1].legend(loc="upper right")
    axes[1].set_ylabel("feature")

    _, _, _, image = axes[2].specgram(
        window,
        NFFT=512,
        Fs=sample_rate,
        noverlap=384,
        cmap="magma",
    )
    axes[2].axvline((candidate.sample_index - left) / sample_rate, color="cyan", linewidth=0.8)
    axes[2].set_ylabel("Hz")
    axes[2].set_xlabel("seconds in plot window")
    fig.colorbar(image, ax=axes[2], label="dB")
    fig.savefig(path, dpi=140)
    plt.close(fig)


def write_source_artifacts(
    out_dir: Path,
    source: Source,
    candidates: list[Candidate],
    samples: np.ndarray,
    diagnostics: dict[str, np.ndarray],
    sample_rate: int,
    plot_count: int,
    clip_count: int,
) -> None:
    clips_dir = out_dir / "clips"
    plots_dir = out_dir / "plots"
    clips_dir.mkdir(parents=True, exist_ok=True)
    plots_dir.mkdir(parents=True, exist_ok=True)
    for candidate in candidates[:clip_count]:
        write_review_clip(
            clips_dir / f"{source.name}_candidate_{candidate.rank:03d}.wav",
            samples,
            candidate.sample_index,
            sample_rate,
        )
    for candidate in candidates[:plot_count]:
        write_plot(
            plots_dir / f"{source.name}_candidate_{candidate.rank:03d}.png",
            samples,
            diagnostics,
            candidate,
            sample_rate,
        )


def write_summary(
    path: Path,
    sources: list[Source],
    summaries: list[dict[str, Any]],
    all_candidates: list[Candidate],
) -> None:
    top_soundboard = [c for c in all_candidates if c.source == "soundboard"][:20]
    lines = [
        "# Pop Candidate Analysis",
        "",
        "This is an audio-first candidate pass over the Soundboard playback WAV.",
        "It identifies likely short transients and correlates them to nearby JSON events.",
        "It is not a root-cause conclusion.",
        "",
        "## Inputs",
        "",
    ]
    for source in sources:
        lines.append(f"- `{source.wav}` with `{source.log}`")
    lines.extend(
        [
            "- `/tmp/chatt-alice.json` for Alice capture/send-side correlation only",
            "- `/tmp/chatt-server.json` for relay correlation only",
        ]
    )
    lines.extend(
        [
            "",
            "## Recording Summary",
            "",
            "| Source | Duration | Samples | Start UTC | Stop UTC | Dropped Recorder Samples | Candidates |",
            "| --- | ---: | ---: | --- | --- | ---: | ---: |",
        ]
    )
    for summary in summaries:
        lines.append(
            "| {source} | {duration:.3f}s | {samples} | {start} | {stop} | {dropped} | {candidates} |".format(
                source=summary["source"],
                duration=summary["duration_s"],
                samples=summary["samples"],
                start=summary["recording_start"],
                stop=summary["recording_stop"] or "",
                dropped=summary["recorder_dropped_samples"],
                candidates=summary["candidate_count"],
            )
        )
    lines.extend(
        [
            "",
            "## Primary Soundboard Candidates",
            "",
            "The Soundboard WAV is the primary symptom recording. Review clips are in",
            "`/tmp/pop-analysis/clips`, and plots are in `/tmp/pop-analysis/plots`.",
            "",
            "| Rank | Sample | Time | UTC | Score | Max Delta 5ms | Nearest Event | dt ms |",
            "| ---: | ---: | ---: | --- | ---: | ---: | --- | ---: |",
        ]
    )
    for candidate in top_soundboard:
        lines.append(
            "| {rank} | {sample} | {time:.6f}s | {timestamp} | {score:.1f} | {delta:.6f} | {event} | {dt:.3f} |".format(
                rank=candidate.rank,
                sample=candidate.sample_index,
                time=candidate.time_s,
                timestamp=fmt_timestamp(candidate.timestamp),
                score=candidate.score,
                delta=candidate.max_delta_5ms,
                event=f"{candidate.nearest_event_source}:{candidate.nearest_event_msg}",
                dt=candidate.nearest_event_dt_ms,
            )
        )
    lines.extend(
        [
            "",
            "## Output Files",
            "",
            "- `/tmp/pop-analysis/soundboard_candidates.csv`",
            "- `/tmp/pop-analysis/all_candidates.csv`",
            "- `/tmp/pop-analysis/candidate_windows.json`",
            "- `/tmp/pop-analysis/clips/*.wav`",
            "- `/tmp/pop-analysis/plots/*.png`",
            "",
            "Next manual step: listen to the top Soundboard clips and mark which candidates",
            "are actual audible pops. Then use `candidate_windows.json` to compare nearby",
            "capture, network, decode, and mixer events by timestamp, sequence, stream_id,",
            "and transition_id.",
        ]
    )
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def main() -> None:
    args = parse_args()
    out_dir: Path = args.out
    if out_dir.exists():
        shutil.rmtree(out_dir)
    out_dir.mkdir(parents=True)

    sources = [
        Source(
            "soundboard",
            Path("/tmp/chatt-soundboard-playback.wav"),
            Path("/tmp/chatt-soundboard.json"),
        ),
    ]
    all_events = {source.name: load_events(source.log) for source in sources}
    all_events["alice"] = load_events(Path("/tmp/chatt-alice.json"))
    all_events["server"] = load_events(Path("/tmp/chatt-server.json"))

    summaries: list[dict[str, Any]] = []
    all_candidates: list[Candidate] = []
    windows: list[dict[str, Any]] = []

    for source in sources:
        candidates, summary, samples, diagnostics = find_candidates(
            source,
            all_events[source.name],
            all_events,
            args.top,
            args.score_floor,
            args.min_distance_ms,
        )
        summaries.append({key: value for key, value in summary.items() if key != "windows"})
        windows.extend(summary["windows"])
        all_candidates.extend(candidates)
        write_candidates_csv(out_dir / f"{source.name}_candidates.csv", candidates)
        write_source_artifacts(
            out_dir,
            source,
            candidates,
            samples,
            diagnostics,
            SR,
            args.plots,
            args.clips,
        )

    all_candidates.sort(key=lambda candidate: (candidate.source, candidate.rank))
    write_candidates_csv(out_dir / "all_candidates.csv", all_candidates)
    (out_dir / "candidate_windows.json").write_text(
        json.dumps(
            {
                "event_window_ms": EVENT_WINDOW_MS,
                "clip_pre_ms": CLIP_PRE_MS,
                "clip_post_ms": CLIP_POST_MS,
                "summaries": summaries,
                "candidates": windows,
            },
            indent=2,
            sort_keys=True,
        )
        + "\n",
        encoding="utf-8",
    )
    write_summary(out_dir / "summary.md", sources, summaries, all_candidates)
    print(f"wrote {out_dir}")


if __name__ == "__main__":
    main()
