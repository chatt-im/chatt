use opus_codec::AlignedBuffer;
use opus_codec::encoder::Encoder;
use opus_codec::error::Error;
#[cfg(not(opus_codec_rust_packet_ops))]
use opus_codec::opus_repacketizer_out_range;
use opus_codec::packet::{packet_frame_count, packet_unpad};
use opus_codec::repacketizer::{Repacketizer, RepacketizerRef};
use opus_codec::types::{Application, Channels, SampleRate};
use opus_codec::{
    OpusRepacketizer, opus_repacketizer_cat, opus_repacketizer_create, opus_repacketizer_destroy,
    opus_repacketizer_get_nb_frames, opus_repacketizer_out,
};

const TOC_CONFIG: u8 = 0x78;
const PACKET_CODE_1: u8 = 0x01;
const PACKET_CODE_3: u8 = 0x03;
const CODE_3_PADDING_FLAG: u8 = 0x40;
#[cfg(opus_codec_rust_packet_ops)]
const FRAME_SEPARATOR_TO_NEXT: u8 = 0x02;
const FRAME_SEPARATOR_WITH_DELTA: u8 = 0x03;
const EXTENSION_ID_5_WITH_ONE_BYTE: u8 = 0x0B;
const EXTENSION_ID_33_WITH_MISSING_LENGTH_END: u8 = 0x43;

fn single_frame_packet(payload: u8) -> [u8; 2] {
    [TOC_CONFIG, payload]
}

fn three_frame_packet_with_extension_on_last_frame() -> [u8; 10] {
    const FRAME_COUNT: u8 = 3;
    const PADDING_LEN: u8 = 4;
    const LAST_FRAME_INDEX: u8 = 2;
    const EXTENSION_PAYLOAD: u8 = b'x';

    [
        TOC_CONFIG | PACKET_CODE_3,
        CODE_3_PADDING_FLAG | FRAME_COUNT,
        PADDING_LEN,
        b'a',
        b'b',
        b'c',
        FRAME_SEPARATOR_WITH_DELTA,
        LAST_FRAME_INDEX,
        EXTENSION_ID_5_WITH_ONE_BYTE,
        EXTENSION_PAYLOAD,
    ]
}

#[cfg(opus_codec_frame_bounded_extensions)]
fn three_frame_packet_with_repeated_extension_payloads() -> [u8; 11] {
    [
        TOC_CONFIG | PACKET_CODE_3,
        CODE_3_PADDING_FLAG | 3,
        5,
        b'a',
        b'b',
        b'c',
        EXTENSION_ID_5_WITH_ONE_BYTE,
        b'x',
        0x04,
        b'y',
        b'z',
    ]
}

fn two_frame_packet_with_malformed_extension_on_first_frame() -> [u8; 7] {
    const FRAME_COUNT: u8 = 2;
    const PADDING_LEN: u8 = 2;

    [
        TOC_CONFIG | PACKET_CODE_3,
        CODE_3_PADDING_FLAG | FRAME_COUNT,
        PADDING_LEN,
        b'a',
        b'b',
        EXTENSION_ID_33_WITH_MISSING_LENGTH_END,
        255,
    ]
}

fn raw_repacketizer_emit(packets: &[&[u8]]) -> Result<Vec<u8>, Error> {
    let rp = unsafe { opus_repacketizer_create() };
    assert!(!rp.is_null());

    for packet in packets {
        let result = unsafe { opus_repacketizer_cat(rp, packet.as_ptr(), packet.len() as i32) };
        assert_eq!(result, 0);
    }

    let mut out = [0u8; 128];
    let len = unsafe { opus_repacketizer_out(rp, out.as_mut_ptr(), out.len() as i32) };
    unsafe { opus_repacketizer_destroy(rp) };
    if len < 0 {
        return Err(Error::from_code(len));
    }
    Ok(out[..usize::try_from(len).unwrap()].to_vec())
}

