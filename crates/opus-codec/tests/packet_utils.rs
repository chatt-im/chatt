use opus_codec::encoder::Encoder;
use opus_codec::error::Error;
use opus_codec::packet::{
    packet_bandwidth, packet_channels, packet_frame_count, packet_parse, packet_sample_count,
    soft_clip,
};
use opus_codec::types::{Application, Bandwidth, Channels, SampleRate};

#[test]
fn test_packet_analysis() {
    // Create a silent packet
    let mut encoder =
        Encoder::new(SampleRate::Hz48000, Channels::Stereo, Application::Audio).unwrap();
    let pcm = vec![0i16; 960 * 2]; // 20ms stereo
    let mut output = [0u8; 100];
    let len = encoder.encode(&pcm, &mut output).unwrap();
    let packet = &output[..len];

    // Analyze
    assert!(packet_frame_count(packet).unwrap() > 0);
    assert_eq!(
        packet_sample_count(packet, SampleRate::Hz48000).unwrap(),
        960
    );
    assert_eq!(packet_channels(packet).unwrap(), Channels::Stereo);
    assert!(packet_bandwidth(packet).unwrap() != Bandwidth::Narrowband); // Likely Fullband for Audio app

    // Parse
    let (_toc, _offset, frames) = packet_parse(packet).unwrap();
    assert!(!frames.is_empty());
}

#[test]
fn test_packet_parse_keeps_zero_length_frames() {
    let packet = [0x02u8, 0x01, 0x00];
    let (_toc, offset, frames) = packet_parse(&packet).unwrap();
    assert_eq!(offset, 2);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].len(), 1);
    assert!(frames[1].is_empty());
}

#[test]
fn test_soft_clip_validations() {
    let mut pcm = vec![1.5f32; 4];
    let mut state = vec![0f32; 2];
    assert!(soft_clip(&mut pcm, 2, 2, &mut state).is_ok());

    let mut short_pcm = vec![1.5f32; 3];
    assert_eq!(
        soft_clip(&mut short_pcm, 2, 2, &mut state),
        Err(Error::BadArg)
    );

    let mut pcm = vec![1.5f32; 4];
    let mut too_small_state = vec![0f32; 1];
    assert_eq!(
        soft_clip(&mut pcm, 2, 2, &mut too_small_state),
        Err(Error::BadArg)
    );

    let mut pcm = vec![1.5f32; 4];
    assert_eq!(soft_clip(&mut pcm, 2, -1, &mut state), Err(Error::BadArg));
}
