use opus_codec::encoder::Encoder;
use opus_codec::error::{Error, Result};
use opus_codec::multistream::MultistreamEncoder;
use opus_codec::packet::{
    multistream_packet_pad, multistream_packet_unpad, packet_pad, packet_unpad,
};
use opus_codec::types::{Application, Channels, SampleRate};
use opus_codec::{OPUS_INTERNAL_ERROR, opus_multistream_packet_pad, opus_packet_pad};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

const SINGLE_RANDOM_PACKET_CASES: usize = 24;
const SINGLE_RANDOM_PACKET_STEPS: usize = 8;
const EXTENSION_PACKET_CASES: usize = 24;
const EXTENSION_PACKET_STEPS: usize = 6;
const MULTISTREAM_PACKET_CASES: usize = 22;
const MULTISTREAM_PACKET_STEPS: usize = 8;
const RANDOMIZED_PAD_PARITY_TOTAL_CASES: usize = SINGLE_RANDOM_PACKET_CASES
    * SINGLE_RANDOM_PACKET_STEPS
    + EXTENSION_PACKET_CASES * EXTENSION_PACKET_STEPS
    + MULTISTREAM_PACKET_CASES * MULTISTREAM_PACKET_STEPS;
const _: [(); 512] = [(); RANDOMIZED_PAD_PARITY_TOTAL_CASES];

fn usize_below(rng: &mut impl Rng, upper_exclusive: usize) -> usize {
    if upper_exclusive <= 1 {
        0
    } else {
        (rng.next_u32() as usize) % upper_exclusive
    }
}

fn i16_sample(rng: &mut impl Rng) -> i16 {
    rng.next_u32() as i16
}

fn c_pad_result(code: i32) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(Error::from_code(code))
    }
}

fn assert_packet_pad_matches_opus(packet: &mut [u8], len: usize, new_len: usize) {
    let mut c_packet = packet.to_vec();
    let c_result = c_pad_result(unsafe {
        opus_packet_pad(
            c_packet.as_mut_ptr(),
            i32::try_from(len).unwrap(),
            i32::try_from(new_len).unwrap(),
        )
    });
    let rust_result = packet_pad(packet, len, new_len);
    assert_eq!(rust_result, c_result);
    if rust_result.is_ok() {
        assert_eq!(&packet[..new_len], &c_packet[..new_len]);
    }
}

fn assert_multistream_packet_pad_matches_opus(
    packet: &mut [u8],
    len: usize,
    new_len: usize,
    nb_streams: i32,
) {
    let mut c_packet = packet.to_vec();
    let c_result = c_pad_result(unsafe {
        opus_multistream_packet_pad(
            c_packet.as_mut_ptr(),
            i32::try_from(len).unwrap(),
            i32::try_from(new_len).unwrap(),
            nb_streams,
        )
    });
    let rust_result = multistream_packet_pad(packet, len, new_len, nb_streams);
    assert_eq!(rust_result, c_result);
    if rust_result.is_ok() {
        assert_eq!(&packet[..new_len], &c_packet[..new_len]);
    }
}

fn packet_with_valid_extensions() -> Vec<u8> {
    let payload = [
        0x7B, 0x41, 16, 0x00, 67, 7, b'a', b'b', b'c', b'd', b'e', b'f', b'g', 200, b'u', b'v',
        b'w', b'x', b'y', b'z',
    ];
    let mut packet = vec![0u8; 40];
    packet[..payload.len()].copy_from_slice(&payload);
    packet
}

fn packet_with_malformed_extension_len() -> Vec<u8> {
    let payload = [
        0x7B, 0x41, 16, 0x00, 67, 255, b'a', b'b', b'c', b'd', b'e', b'f', b'g', 200, b'u', b'v',
        b'w', b'x', b'y', b'z',
    ];
    let mut packet = vec![0u8; 40];
    packet[..payload.len()].copy_from_slice(&payload);
    packet
}