#[cfg(not(opus_codec_rust_packet_ops))]
fn raw_repacketizer_emit_range(packets: &[&[u8]], begin: i32, end: i32) -> Result<Vec<u8>, Error> {
    let rp = unsafe { opus_repacketizer_create() };
    assert!(!rp.is_null());

    for packet in packets {
        let result = unsafe { opus_repacketizer_cat(rp, packet.as_ptr(), packet.len() as i32) };
        assert_eq!(result, 0);
    }

    let mut out = [0u8; 128];
    let len =
        unsafe { opus_repacketizer_out_range(rp, begin, end, out.as_mut_ptr(), out.len() as i32) };
    unsafe { opus_repacketizer_destroy(rp) };
    if len < 0 {
        return Err(Error::from_code(len));
    }
    Ok(out[..usize::try_from(len).unwrap()].to_vec())
}

#[cfg(not(opus_codec_rust_packet_ops))]
fn assert_repacketizer_result_matches_raw(
    actual: Result<usize, Error>,
    out: &[u8],
    expected: Result<Vec<u8>, Error>,
) {
    match expected {
        Ok(expected) => {
            let actual_len = actual.unwrap();
            assert_eq!(&out[..actual_len], expected.as_slice());
        }
        Err(expected) => assert_eq!(actual.unwrap_err(), expected),
    }
}

#[test]
fn test_repacketizer() {
    let mut rp = Repacketizer::new().unwrap();
    let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();

    // Create two 20ms frames
    let frame_size = 960;
    let pcm = vec![0i16; frame_size];
    let mut packet1 = [0u8; 200];
    let mut packet2 = [0u8; 200];

    let len1 = encoder.encode(&pcm, &mut packet1).unwrap();
    let len2 = encoder.encode(&pcm, &mut packet2).unwrap();

    // Add them to repacketizer
    rp.push(&packet1[..len1]).unwrap();
    rp.push(&packet2[..len2]).unwrap();

    // Verify we have 2 frames
    assert_eq!(rp.len(), 2);

    // Merge into one packet
    let mut merged = [0u8; 500];
    let merged_len = rp.emit(&mut merged).unwrap();
    assert!(merged_len > 0);

    // Verify the merged packet has 2 frames
    assert_eq!(packet_frame_count(&merged[..merged_len]).unwrap(), 2);
}

#[test]
fn test_init_in_place_null_repacketizer() {
    let err = unsafe { Repacketizer::init_in_place(std::ptr::null_mut()) }.unwrap_err();
    assert_eq!(err, Error::BadArg);
}

#[test]
fn test_init_in_place_unowned_repacketizer() {
    let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
    let frame_size = 960;
    let pcm = vec![0i16; frame_size];
    let mut packet = [0u8; 500];
    let len = encoder.encode(&pcm, &mut packet).unwrap();

    let rp_size = Repacketizer::size().unwrap();
    let mut rp_buf = AlignedBuffer::with_capacity_bytes(rp_size);
    let rp_ptr = rp_buf.as_mut_ptr();
    unsafe {
        Repacketizer::init_in_place(rp_ptr).unwrap();
    }
    let mut rp = unsafe { RepacketizerRef::from_raw(rp_ptr) };

    rp.push(&packet[..len]).unwrap();
    let mut out = [0u8; 500];
    let out_len = rp.emit(&mut out).unwrap();
    assert_eq!(&out[..out_len], &packet[..len]);
}

#[test]
fn test_repacketizer_owns_packet_data() {
    let mut encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip).unwrap();
    let mut rp = Repacketizer::new().unwrap();

    let frame_size = 960;
    let pcm = vec![0i16; frame_size];
    let mut packet = [0u8; 200];
    let len = encoder.encode(&pcm, &mut packet).unwrap();
    let original = packet[..len].to_vec();

    rp.push(&packet[..len]).unwrap();
    packet[..len].fill(0);

    let mut out = [0u8; 200];
    let out_len = rp.emit(&mut out).unwrap();
    assert_eq!(&out[..out_len], original.as_slice());
}

#[test]
fn test_repacketizer_emit_handles_padding_extensions() {
    let packet = [
        0x7B, 0x41, 16, 0x00, 67, 7, b'a', b'b', b'c', b'd', b'e', b'f', b'g', 200, b'u', b'v',
        b'w', b'x', b'y', b'z',
    ];
    let mut rp = Repacketizer::new().unwrap();
    rp.push(&packet).unwrap();

    let mut out = [0u8; 64];
    let out_len = rp.emit(&mut out).unwrap();
    assert_eq!(packet_frame_count(&out[..out_len]).unwrap(), 1);
    assert_eq!(&out[..out_len], packet);

    let unpadded_len = packet_unpad(&mut out, out_len).unwrap();
    assert!(unpadded_len > 0);
}

