use opus_codec::{
    Application, Bandwidth, Bitrate, Channels, Complexity, Encoder, SampleRate, Signal,
};

#[test]
fn encoder_control_roundtrip() {
    let sr = SampleRate::Hz48000;
    let mut encoder =
        Encoder::new(sr, Channels::Stereo, Application::Audio).expect("create encoder");

    encoder
        .set_bitrate(Bitrate::Custom(96_000))
        .expect("set bitrate");
    match encoder.bitrate().expect("get bitrate") {
        Bitrate::Custom(bps) => assert_eq!(bps, 96_000),
        other => panic!("unexpected bitrate variant: {other:?}"),
    }

    encoder
        .set_complexity(Complexity::new(4))
        .expect("set complexity");
    assert_eq!(encoder.complexity().expect("get complexity").value(), 4);

    encoder.set_vbr(false).expect("disable vbr");
    assert!(!encoder.vbr().expect("get vbr"));

    encoder
        .set_vbr_constraint(true)
        .expect("set vbr constraint");
    assert!(encoder.vbr_constraint().expect("get vbr constraint"));

    encoder.set_inband_fec(true).expect("enable fec");
    assert!(encoder.inband_fec().expect("get fec"));

    encoder.set_packet_loss_perc(15).expect("set packet loss");
    assert_eq!(encoder.packet_loss_perc().expect("get packet loss"), 15);

    encoder.set_signal(Signal::Music).expect("set signal");
    assert_eq!(encoder.signal().expect("get signal"), Signal::Music);

    encoder
        .set_max_bandwidth(Bandwidth::Wideband)
        .expect("set max bandwidth");
    assert_eq!(
        encoder.max_bandwidth().expect("get max bandwidth"),
        Bandwidth::Wideband
    );

    encoder
        .set_force_channels(Some(Channels::Mono))
        .expect("force mono");
    assert_eq!(
        encoder.force_channels().expect("get forced channels"),
        Some(Channels::Mono)
    );

    encoder
        .set_force_channels(None)
        .expect("clear force channels");
    assert_eq!(encoder.force_channels().expect("get forced channels"), None);
}
