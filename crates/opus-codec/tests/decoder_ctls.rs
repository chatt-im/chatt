use opus_codec::{Channels, Decoder, SampleRate};

#[test]
fn decoder_control_roundtrip() {
    let sr = SampleRate::Hz48000;
    let mut decoder = Decoder::new(sr, Channels::Stereo).expect("create decoder");

    decoder.set_gain(256).expect("set gain");
    assert_eq!(decoder.gain().expect("get gain"), 256);

    decoder
        .set_phase_inversion_disabled(true)
        .expect("disable phase inversion");
    assert!(
        decoder
            .phase_inversion_disabled()
            .expect("phase inversion flag")
    );

    decoder
        .set_phase_inversion_disabled(false)
        .expect("enable phase inversion");
    assert!(
        !decoder
            .phase_inversion_disabled()
            .expect("phase inversion flag")
    );

    assert_eq!(decoder.get_sample_rate().expect("sample rate"), sr.as_i32());

    // OPUS spec guarantees last packet duration reflects most recent decode; before any decode
    assert_eq!(
        decoder
            .get_last_packet_duration()
            .expect("last packet duration"),
        0
    );
}