#[test]
fn test_repacketizer_empty_state_rejects_emit() {
    let mut rp = Repacketizer::new().unwrap();
    assert!(rp.is_empty());

    let mut out = [0u8; 8];
    assert_eq!(rp.emit(&mut out).unwrap_err(), Error::BadArg);
    assert_eq!(rp.emit_range(0, 1, &mut out).unwrap_err(), Error::BadArg);

    let mut empty = [];
    assert_eq!(rp.emit(&mut empty).unwrap_err(), Error::BadArg);
    assert_eq!(rp.emit_range(0, 1, &mut empty).unwrap_err(), Error::BadArg);
}

#[test]
fn test_repacketizer_emit_range_validates_bounds() {
    let mut rp = Repacketizer::new().unwrap();
    let packet_a = single_frame_packet(b'a');
    let packet_b = single_frame_packet(b'b');
    rp.push(&packet_a).unwrap();
    rp.push(&packet_b).unwrap();

    let mut out = [0u8; 8];
    assert_eq!(rp.emit_range(-1, 1, &mut out).unwrap_err(), Error::BadArg);
    assert_eq!(rp.emit_range(1, 1, &mut out).unwrap_err(), Error::BadArg);
    assert_eq!(rp.emit_range(2, 1, &mut out).unwrap_err(), Error::BadArg);
    assert_eq!(rp.emit_range(0, 3, &mut out).unwrap_err(), Error::BadArg);
}

#[test]
fn test_repacketizer_emit_reports_buffer_too_small() {
    let mut rp = Repacketizer::new().unwrap();
    let packet = single_frame_packet(b'a');
    rp.push(&packet).unwrap();

    let mut tiny = [0u8; 1];
    assert_eq!(rp.emit(&mut tiny).unwrap_err(), Error::BufferTooSmall);
    assert_eq!(
        rp.emit_range(0, 1, &mut tiny).unwrap_err(),
        Error::BufferTooSmall
    );

    let mut empty = [];
    assert_eq!(rp.emit(&mut empty).unwrap_err(), Error::BufferTooSmall);
    assert_eq!(
        rp.emit_range(0, 1, &mut empty).unwrap_err(),
        Error::BufferTooSmall
    );
}

#[test]
fn test_repacketizer_emit_range_outputs_exact_frame_subset() {
    let mut rp = Repacketizer::new().unwrap();
    let packet_a = single_frame_packet(b'a');
    let packet_b = single_frame_packet(b'b');
    let packet_c = single_frame_packet(b'c');
    rp.push(&packet_a).unwrap();
    rp.push(&packet_b).unwrap();
    rp.push(&packet_c).unwrap();

    let mut out = [0u8; 16];
    let out_len = rp.emit(&mut out).unwrap();
    assert_eq!(
        &out[..out_len],
        &[TOC_CONFIG | PACKET_CODE_3, 3, b'a', b'b', b'c']
    );

    let out_len = rp.emit_range(1, 2, &mut out).unwrap();
    assert_eq!(&out[..out_len], &[TOC_CONFIG, b'b']);

    let out_len = rp.emit_range(1, 3, &mut out).unwrap();
    assert_eq!(&out[..out_len], &[TOC_CONFIG | PACKET_CODE_1, b'b', b'c']);
}

#[cfg(opus_codec_frame_bounded_extensions)]
#[test]
fn test_repacketizer_emit_handles_repeated_extensions() {
    let packet = three_frame_packet_with_repeated_extension_payloads();
    let mut rp = Repacketizer::new().unwrap();
    rp.push(&packet).unwrap();

    let mut out = [0u8; 32];
    let out_len = rp.emit(&mut out).unwrap();
    assert_eq!(&out[..out_len], packet);

    let out_len = rp.emit_range(1, 3, &mut out).unwrap();
    assert_eq!(
        &out[..out_len],
        &[
            TOC_CONFIG | PACKET_CODE_3,
            CODE_3_PADDING_FLAG | 2,
            4,
            b'b',
            b'c',
            EXTENSION_ID_5_WITH_ONE_BYTE,
            b'y',
            0x04,
            b'z',
        ]
    );
}

