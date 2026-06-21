use opus_codec::{
    Application, Bitrate, SampleRate,
    projection::{ProjectionDecoder, ProjectionEncoder},
};

const FRAME: usize = 960; // 20 ms @ 48 kHz
const MAPPING_FAMILY: i32 = 3;
const CHANNELS: u8 = 16;

#[test]
fn projection_roundtrip_basic() {
    let sr = SampleRate::Hz48000;
    let mut encoder = match ProjectionEncoder::new(sr, CHANNELS, MAPPING_FAMILY, Application::Audio)
    {
        Ok(enc) => enc,
        Err(opus_codec::Error::Unimplemented) => return,
        Err(err) => panic!("failed to create projection encoder: {err:?}"),
    };

    let target_bitrate = 64_000 * i32::from(encoder.streams() + encoder.coupled_streams());
    encoder
        .set_bitrate(Bitrate::Custom(target_bitrate))
        .expect("set bitrate");

    let demixing = encoder
        .demixing_matrix_bytes()
        .expect("demixing matrix bytes");

    let mut decoder = ProjectionDecoder::new(
        sr,
        CHANNELS,
        encoder.streams(),
        encoder.coupled_streams(),
        &demixing,
    )
    .expect("projection decoder");

    let mut pcm = vec![0i16; FRAME * CHANNELS as usize];
    for (i, sample) in pcm.iter_mut().enumerate() {
        *sample = (((i as i32 * 47) % 30_000) - 15_000) as i16;
    }

    let mut packet = vec![0u8; 4000];
    let bytes = encoder
        .encode(&pcm, FRAME, &mut packet)
        .expect("encode projection");
    assert!(bytes > 0);

    let mut out = vec![0i16; FRAME * CHANNELS as usize];
    let decoded = decoder
        .decode(&packet[..bytes], &mut out, FRAME, false)
        .expect("decode projection");
    assert_eq!(decoded, FRAME);
}

#[test]
fn projection_demixing_matrix_ctl_consistency() {
    let sr = SampleRate::Hz48000;
    let mut encoder = match ProjectionEncoder::new(sr, CHANNELS, MAPPING_FAMILY, Application::Audio)
    {
        Ok(enc) => enc,
        Err(opus_codec::Error::Unimplemented) => return,
        Err(err) => panic!("failed to create projection encoder: {err:?}"),
    };

    let size = encoder
        .demixing_matrix_size()
        .expect("demixing matrix size");
    assert!(size > 0, "demixing matrix size must be positive");
    let size_usize = usize::try_from(size).expect("size fits in usize");

    let _gain = encoder
        .demixing_matrix_gain()
        .expect("demixing matrix gain");

    let mut buffer = vec![0u8; size_usize];
    let written = encoder
        .write_demixing_matrix(&mut buffer)
        .expect("write demixing matrix");
    assert_eq!(written, size_usize);

    let from_bytes = encoder
        .demixing_matrix_bytes()
        .expect("demixing matrix bytes");
    assert_eq!(from_bytes.len(), size_usize);
    assert_eq!(from_bytes, buffer);

    // Ensure the matrix we obtained through the CTLs can actually seed a decoder
    ProjectionDecoder::new(
        sr,
        CHANNELS,
        encoder.streams(),
        encoder.coupled_streams(),
        &from_bytes,
    )
    .expect("projection decoder from CTLs");
}
