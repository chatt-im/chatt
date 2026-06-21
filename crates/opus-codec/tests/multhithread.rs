use opus_codec::{Application, Channels, Decoder, Encoder, SampleRate};
use std::sync::Arc;
use std::thread;

fn assert_send_sync<T: Send + Sync>() {}

fn pcm_frame() -> Vec<i16> {
    const FRAME: usize = 960; // 20 ms @ 48 kHz per channel
    let samples_per_frame = FRAME * Channels::Stereo.as_usize();
    (0..samples_per_frame)
        .map(|i| ((i * 17) as i16).wrapping_sub(16000))
        .collect()
}

#[test]
fn encoder_is_send_sync() {
    assert_send_sync::<Encoder>();
}

#[test]
fn decoder_is_send_sync() {
    assert_send_sync::<Decoder>();
}

#[test]
fn encoder_multithread_smoke() {
    const THREADS: usize = 4;
    const ITERATIONS: usize = 16;
    let sr = SampleRate::Hz48000;
    let frame = Arc::new(pcm_frame());

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let mut encoder =
                Encoder::new(sr, Channels::Stereo, Application::Audio).expect("create encoder");
            let frame = Arc::clone(&frame);
            thread::spawn(move || {
                let mut packet = vec![0u8; 4096];
                for _ in 0..ITERATIONS {
                    encoder
                        .encode(frame.as_slice(), &mut packet)
                        .expect("encode frame");
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("encoder thread");
    }
}

#[test]
fn decoder_multithread_smoke() {
    const THREADS: usize = 4;
    const ITERATIONS: usize = 16;
    let sr = SampleRate::Hz48000;
    let frame = pcm_frame();
    let out_samples = frame.len();
    let mut encoder = Encoder::new(sr, Channels::Stereo, Application::Audio).expect("encoder");
    let mut packet = vec![0u8; 4096];
    let produced = encoder
        .encode(frame.as_slice(), &mut packet)
        .expect("encode reference frame");
    packet.truncate(produced);
    let packet = Arc::new(packet);

    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let mut decoder = Decoder::new(sr, Channels::Stereo).expect("decoder");
            let packet = Arc::clone(&packet);
            thread::spawn(move || {
                let mut output = vec![0i16; out_samples];
                for _ in 0..ITERATIONS {
                    decoder
                        .decode(packet.as_slice(), &mut output, false)
                        .expect("decode frame");
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("decoder thread");
    }
}
