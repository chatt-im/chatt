//! Basic example demonstrating Opus codec encoding and decoding

use opus_codec::{Application, Bitrate, Channels, Decoder, Encoder, SampleRate};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Opus Codec Basic Example");
    println!("========================");

    // Create an encoder for voice communication
    let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)?;
    println!(
        "✓ Created encoder: {} Hz, {} channel(s), {:?} application",
        encoder.sample_rate().as_i32(),
        encoder.channels().as_usize(),
        Application::Voip
    );

    // Create a decoder
    let mut decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono)?;
    println!(
        "✓ Created decoder: {} Hz, {} channel(s)",
        decoder.sample_rate().as_i32(),
        decoder.channels().as_usize()
    );

    // Generate some test audio data (sine wave)
    let frequency = 440.0; // A4 note
    let sample_rate = 48000.0;
    let duration_ms = 20; // 20ms frame
    let num_samples = (duration_ms as f32 * sample_rate / 1000.0) as usize;

    let mut input_pcm = vec![0i16; num_samples];
    for (i, s) in input_pcm.iter_mut().enumerate().take(num_samples) {
        let t = i as f32 / sample_rate;
        let sample = (frequency * 2.0 * std::f32::consts::PI * t).sin();
        *s = (sample * i16::MAX as f32 * 0.1) as i16; // 10% volume
    }

    println!("✓ Generated {} samples of test audio", input_pcm.len());

    // Encode the audio
    let mut output = vec![0u8; 4000]; // Output buffer
    let encoded_size = encoder.encode(&input_pcm, &mut output)?;
    println!(
        "✓ Encoded {} bytes (compression ratio: {:.2})",
        encoded_size,
        input_pcm.len() as f32 * 2.0 / encoded_size as f32
    );

    // Decode the audio
    let mut decoded_pcm = vec![0i16; num_samples];
    let decoded_samples = decoder.decode(&output[..encoded_size], &mut decoded_pcm, false)?;
    println!("✓ Decoded {} samples", decoded_samples);

    // Calculate RMS error
    let mut sum_squared_error = 0.0;
    for i in 0..num_samples.min(decoded_samples) {
        let error = input_pcm[i] as f32 - decoded_pcm[i] as f32;
        sum_squared_error += error * error;
    }
    let rms_error = (sum_squared_error / num_samples as f32).sqrt();
    println!("✓ RMS reconstruction error: {:.2}", rms_error);

    // Show bitrate information
    let bitrate = encoder.bitrate()?;
    match bitrate {
        Bitrate::Auto => println!("✓ Bitrate: Auto"),
        Bitrate::Max => println!("✓ Bitrate: Max"),
        Bitrate::Custom(bps) => {
            println!("✓ Bitrate: {} bps ({:.1} kbps)", bps, bps as f32 / 1000.0)
        }
    }

    // Show complexity
    let complexity = encoder.complexity()?;
    println!("✓ Encoder complexity: {}", complexity.value());

    // Show VBR status
    let vbr_enabled = encoder.vbr()?;
    println!(
        "✓ Variable bitrate: {}",
        if vbr_enabled { "enabled" } else { "disabled" }
    );

    println!("\nExample completed successfully!");
    Ok(())
}