fn packet_with_short_id_two_extension_in_padding() -> Vec<u8> {
    let payload = [0x7B, 0x41, 2, 0x00, 0x05, b'a'];
    let mut packet = vec![0u8; 10];
    packet[..payload.len()].copy_from_slice(&payload);
    packet
}

#[test]
fn packet_pad_handles_repadding_large_packet() {
    let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
    let pcm = vec![0i16; 960];
    let mut encoded_packet = [0u8; 512];
    let encoded_len = encoder.encode(&pcm, &mut encoded_packet).unwrap();

    let mut packet = vec![0u8; 70_001];
    packet[..encoded_len].copy_from_slice(&encoded_packet[..encoded_len]);
    let original = packet[..encoded_len].to_vec();

    packet_pad(&mut packet, encoded_len, 70_000).unwrap();
    packet_pad(&mut packet, 70_000, 70_001).unwrap();
    assert_eq!(packet_unpad(&mut packet, 70_001).unwrap(), encoded_len);
    assert_eq!(&packet[..encoded_len], original.as_slice());
}

#[test]
fn packet_pad_rejects_zero_len_even_when_noop() {
    let mut packet = [0u8; 1];
    let err = packet_pad(&mut packet, 0, 0).unwrap_err();
    assert_eq!(err, Error::BadArg);
}

#[test]
fn packet_unpad_rejects_zero_len() {
    let mut packet = [0u8; 1];
    let err = packet_unpad(&mut packet, 0).unwrap_err();
    assert_eq!(err, Error::BadArg);
}

#[test]
fn packet_pad_matches_opus_for_existing_extensions() {
    let mut packet = packet_with_valid_extensions();
    assert_packet_pad_matches_opus(&mut packet, 20, 40);
}

#[test]
fn packet_pad_matches_opus_internal_error_for_malformed_extensions() {
    let mut packet = packet_with_malformed_extension_len();
    let mut c_packet = packet.clone();

    let c_result = unsafe { opus_packet_pad(c_packet.as_mut_ptr(), 20, 40) };
    assert_eq!(c_result, OPUS_INTERNAL_ERROR);
    assert_eq!(packet_pad(&mut packet, 20, 40), Err(Error::InternalError));
}

#[test]
fn packet_pad_matches_opus_for_short_id_two_extension_in_padding() {
    let mut packet = packet_with_short_id_two_extension_in_padding();
    assert_packet_pad_matches_opus(&mut packet, 6, 10);
}

#[test]
fn packet_pad_matches_opus_randomized_repadding() {
    let mut rng = StdRng::seed_from_u64(0x5EED_CAFE);
    let frame_sizes = [120usize, 240, 480, 960, 1920, 2880];

    for _ in 0..SINGLE_RANDOM_PACKET_CASES {
        let frame_size = frame_sizes[usize_below(&mut rng, frame_sizes.len())];
        let mut encoder =
            Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
        let pcm: Vec<i16> = (0..frame_size).map(|_| i16_sample(&mut rng)).collect();
        let mut encoded_packet = [0u8; 1500];
        let encoded_len = encoder.encode(&pcm, &mut encoded_packet).unwrap();

        let mut packet = vec![0u8; encoded_len + 1024];
        packet[..encoded_len].copy_from_slice(&encoded_packet[..encoded_len]);
        let mut len = encoded_len;

        for _ in 0..SINGLE_RANDOM_PACKET_STEPS {
            let remaining = packet.len() - len;
            let growth = usize_below(&mut rng, remaining + 1);
            let new_len = len + growth;
            assert_packet_pad_matches_opus(&mut packet, len, new_len);
            len = new_len;
        }
    }
}

#[test]
fn packet_pad_matches_opus_randomized_existing_extensions() {
    let mut rng = StdRng::seed_from_u64(0xA11C_E55E);

    for _ in 0..EXTENSION_PACKET_CASES {
        let mut packet = vec![0u8; 160];
        let base = packet_with_valid_extensions();
        packet[..20].copy_from_slice(&base[..20]);
        let mut len = 20usize;

        for _ in 0..EXTENSION_PACKET_STEPS {
            let remaining = packet.len() - len;
            let growth = usize_below(&mut rng, remaining + 1);
            let new_len = len + growth;
            assert_packet_pad_matches_opus(&mut packet, len, new_len);
            len = new_len;
        }
    }
}

