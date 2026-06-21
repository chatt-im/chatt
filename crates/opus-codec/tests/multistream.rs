use opus_codec::AlignedBuffer;
use opus_codec::error::Error;
use opus_codec::max_frame_samples_for;
use opus_codec::multistream::{
    Mapping, MultistreamDecoder, MultistreamDecoderRef, MultistreamEncoder, MultistreamEncoderRef,
};
use opus_codec::packet::{multistream_packet_pad, multistream_packet_unpad};
use opus_codec::types::{Application, SampleRate};

#[test]
fn test_multistream_surround() {
    // 5.1 Surround: 6 channels
    let channels = 6;
    let mapping_family = 1; // Family 1 is for surround
    let (mut encoder, _) = MultistreamEncoder::new_surround(
        SampleRate::Hz48000,
        channels,
        mapping_family,
        Application::Audio,
    )
    .unwrap();

    let streams = encoder.streams();
    let coupled = encoder.coupled_streams();
    let mapping_table = [0, 1, 2, 3, 4, 5]; // Standard identity mapping for the streams

    let mapping = Mapping {
        channels,
        streams,
        coupled_streams: coupled,
        mapping: &mapping_table,
    };

    let mut decoder = MultistreamDecoder::new(SampleRate::Hz48000, mapping).unwrap();

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
fn test_multistream_frame_size_validation() {
    let mapping_table = [0u8];
    let mapping = Mapping {
        channels: 1,
        streams: 1,
        coupled_streams: 0,
        mapping: &mapping_table,
    };
    let mut encoder =
        MultistreamEncoder::new(SampleRate::Hz48000, Application::Audio, mapping).unwrap();
    let mut decoder = MultistreamDecoder::new(SampleRate::Hz48000, mapping).unwrap();
    let mut out = [0u8; 100];

    assert_eq!(encoder.encode(&[], 0, &mut out), Err(Error::BadArg));
    let too_large = max_frame_samples_for(SampleRate::Hz48000) + 1;
    assert_eq!(encoder.encode(&[], too_large, &mut out), Err(Error::BadArg));

    let mut pcm_out: [i16; 0] = [];
    assert_eq!(
        decoder.decode(&[], &mut pcm_out, 0, false),
        Err(Error::BadArg)
    );
    assert_eq!(
        decoder.decode(&[], &mut pcm_out, too_large, false),
        Err(Error::BadArg)
    );
}

#[test]
fn test_init_in_place_unowned_multistream() {
    let sr = SampleRate::Hz48000;
    let frame_size = 960;
    let mapping_table = [0u8, 1u8];
    let mapping = Mapping {
        channels: 2,
        streams: 1,
        coupled_streams: 1,
        mapping: &mapping_table,
    };

    let enc_size = MultistreamEncoder::size(mapping.streams, mapping.coupled_streams).unwrap();
    let mut enc_buf = AlignedBuffer::with_capacity_bytes(enc_size);
    let enc_ptr = enc_buf.as_mut_ptr();
    unsafe {
        MultistreamEncoder::init_in_place(enc_ptr, sr, Application::Audio, mapping).unwrap();
    }
    let mut encoder = unsafe { MultistreamEncoderRef::from_raw(enc_ptr, sr, mapping) };

    let dec_size = MultistreamDecoder::size(mapping.streams, mapping.coupled_streams).unwrap();
    let mut dec_buf = AlignedBuffer::with_capacity_bytes(dec_size);
    let dec_ptr = dec_buf.as_mut_ptr();
    unsafe {
        MultistreamDecoder::init_in_place(dec_ptr, sr, mapping).unwrap();
    }
    let mut decoder = unsafe { MultistreamDecoderRef::from_raw(dec_ptr, sr, mapping) };

    let mut pcm = vec![0i16; frame_size * mapping.channels as usize];
    for (i, sample) in pcm.iter_mut().enumerate() {
        *sample = ((i as i32 * 17) % 2000) as i16;
    }
    let mut packet = vec![0u8; 4000];
    let len = encoder.encode(&pcm, frame_size, &mut packet).unwrap();
    assert!(len > 0);

    let mut out = vec![0i16; frame_size * mapping.channels as usize];
    let decoded = decoder
        .decode(&packet[..len], &mut out, frame_size, false)
        .unwrap();
    assert_eq!(decoded, frame_size);
}

#[test]
fn test_init_in_place_invalid_mapping() {
    let sr = SampleRate::Hz48000;
    let mapping_table = [0u8];
    let mapping = Mapping {
        channels: 2,
        streams: 1,
        coupled_streams: 1,
        mapping: &mapping_table,
    };

    let enc_size = MultistreamEncoder::size(mapping.streams, mapping.coupled_streams).unwrap();
    let mut enc_buf = AlignedBuffer::with_capacity_bytes(enc_size);
    let enc_ptr = enc_buf.as_mut_ptr();
    let err =
        unsafe { MultistreamEncoder::init_in_place(enc_ptr, sr, Application::Audio, mapping) }
            .unwrap_err();
    assert_eq!(err, Error::BadArg);

    let dec_size = MultistreamDecoder::size(mapping.streams, mapping.coupled_streams).unwrap();
    let mut dec_buf = AlignedBuffer::with_capacity_bytes(dec_size);
    let dec_ptr = dec_buf.as_mut_ptr();
    let err = unsafe { MultistreamDecoder::init_in_place(dec_ptr, sr, mapping) }.unwrap_err();
    assert_eq!(err, Error::BadArg);
}

#[test]
fn test_multistream_packet_pad_handles_repadding_large_packet() {
    let channels = 6;
    let frame_size = 960;
    let (mut encoder, _) =
        MultistreamEncoder::new_surround(SampleRate::Hz48000, channels, 1, Application::Audio)
            .unwrap();

    let pcm = vec![0i16; frame_size * channels as usize];
    let mut packet = vec![0u8; 70_001];
    let len = encoder.encode(&pcm, frame_size, &mut packet).unwrap();
    let original = packet[..len].to_vec();
    let nb_streams = i32::from(encoder.streams());

    multistream_packet_pad(&mut packet, len, 70_000, nb_streams).unwrap();
    multistream_packet_pad(&mut packet, 70_000, 70_001, nb_streams).unwrap();
    assert_eq!(
        multistream_packet_unpad(&mut packet, 70_001, nb_streams).unwrap(),
        len
    );
    assert_eq!(&packet[..len], original.as_slice());
}
