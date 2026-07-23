use std::{os::fd::AsRawFd, os::unix::net::UnixStream};

use local_rpc::{
    DEFAULT_UPLOAD_LIMIT_BYTES, MAX_HISTORY_REQUEST_MESSAGES, MAX_MESSAGE_BODY_BYTES,
    PROTOCOL_MAX_VERSION, PROTOCOL_MIN_VERSION,
    bulk::BulkFinished,
    frame::{ClientFrame, ClientHello, DaemonFrame, NegotiatedLimits, Welcome},
    ids::StreamId,
    model::{
        BulkTransferId, ConnectionState, DaemonInstanceId, RequestId, StateSnapshot, VoiceState,
    },
    unix::{FrameReader, FrameWriter},
};

#[test]
fn renderer_and_daemon_exchange_protocol_v6_frames_and_live_share_fd() {
    assert_eq!(PROTOCOL_MIN_VERSION, 6);
    assert_eq!(PROTOCOL_MAX_VERSION, 6);
    assert_eq!(MAX_MESSAGE_BODY_BYTES, 8 * 1024);
    assert_eq!(DEFAULT_UPLOAD_LIMIT_BYTES, 50 * 1024 * 1024);
    assert_eq!(MAX_HISTORY_REQUEST_MESSAGES, 500);

    let hello = ClientHello::current("test-renderer");
    assert_eq!(hello.negotiated_version(), Some(6));

    let (daemon_socket, renderer_socket) = UnixStream::pair().unwrap();
    let mut daemon_reader = FrameReader::new(daemon_socket.try_clone().unwrap());
    let mut daemon_writer = FrameWriter::new(daemon_socket);
    let mut renderer_reader = FrameReader::new(renderer_socket.try_clone().unwrap());
    let mut renderer_writer = FrameWriter::new(renderer_socket);

    let welcome = DaemonFrame::Welcome(Welcome {
        version: hello.negotiated_version().unwrap(),
        instance_id: DaemonInstanceId([1; 16]),
        daemon_build: "test-daemon".into(),
        connection: ConnectionState::Online,
        active_server: Some("test-server".into()),
        first_event_seq: 1,
        limits: NegotiatedLimits::default(),
        commands: Vec::new(),
    });
    daemon_writer.send_daemon(&welcome).unwrap();
    assert_eq!(renderer_reader.recv_daemon().unwrap(), welcome);

    let request = ClientFrame::RequestSnapshot {
        request_id: RequestId(1),
    };
    renderer_writer.send_client(&request).unwrap();
    assert_eq!(daemon_reader.recv_client().unwrap(), request);

    let snapshot = DaemonFrame::Snapshot {
        instance_id: DaemonInstanceId([1; 16]),
        event_seq: 1,
        snapshot: StateSnapshot {
            connection: ConnectionState::Online,
            active_server: Some("test-server".into()),
            local_identity: Some("alice".into()),
            rooms: Vec::new(),
            selected_room: None,
            room: None,
            voice: VoiceState {
                muted: false,
                deafened: false,
                output_volume: 100.0,
                joined_room: None,
            },
            transfers: Vec::new(),
            live_shares: Vec::new(),
        },
    };
    daemon_writer.send_daemon(&snapshot).unwrap();
    assert_eq!(renderer_reader.recv_daemon().unwrap(), snapshot);

    let transfer_id = BulkTransferId(4);
    daemon_writer
        .send_daemon_bulk_chunk(transfer_id, &[1, 2, 3, 4])
        .unwrap();
    let mut received_chunk = false;
    assert!(
        renderer_reader
            .recv_daemon_with_bulk(|received_id, bytes| {
                assert_eq!(received_id, transfer_id);
                assert_eq!(bytes, [1, 2, 3, 4]);
                received_chunk = true;
                Ok(())
            })
            .unwrap()
            .is_none()
    );
    assert!(received_chunk);

    let finished = DaemonFrame::BulkFinished(BulkFinished { transfer_id });
    daemon_writer.send_daemon(&finished).unwrap();
    assert_eq!(renderer_reader.recv_daemon().unwrap(), finished);

    let (video, _video_peer) = UnixStream::pair().unwrap();
    let opened = DaemonFrame::LiveShareOpened {
        request_id: RequestId(2),
        stream_id: StreamId(8),
    };
    daemon_writer
        .send_daemon_with_fds(&opened, &[video.as_raw_fd()])
        .unwrap();
    let received = renderer_reader.recv_daemon_with_fds().unwrap();
    assert_eq!(received.frame, opened);
    assert_eq!(received.fds.len(), 1);
}
