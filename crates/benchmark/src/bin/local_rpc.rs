use std::{
    os::unix::net::UnixStream,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use local_rpc::{
    bulk::BulkChunk,
    frame::DaemonFrame,
    model::BulkTransferId,
    unix::{FrameReader, FrameWriter},
};

const WARMUP_SAMPLES: usize = 20;
const MEASURED_SAMPLES: usize = 250;
const PAYLOAD_SIZES: &[usize] = &[64 * 1024, 192 * 1024, local_rpc::MAX_CHUNK_BYTES];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (writer_stream, reader_stream) = UnixStream::pair()?;
    let (delivered_tx, delivered_rx) = mpsc::sync_channel(1);
    let reader_thread = thread::Builder::new()
        .name("local-rpc-benchmark-reader".into())
        .spawn(move || {
            let mut reader = FrameReader::new(reader_stream);
            loop {
                match reader.recv_daemon() {
                    Ok(DaemonFrame::BulkChunk(chunk)) => {
                        if delivered_tx.send(Ok(chunk.bytes.len())).is_err() {
                            break;
                        }
                    }
                    Ok(frame) => {
                        let _ =
                            delivered_tx.send(Err(format!("unexpected daemon frame: {frame:?}")));
                        break;
                    }
                    Err(error) => {
                        let _ = delivered_tx.send(Err(error.to_string()));
                        break;
                    }
                }
            }
        })?;
    let mut writer = FrameWriter::new(writer_stream);

    println!(
        "persistent local Unix RPC delivery (serialize + socket + decode), {} measured samples",
        MEASURED_SAMPLES
    );
    println!("payload       p50       p95       min      mean   p95 frames@120Hz");
    for (index, &payload_bytes) in PAYLOAD_SIZES.iter().enumerate() {
        let frame = DaemonFrame::BulkChunk(BulkChunk {
            transfer_id: BulkTransferId((index + 1) as u64),
            bytes: vec![0x5a; payload_bytes],
        });
        for _ in 0..WARMUP_SAMPLES {
            deliver(&mut writer, &delivered_rx, &frame, payload_bytes)?;
        }
        let mut samples = Vec::with_capacity(MEASURED_SAMPLES);
        for _ in 0..MEASURED_SAMPLES {
            let started = Instant::now();
            deliver(&mut writer, &delivered_rx, &frame, payload_bytes)?;
            samples.push(started.elapsed());
        }
        report(payload_bytes, &mut samples);
    }

    drop(writer);
    reader_thread
        .join()
        .map_err(|_| "local RPC benchmark reader panicked")?;
    Ok(())
}

fn deliver(
    writer: &mut FrameWriter,
    delivered: &mpsc::Receiver<Result<usize, String>>,
    frame: &DaemonFrame,
    expected: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    writer.send_daemon(frame)?;
    let actual = delivered.recv().map_err(|error| error.to_string())??;
    if actual != expected {
        return Err(format!("decoded {actual} payload bytes; expected {expected}").into());
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

fn percentile(samples: &[Duration], fraction: f64) -> Duration {
    let index = ((samples.len() as f64 * fraction).ceil() as usize)
        .saturating_sub(1)
        .min(samples.len() - 1);
    samples[index]
}

fn micros(duration: Duration) -> f64 {
    duration.as_nanos() as f64 / 1_000.0
}
