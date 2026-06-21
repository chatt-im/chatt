use opus_codec::AlignedBuffer;
use opus_codec::decoder::{Decoder, DecoderRef};
use opus_codec::encoder::{Encoder, EncoderRef};
use opus_codec::error::Error;
use opus_codec::types::{Application, Channels, SampleRate};

#[test]
fn test_encode_decode() {
    let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
    let mut decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();

    let frame_size = 960; // 20ms
    let pcm_in = vec![0i16; frame_size];
    let mut packet = [0u8; 500];
    let mut pcm_out = vec![0i16; frame_size];

    let len = encoder.encode(&pcm_in, &mut packet).unwrap();
    assert!(len > 0);

    let decoded_len = decoder.decode(&packet[..len], &mut pcm_out, false).unwrap();
    assert_eq!(decoded_len, frame_size);
}

#[test]
fn test_float_api() {
    let mut encoder =
        Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio).unwrap();
    let mut decoder = Decoder::new(SampleRate::Hz48000, Channels::Stereo).unwrap();

    let frame_size = 480; // 10ms
    let pcm_in = vec![0.0f32; frame_size * 2];
    let mut packet = [0u8; 500];
    let mut pcm_out = vec![0.0f32; frame_size * 2];

    let len = encoder.encode_float(&pcm_in, &mut packet).unwrap();
    assert!(len > 0);

    let decoded_len = decoder
        .decode_float(&packet[..len], &mut pcm_out, false)
        .unwrap();
    assert_eq!(decoded_len, frame_size);
}

#[test]
fn test_buffer_empty() {
    let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
    let pcm = vec![0i16; 960];
    let mut empty_buf = [0u8; 0];

    // The wrapper should catch this and return BadArg before calling libopus
    let result = encoder.encode(&pcm, &mut empty_buf);
    assert_eq!(result, Err(Error::BadArg));
}

#[test]
fn test_packet_samples_rejects_empty_packet() {
    let decoder = Decoder::new(SampleRate::Hz48000, Channels::Mono).unwrap();
    let result = decoder.packet_samples(&[]);
    assert_eq!(result, Err(Error::BadArg));
}

#[test]
fn test_init_in_place_alignment_checks() {
    let sr = SampleRate::Hz48000;
    let channels = Channels::Mono;

    let enc_size = Encoder::size(channels).unwrap();
    let mut enc_buf = vec![0u8; enc_size + 1];
    let enc_ptr = unsafe { enc_buf.as_mut_ptr().add(1) };
    let err = unsafe { Encoder::init_in_place(enc_ptr.cast(), sr, channels, Application::Voip) }
        .unwrap_err();
    assert_eq!(err, Error::BadArg);

    let dec_size = Decoder::size(channels).unwrap();
    let mut dec_buf = vec![0u8; dec_size + 1];
    let dec_ptr = unsafe { dec_buf.as_mut_ptr().add(1) };
    let err = unsafe { Decoder::init_in_place(dec_ptr.cast(), sr, channels) }.unwrap_err();
    assert_eq!(err, Error::BadArg);
}

#[test]
fn test_init_in_place_unowned_encoder_decoder() {
    let sr = SampleRate::Hz48000;
    let channels = Channels::Mono;
    let frame_size = 960;

    let enc_size = Encoder::size(channels).unwrap();
    let mut enc_buf = AlignedBuffer::with_capacity_bytes(enc_size);
    let enc_ptr = enc_buf.as_mut_ptr();
    unsafe {
        Encoder::init_in_place(enc_ptr, sr, channels, Application::Voip).unwrap();
    }
    let mut encoder = unsafe { EncoderRef::from_raw(enc_ptr, sr, channels) };

    let dec_size = Decoder::size(channels).unwrap();
    let mut dec_buf = AlignedBuffer::with_capacity_bytes(dec_size);
    let dec_ptr = dec_buf.as_mut_ptr();
    unsafe {
        Decoder::init_in_place(dec_ptr, sr, channels).unwrap();
    }
    let mut decoder = unsafe { DecoderRef::from_raw(dec_ptr, sr, channels) };

    let pcm = vec![0i16; frame_size];
    let mut packet = [0u8; 500];
    let len = encoder.encode(&pcm, &mut packet).unwrap();
    assert!(len > 0);

    let mut out = vec![0i16; frame_size];
    let decoded = decoder.decode(&packet[..len], &mut out, false).unwrap();
    assert_eq!(decoded, frame_size);
}