#[test]
fn test_repacketizer_emit_range_filters_and_reindexes_extensions() {
    let mut rp = Repacketizer::new().unwrap();
    let packet = three_frame_packet_with_extension_on_last_frame();
    rp.push(&packet).unwrap();

    let mut out = [0u8; 32];
    #[cfg(opus_codec_rust_packet_ops)]
    {
        let out_len = rp.emit(&mut out).unwrap();
        assert_eq!(&out[..out_len], packet);

        let out_len = rp.emit_range(0, 1, &mut out).unwrap();
        assert_eq!(&out[..out_len], &[TOC_CONFIG, b'a']);

        let out_len = rp.emit_range(1, 3, &mut out).unwrap();
        assert_eq!(
            &out[..out_len],
            &[
                TOC_CONFIG | PACKET_CODE_3,
                CODE_3_PADDING_FLAG | 2,
                3,
                b'b',
                b'c',
                FRAME_SEPARATOR_TO_NEXT,
                EXTENSION_ID_5_WITH_ONE_BYTE,
                b'x',
            ]
        );

        let out_len = rp.emit_range(2, 3, &mut out).unwrap();
        assert_eq!(
            &out[..out_len],
            &[
                TOC_CONFIG | PACKET_CODE_3,
                CODE_3_PADDING_FLAG | 1,
                2,
                b'c',
                EXTENSION_ID_5_WITH_ONE_BYTE,
                b'x',
            ]
        );
    }
    #[cfg(not(opus_codec_rust_packet_ops))]
    {
        assert_repacketizer_result_matches_raw(
            rp.emit(&mut out),
            &out,
            raw_repacketizer_emit(&[&packet]),
        );

        assert_repacketizer_result_matches_raw(
            rp.emit_range(0, 1, &mut out),
            &out,
            raw_repacketizer_emit_range(&[&packet], 0, 1),
        );

        assert_repacketizer_result_matches_raw(
            rp.emit_range(1, 3, &mut out),
            &out,
            raw_repacketizer_emit_range(&[&packet], 1, 3),
        );

        assert_repacketizer_result_matches_raw(
            rp.emit_range(2, 3, &mut out),
            &out,
            raw_repacketizer_emit_range(&[&packet], 2, 3),
        );
    }
}

#[test]
fn test_repacketizer_emit_range_ignores_malformed_extensions_outside_range() {
    let mut rp = Repacketizer::new().unwrap();
    let packet = two_frame_packet_with_malformed_extension_on_first_frame();
    rp.push(&packet).unwrap();

    let mut out = [0u8; 8];
    assert_eq!(
        rp.emit_range(0, 1, &mut out).unwrap_err(),
        Error::InternalError
    );

    let out_len = rp.emit_range(1, 2, &mut out).unwrap();
    assert_eq!(&out[..out_len], &[TOC_CONFIG, b'b']);
}

#[test]
fn test_repacketizer_emit_sorts_extensions_by_output_frame() {
    let future_frame_extension = [
        TOC_CONFIG | PACKET_CODE_3,
        CODE_3_PADDING_FLAG | 1,
        4,
        b'a',
        FRAME_SEPARATOR_WITH_DELTA,
        2,
        EXTENSION_ID_5_WITH_ONE_BYTE,
        b'x',
    ];
    let current_frame_extension = [
        TOC_CONFIG | PACKET_CODE_3,
        CODE_3_PADDING_FLAG | 1,
        2,
        b'b',
        EXTENSION_ID_5_WITH_ONE_BYTE,
        b'y',
    ];
    let plain_packet = single_frame_packet(b'c');
    let expected = raw_repacketizer_emit(&[
        &future_frame_extension,
        &current_frame_extension,
        &plain_packet,
    ]);

    let mut rp = Repacketizer::new().unwrap();
    rp.push(&future_frame_extension).unwrap();
    rp.push(&current_frame_extension).unwrap();
    rp.push(&plain_packet).unwrap();

    let mut out = [0u8; 128];
    match expected {
        Ok(expected) => {
            let out_len = rp.emit(&mut out).unwrap();
            assert_eq!(&out[..out_len], expected.as_slice());
        }
        Err(expected) => {
            assert_eq!(rp.emit(&mut out).unwrap_err(), expected);
        }
    }
}

