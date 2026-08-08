use std::{
    os::unix::net::UnixStream,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use local_rpc::{
    frame::{DaemonFrame, StateDelta, StateEvent},
    ids::RoomId,
    model::{BulkTransferId, DaemonInstanceId},
    unix::{FrameReader, FrameWriter},
};

const WARMUP_SAMPLES: usize = 20;
const MEASURED_SAMPLES: usize = 250;
const PAYLOAD_SIZES: &[usize] = &[64 * 1024, 192 * 1024, local_rpc::MAX_CHUNK_BYTES];
/// Frames a single daemon runtime tick broadcasts back to back: the snapshot or
/// voice roster plus the settings, appearance, and identity events.
const CONTROL_BURST: usize = 4;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (writer_stream, reader_stream) = UnixStream::pair()?;
    let (delivered_tx, delivered_rx) = mpsc::sync_channel(CONTROL_BURST);
    let reader_thread = thread::Builder::new()
        .name("local-rpc-benchmark-reader".into())
        .spawn(move || {
            let mut reader = FrameReader::new(reader_stream);
            loop {
                match reader.recv_daemon_with_fds_and_bulk(|_, bytes| {
                    delivered_tx
                        .send(Ok(bytes.len()))
                        .map_err(|_| std::io::ErrorKind::BrokenPipe.into())
                }) {
                    Ok(None) => {}
                    Ok(Some(_)) => {
                        if delivered_tx.send(Ok(0)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = delivered_tx.send(Err(error.to_string()));
                        break;
                    }
                }
            }
        })?;
    let mut writer = FrameWriter::new(writer_stream);

    // `--burst` keeps the bulk rows out of the run so `perf stat` and
    // `strace -c` count only the control-frame path; `--queued` and `--live`
    // narrow it further to one send shape, so the two can be attributed apart.
    let arguments = std::env::args().collect::<Vec<_>>();
    let selected = |name: &str| arguments.iter().any(|argument| argument == name);
    let modes: &[(&str, bool)] = match (selected("--queued"), selected("--live")) {
        (true, false) => &[("queued", true)],
        (false, true) => &[("live", false)],
        _ => &[("queued", true), ("live", false)],
    };
    let payload_sizes = match selected("--burst") || modes.len() == 1 {
        true => &[][..],
        false => PAYLOAD_SIZES,
    };
    println!(
        "persistent local Unix RPC delivery (frame + socket + borrowed decode), {} measured samples",
        MEASURED_SAMPLES
    );
    println!("payload       p50       p95       min      mean   p95 frames@120Hz");
    for (index, &payload_bytes) in payload_sizes.iter().enumerate() {
        let transfer_id = BulkTransferId((index + 1) as u64);
        let payload = vec![0x5a; payload_bytes];
        for _ in 0..WARMUP_SAMPLES {
            deliver(&mut writer, &delivered_rx, transfer_id, &payload)?;
        }
        let mut samples = Vec::with_capacity(MEASURED_SAMPLES);
        for _ in 0..MEASURED_SAMPLES {
            let started = Instant::now();
            deliver(&mut writer, &delivered_rx, transfer_id, &payload)?;
            samples.push(started.elapsed());
        }
        report(payload_bytes, &mut samples);
    }

    // A runtime tick's worth of small control frames, the shape the renderer
    // actually sees between attachments. `queued` sends the accumulated burst
    // the way the daemon's writer thread does; `live` sends a frame at a time,
    // which is what it costs when a sender cannot coalesce.
    let burst = control_burst_bytes()?;
    println!(
        "\ncontrol burst: {CONTROL_BURST} frames, {} B framed ({} B/frame)",
        burst.len(),
        burst.len() / CONTROL_BURST
    );
    println!("mode          p50       p95       min      mean");
    for &(label, queued) in modes {
        for _ in 0..WARMUP_SAMPLES {
            deliver_burst(&mut writer, &delivered_rx, &burst, queued)?;
        }
        let mut samples = Vec::with_capacity(MEASURED_SAMPLES);
        for _ in 0..MEASURED_SAMPLES {
            let started = Instant::now();
            deliver_burst(&mut writer, &delivered_rx, &burst, queued)?;
            samples.push(started.elapsed());
        }
        report_burst(label, &mut samples);
    }

    drop(writer);
    reader_thread
        .join()
        .map_err(|_| "local RPC benchmark reader panicked")?;
    Ok(())
}

fn control_frame(seq: u64) -> DaemonFrame {
    DaemonFrame::Event(StateEvent {
        instance_id: DaemonInstanceId([7; 16]),
        event_seq: seq,
        delta: StateDelta::RoomUnreadChanged {
            room_id: RoomId(9),
            unread: seq as u32,
            behind_head: true,
        },
    })
}

/// The whole burst pre-framed, the way the daemon's outbound buffer holds a
/// tick's worth of broadcasts before the writer thread sends them.
fn control_burst_bytes() -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    for seq in 1..=CONTROL_BURST as u64 {
        let payload = local_rpc::frame::encode_daemon(&control_frame(seq))?;
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&payload);
    }
    Ok(bytes)
}

fn deliver_burst(
    writer: &mut FrameWriter,
    delivered: &mpsc::Receiver<Result<usize, String>>,
    burst: &[u8],
    queued: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if queued {
        writer.send_frames(burst, usize::MAX)?;
    } else {
        for seq in 1..=CONTROL_BURST as u64 {
            writer.send_daemon(&control_frame(seq))?;
        }
    }
    for _ in 0..CONTROL_BURST {
        delivered.recv().map_err(|error| error.to_string())??;
    }
    Ok(())
}

fn deliver(
    writer: &mut FrameWriter,
    delivered: &mpsc::Receiver<Result<usize, String>>,
    transfer_id: BulkTransferId,
    payload: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    writer.send_daemon_bulk_chunk(transfer_id, payload)?;
    let actual = delivered.recv().map_err(|error| error.to_string())??;
    if actual != payload.len() {
        return Err(format!("decoded {actual} payload bytes; expected {}", payload.len()).into());
    }
    Ok(())
}

fn report(payload_bytes: usize, samples: &mut [Duration]) {
    samples.sort_unstable();
    let p50 = percentile(samples, 0.50);
    let p95 = percentile(samples, 0.95);
    let min = samples[0];
    let mean = samples.iter().map(Duration::as_nanos).sum::<u128>() / samples.len() as u128;
    let frame_ns = 1_000_000_000f64 / 120.0;
    println!(
        "{:>7} KiB  {:>7.1} us {:>7.1} us {:>7.1} us {:>7.1} us {:>10.3}",
        payload_bytes / 1024,
        micros(p50),
        micros(p95),
        micros(min),
        mean as f64 / 1_000.0,
        p95.as_nanos() as f64 / frame_ns,
    );
}

fn report_burst(label: &str, samples: &mut [Duration]) {
    samples.sort_unstable();
    let mean = samples.iter().map(Duration::as_nanos).sum::<u128>() / samples.len() as u128;
    println!(
        "{label:<8} {:>7.1} us {:>7.1} us {:>7.1} us {:>7.1} us",
        micros(percentile(samples, 0.50)),
        micros(percentile(samples, 0.95)),
        micros(samples[0]),
        mean as f64 / 1_000.0,
    );
}

fn percentile(samples: &[Duration], fraction: f64) -> Duration {
    let index = ((samples.len() as f64 * fraction).ceil() as usize)
        .saturating_sub(1)
        .min(samples.len() - 1);
    samples[index]
}

fn micros(duration: Duration) -> f64 {
    duration.as_nanos() as f64 / 1_000.0
}
