#[test]
fn remote_and_local_rpc_share_resource_id_types() {
    fn renderer_room_id(_: local_rpc::ids::RoomId) {}
    fn remote_room_id(_: rpc::ids::RoomId) {}

    renderer_room_id(rpc::ids::RoomId(7));
    remote_room_id(local_rpc::ids::RoomId(9));
}

#[test]
fn remote_plaintext_video_frame_is_locally_borrowed_without_reframing() {
    let encoded = rpc::video::encode_video_frame(33, true, 11, b"encoded frame");
    let expected_body = encoded[local_rpc::video::VIDEO_FRAME_HEADER_LEN..].as_ptr();

    let (frame, consumed) = local_rpc::video::parse_video_frame(&encoded)
        .unwrap()
        .expect("complete frame");

    assert_eq!(consumed, encoded.len());
    assert_eq!(frame.ts_ms, 33);
    assert!(frame.is_key);
    assert_eq!(frame.stream_id, 11);
    assert_eq!(frame.data, b"encoded frame");
    assert_eq!(frame.data.as_ptr(), expected_body);
}