#[test]
fn test_repacketizer_reset_clears_state_and_allows_reuse() {
    let mut rp = Repacketizer::new().unwrap();
    let packet_a = single_frame_packet(b'a');
    let packet_b = single_frame_packet(b'b');
    rp.push(&packet_a).unwrap();
    rp.push(&packet_b).unwrap();
    assert_eq!(rp.len(), 2);

    rp.reset();
    assert!(rp.is_empty());
    let mut out = [0u8; 8];
    assert_eq!(rp.emit(&mut out).unwrap_err(), Error::BadArg);

    rp.push(&packet_b).unwrap();
    let out_len = rp.emit(&mut out).unwrap();
    assert_eq!(&out[..out_len], packet_b);
}

#[test]
fn test_repacketizer_failed_push_does_not_change_state() {
    let mut rp = Repacketizer::new().unwrap();
    let packet = single_frame_packet(b'a');
    let incompatible = [0x00, b'z'];
    rp.push(&packet).unwrap();

    assert_eq!(rp.push(&incompatible).unwrap_err(), Error::InvalidPacket);
    assert_eq!(rp.len(), 1);

    let mut out = [0u8; 8];
    let out_len = rp.emit(&mut out).unwrap();
    assert_eq!(&out[..out_len], packet);
}

#[test]
fn test_repacketizer_ref_init_in_rejects_small_buffer() {
    let mut buf = AlignedBuffer::with_capacity_bytes(0);
    assert!(matches!(
        RepacketizerRef::init_in(&mut buf),
        Err(Error::BadArg)
    ));
}

#[test]
fn test_init_in_place_rejects_misaligned_pointer() {
    let required = Repacketizer::size().unwrap();
    let mut storage = vec![0u8; required + std::mem::align_of::<usize>()];
    let ptr = unsafe { storage.as_mut_ptr().add(1).cast::<OpusRepacketizer>() };

    let err = unsafe { Repacketizer::init_in_place(ptr) }.unwrap_err();
    assert_eq!(err, Error::BadArg);
}

#[test]
fn test_repacketizer_ref_drop_resets_external_state() {
    let rp_size = Repacketizer::size().unwrap();
    let mut rp_buf = AlignedBuffer::with_capacity_bytes(rp_size);
    let rp_ptr = rp_buf.as_mut_ptr();
    unsafe {
        Repacketizer::init_in_place(rp_ptr).unwrap();
    }

    {
        let packet = single_frame_packet(b'a');
        let mut rp = unsafe { RepacketizerRef::from_raw(rp_ptr) };
        rp.push(&packet).unwrap();
        assert_eq!(unsafe { opus_repacketizer_get_nb_frames(rp_ptr) }, 1);
    }

    assert_eq!(unsafe { opus_repacketizer_get_nb_frames(rp_ptr) }, 0);
}

#[test]
fn test_repacketizer_ref_can_emit_prepopulated_raw_state() {
    let rp_ptr = unsafe { opus_repacketizer_create() };
    assert!(!rp_ptr.is_null());
    let packet = single_frame_packet(b'a');
    let result = unsafe { opus_repacketizer_cat(rp_ptr, packet.as_ptr(), packet.len() as i32) };
    assert_eq!(result, 0);

    {
        let mut rp = unsafe { RepacketizerRef::from_raw(rp_ptr) };
        assert_eq!(rp.len(), 1);

        let mut out = [0u8; 8];
        let out_len = rp.emit(&mut out).unwrap();
        assert_eq!(&out[..out_len], packet);

        let out_len = rp.emit_range(0, 1, &mut out).unwrap();
        assert_eq!(&out[..out_len], packet);
    }

    assert_eq!(unsafe { opus_repacketizer_get_nb_frames(rp_ptr) }, 1);
    unsafe { opus_repacketizer_destroy(rp_ptr) };
}
