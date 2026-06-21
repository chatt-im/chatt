use opus_codec::AlignedBuffer;
use opus_codec::error::Error;
use opus_codec::projection::{
    ProjectionDecoder, ProjectionDecoderRef, ProjectionEncoder, ProjectionEncoderRef,
};
use opus_codec::types::{Application, SampleRate};

#[test]
fn test_projection_ambisonics() {
    // First Order Ambisonics (4 channels) with Family 3 (Ambisonics)
    let channels = 4;
    let mapping_family = 3;
    let mut encoder = ProjectionEncoder::new(
        SampleRate::Hz48000,
        channels,
        mapping_family,
        Application::Audio,
    )
    .unwrap();

    let demixing_matrix = encoder.demixing_matrix_bytes().unwrap();
    assert!(!demixing_matrix.is_empty());

    let mut decoder = ProjectionDecoder::new(
        SampleRate::Hz48000,
        channels,
        encoder.streams(),
        encoder.coupled_streams(),
        &demixing_matrix,
    )
    .unwrap();

    let frame_size = 960;
    let pcm_in = vec![0i16; frame_size * channels as usize];
    let mut packet = [0u8; 1500];
    let mut pcm_out = vec![0i16; frame_size * channels as usize];

    let len = encoder.encode(&pcm_in, frame_size, &mut packet).unwrap();
    assert!(len > 0);

    let decoded_len = decoder
        .decode(&packet[..len], &mut pcm_out, frame_size, false)
        .unwrap();
    assert_eq!(decoded_len, frame_size);
}

#[test]
fn test_init_in_place_unowned_projection() {
    let sr = SampleRate::Hz48000;
    let channels = 4;
    let mapping_family = 3;
    let frame_size = 960;

    let enc_size = ProjectionEncoder::size(channels, mapping_family).unwrap();
    let mut enc_buf = AlignedBuffer::with_capacity_bytes(enc_size);
    let enc_ptr = enc_buf.as_mut_ptr();
    let (streams, coupled) = unsafe {
        ProjectionEncoder::init_in_place(enc_ptr, sr, channels, mapping_family, Application::Audio)
            .unwrap()
    };
    let mut encoder =
        unsafe { ProjectionEncoderRef::from_raw(enc_ptr, sr, channels, streams, coupled) };

    let demixing = encoder.demixing_matrix_bytes().unwrap();
    let dec_size = ProjectionDecoder::size(channels, streams, coupled).unwrap();
    let mut dec_buf = AlignedBuffer::with_capacity_bytes(dec_size);
    let dec_ptr = dec_buf.as_mut_ptr();
    unsafe {
        ProjectionDecoder::init_in_place(dec_ptr, sr, channels, streams, coupled, &demixing)
            .unwrap();
    }
    let mut decoder =
        unsafe { ProjectionDecoderRef::from_raw(dec_ptr, sr, channels, streams, coupled) };

    let pcm_in = vec![0i16; frame_size * channels as usize];
    let mut packet = vec![0u8; 4000];
    let len = encoder.encode(&pcm_in, frame_size, &mut packet).unwrap();
    assert!(len > 0);

    let mut pcm_out = vec![0i16; frame_size * channels as usize];
    let decoded = decoder
        .decode(&packet[..len], &mut pcm_out, frame_size, false)
        .unwrap();
    assert_eq!(decoded, frame_size);
}

#[test]
fn test_init_in_place_invalid_demixing_matrix() {
    let sr = SampleRate::Hz48000;
    let channels = 4;
    let streams = 2;
    let coupled_streams = 2;
    let size = ProjectionDecoder::size(channels, streams, coupled_streams).unwrap();
    let mut buf = AlignedBuffer::with_capacity_bytes(size);
    let ptr = buf.as_mut_ptr();
    let err = unsafe {
        ProjectionDecoder::init_in_place(ptr, sr, channels, streams, coupled_streams, &[])
    }
    .unwrap_err();
    assert_eq!(err, Error::BadArg);
}
