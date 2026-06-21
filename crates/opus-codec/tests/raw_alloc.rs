use opus_codec::decoder::Decoder;
use opus_codec::encoder::Encoder;
use opus_codec::multistream::{MultistreamDecoder, MultistreamEncoder};
use opus_codec::repacketizer::Repacketizer;
use opus_codec::types::Channels;

#[test]
fn test_raw_size_helpers() {
    assert!(Encoder::size(Channels::Mono).unwrap() > 0);
    assert!(Decoder::size(Channels::Stereo).unwrap() > 0);
    assert!(MultistreamEncoder::size(1, 0).unwrap() > 0);
    assert!(MultistreamDecoder::size(1, 0).unwrap() > 0);
    assert!(Repacketizer::size().unwrap() > 0);
}