#[test]
fn multistream_pad_rejects_invalid_stream_count() {
    let mut packet = vec![0u8; 8];
    assert_eq!(
        multistream_packet_pad(&mut packet, 1, 2, 0).unwrap_err(),
        Error::BadArg
    );
    assert_eq!(
        multistream_packet_pad(&mut packet, 1, 2, i32::MIN).unwrap_err(),
        Error::BadArg
    );
}

#[test]
fn multistream_pad_rejects_zero_len_even_when_noop() {
    let mut packet = [0u8; 1];
    let err = multistream_packet_pad(&mut packet, 0, 0, 1).unwrap_err();
    assert_eq!(err, Error::BadArg);
}

#[test]
fn multistream_pad_matches_opus_for_existing_extensions() {
    let first_stream = [0x00, 0x01, 0x00];
    let second_stream = &packet_with_valid_extensions()[..20];
    let mut packet = vec![0u8; 50];
    packet[..first_stream.len()].copy_from_slice(&first_stream);
    packet[first_stream.len()..first_stream.len() + second_stream.len()]
        .copy_from_slice(second_stream);
    assert_multistream_packet_pad_matches_opus(&mut packet, 23, 50, 2);
}

#[test]
fn multistream_pad_matches_opus_internal_error_for_malformed_extensions() {
    let mut packet = packet_with_malformed_extension_len();
    let mut c_packet = packet.clone();

    let c_result = unsafe { opus_multistream_packet_pad(c_packet.as_mut_ptr(), 20, 40, 1) };
    assert_eq!(c_result, OPUS_INTERNAL_ERROR);
    assert_eq!(
        multistream_packet_pad(&mut packet, 20, 40, 1),
        Err(Error::InternalError)
    );
}

#[test]
fn multistream_unpad_rejects_invalid_stream_count() {
    let mut packet = vec![0u8; 8];
    assert_eq!(
        multistream_packet_unpad(&mut packet, 1, 0).unwrap_err(),
        Error::BadArg
    );
    assert_eq!(
        multistream_packet_unpad(&mut packet, 1, i32::MIN).unwrap_err(),
        Error::BadArg
    );
}

#[test]
fn multistream_unpad_rejects_zero_len() {
    let mut packet = [0u8; 1];
    let err = multistream_packet_unpad(&mut packet, 0, 1).unwrap_err();
    assert_eq!(err, Error::BadArg);
}

#[test]
fn multistream_packet_pad_matches_opus_randomized_repadding() {
    let mut rng = StdRng::seed_from_u64(0xC0DE_BAAD);
    let frame_sizes = [120usize, 240, 480, 960];

    for _ in 0..MULTISTREAM_PACKET_CASES {
        let frame_size = frame_sizes[usize_below(&mut rng, frame_sizes.len())];
        let channels = 6usize;
        let (mut encoder, _) = MultistreamEncoder::new_surround(
            SampleRate::Hz48000,
            channels as u8,
            1,
            Application::Audio,
        )
        .unwrap();
        let nb_streams = i32::from(encoder.streams());
        let pcm: Vec<i16> = (0..frame_size * channels)
            .map(|_| i16_sample(&mut rng))
            .collect();
        let mut packet = vec![0u8; 4096];
        let len = encoder.encode(&pcm, frame_size, &mut packet).unwrap();
        let mut len = len;

        for _ in 0..MULTISTREAM_PACKET_STEPS {
            let remaining = packet.len() - len;
            let growth = usize_below(&mut rng, remaining + 1);
            let new_len = len + growth;
            assert_multistream_packet_pad_matches_opus(&mut packet, len, new_len, nb_streams);
            len = new_len;
        }
    }
}
