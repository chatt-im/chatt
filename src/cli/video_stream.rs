//! `chatt video-stream`: daemon-backed live-share discovery and NUT piping.

use std::{
    collections::HashMap,
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    process::{Command, Stdio},
    thread,
};

use local_rpc::{
    bitstream::{self, Codec},
    frame::{ClientFrame, DaemonFrame, RequestOutcome},
    model::{LiveShare, RequestId, StateSnapshot},
    unix::{FrameReader, FrameWriter},
};
use unicode_width::UnicodeWidthStr;

use crate::video::NutPipeMuxer;

const REQUEST_ID: RequestId = RequestId(1);

/// mpv's low-latency profile does not make an intermittently-produced stream
/// untimed. Without that explicit policy, its playback clock stops while the
/// demuxer is waiting for damage and the next source timestamp is treated as a
/// future presentation time. The remaining options mirror the native GUI's
/// live-player setup using options available in an unmodified mpv.
const MPV_LIVE_ARGS: &[&str] = &[
    "--no-config",
    "--audio=no",
    "--profile=low-latency",
    "--cache=no",
    "--demuxer-thread=yes",
    "--demuxer-readahead-secs=0",
    "--demuxer=lavf",
    "--demuxer-lavf-format=nut",
    "--demuxer-lavf-probe-info=nostreams",
    "--demuxer-lavf-analyzeduration=0",
    "--untimed=yes",
    "--video-latency-hacks=yes",
    "--swapchain-depth=1",
    "--vd-lavc-threads=1",
    "--vd-lavc-o=flags=low_delay",
    "--interpolation=no",
    "--stream-buffer-size=4k",
    "-",
];

struct OpenVideoStream {
    reader: FrameReader,
    video: UnixStream,
    stream_id: u32,
    muxer: NutPipeMuxer,
}

pub(super) fn list() -> Result<(), Box<dyn std::error::Error>> {
    let (_reader, snapshot) = daemon_snapshot()?;
    print!("{}", render_list(&snapshot));
    Ok(())
}

pub(super) fn pipe(selector: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let opened = open_video_stream(selector)?;
    let stdout = io::stdout();
    write_open_video_stream(opened, stdout.lock()).map_err(Into::into)
}

pub(super) fn play(selector: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let opened = open_video_stream(selector)?;
    let mut player = Command::new("mpv")
        .args(MPV_LIVE_ARGS)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot start mpv: {error}"))?;
    let input = player
        .stdin
        .take()
        .ok_or("mpv did not provide a standard-input pipe")?;
    let stream_result = write_open_video_stream(opened, input);
    let status = player
        .wait()
        .map_err(|error| format!("cannot wait for mpv: {error}"))?;
    stream_result?;
    if !status.success() {
        return Err(format!("mpv exited with {status}").into());
    }
    Ok(())
}

fn open_video_stream(selector: Option<&str>) -> Result<OpenVideoStream, String> {
    let (mut reader, snapshot) = daemon_snapshot()?;
    let share = select_share(&snapshot.live_shares, selector)?.clone();
    let codec = codec_from_string(&share.codec)?;
    let muxer = NutPipeMuxer::new(
        codec,
        share.coded_width,
        share.coded_height,
        &share.extradata,
    )?;

    let writer_stream = reader
        .stream()
        .try_clone()
        .map_err(|error| format!("cannot clone daemon RPC socket: {error}"))?;
    let mut writer = FrameWriter::new(writer_stream);
    writer
        .send_client(&ClientFrame::StartLiveShare {
            request_id: REQUEST_ID,
            stream_id: share.stream_id,
            generation: share.generation,
        })
        .map_err(|error| format!("cannot request video stream: {error}"))?;

    let video = wait_for_video_stream(&mut reader, &share)?;
    drop(writer);

    Ok(OpenVideoStream {
        reader,
        video,
        stream_id: share.stream_id.0,
        muxer,
    })
}

fn write_open_video_stream(opened: OpenVideoStream, output: impl Write) -> Result<(), String> {
    let OpenVideoStream {
        mut reader,
        video,
        stream_id,
        mut muxer,
    } = opened;

    // A long-lived pipe still has to consume projection and status frames or
    // the daemon's RPC output queue can fill and tear down the viewer. The
    // dedicated video descriptor remains the data path.
    let control = reader
        .stream()
        .try_clone()
        .map_err(|error| format!("cannot clone daemon RPC control socket: {error}"))?;
    let drain = thread::Builder::new()
        .name("chatt-video-stream-control".into())
        .spawn(move || while reader.recv_daemon_with_fds().is_ok() {})
        .map_err(|error| format!("cannot start daemon RPC reader: {error}"))?;

    let result = write_nut_stream(video, stream_id, &mut muxer, output);
    let _ = control.shutdown(Shutdown::Both);
    let _ = drain.join();
    result
}

fn daemon_snapshot() -> Result<(FrameReader, StateSnapshot), String> {
    let hello = local_rpc::frame::ClientHello::current(format!(
        "chatt-video-stream/{}",
        env!("CARGO_PKG_VERSION")
    ));
    let mut reader = local_rpc::unix::connect(&hello)
        .map_err(|error| format!("cannot connect to running Chatt daemon: {error}"))?;
    match reader
        .recv_daemon()
        .map_err(|error| format!("cannot read daemon welcome: {error}"))?
    {
        DaemonFrame::Welcome(_) => {}
        _ => return Err("daemon RPC did not begin with a welcome frame".into()),
    }
    match reader
        .recv_daemon()
        .map_err(|error| format!("cannot read daemon snapshot: {error}"))?
    {
        DaemonFrame::Snapshot { snapshot, .. } => Ok((reader, snapshot)),
        _ => Err("daemon RPC did not provide an initial state snapshot".into()),
    }
}

fn wait_for_video_stream(
    reader: &mut FrameReader,
    share: &LiveShare,
) -> Result<UnixStream, String> {
    loop {
        let received = reader
            .recv_daemon_with_fds()
            .map_err(|error| format!("cannot open video stream: {error}"))?;
        match received.frame {
            DaemonFrame::LiveShareOpened {
                request_id,
                stream_id,
                generation,
                ..
            } if request_id == REQUEST_ID => {
                if stream_id != share.stream_id || generation != share.generation {
                    return Err("daemon opened a different video stream than requested".into());
                }
                let [fd]: [std::os::fd::OwnedFd; 1] =
                    received.fds.try_into().map_err(|fds: Vec<_>| {
                        format!(
                            "daemon opened the video stream with {} descriptors instead of one",
                            fds.len()
                        )
                    })?;
                return Ok(UnixStream::from(fd));
            }
            DaemonFrame::RequestResult(result) if result.request_id == REQUEST_ID => {
                if let RequestOutcome::Rejected { message, .. } = result.outcome {
                    return Err(format!("video stream request rejected: {message}"));
                }
            }
            _ => {}
        }
    }
}

fn codec_from_string(codec: &str) -> Result<Codec, String> {
    if codec.starts_with("avc1.") || codec.eq_ignore_ascii_case("h264") {
        Ok(Codec::H264)
    } else if codec.starts_with("hvc1.")
        || codec.starts_with("hev1.")
        || codec.eq_ignore_ascii_case("hevc")
    {
        Ok(Codec::Hevc)
    } else {
        Err(format!("unsupported live video codec {codec:?}"))
    }
}

fn write_nut_stream(
    mut stream: UnixStream,
    expected_stream_id: u32,
    muxer: &mut NutPipeMuxer,
    mut output: impl Write,
) -> Result<(), String> {
    if !write_output(&mut output, muxer.header())? {
        return Ok(());
    }

    let mut nut_frame = Vec::new();
    loop {
        let mut wire_header = [0u8; local_rpc::video::VIDEO_FRAME_HEADER_LEN];
        if !read_frame_header(&mut stream, &mut wire_header)
            .map_err(|error| format!("cannot read video frame header: {error}"))?
        {
            return Ok(());
        }
        let header = local_rpc::video::parse_video_frame_header(&wire_header)
            .map_err(|error| format!("invalid video frame header: {error}"))?
            .expect("fixed-size video frame header is complete");
        if header.stream_id != expected_stream_id {
            return Err(format!(
                "video frame belongs to stream {}, expected {expected_stream_id}",
                header.stream_id
            ));
        }
        if header.bootstrap_end {
            continue;
        }

        let payload_len = header.size - local_rpc::video::VIDEO_FRAME_HEADER_LEN;
        let payload_offset =
            muxer.start_frame(&mut nut_frame, header.ts_ms, header.is_key, payload_len);
        nut_frame.resize(payload_offset + payload_len, 0);
        stream
            .read_exact(&mut nut_frame[payload_offset..])
            .map_err(|error| format!("cannot read video frame payload: {error}"))?;
        bitstream::length_prefixed_to_annex_b_in_place(&mut nut_frame[payload_offset..])?;
        if !write_output(&mut output, &nut_frame)? {
            return Ok(());
        }
    }
}

fn read_frame_header(
    stream: &mut UnixStream,
    header: &mut [u8; local_rpc::video::VIDEO_FRAME_HEADER_LEN],
) -> io::Result<bool> {
    let mut filled = 0;
    while filled < header.len() {
        match stream.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(false),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "video socket closed during a frame header",
                ));
            }
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

/// Writes binary output without turning a downstream player closing its stdin
/// into a command failure.
fn write_output(output: &mut impl Write, bytes: &[u8]) -> Result<bool, String> {
    match output.write_all(bytes) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(error) => Err(format!("cannot write NUT stream: {error}")),
    }
}

fn select_share<'a>(
    shares: &'a [LiveShare],
    selector: Option<&str>,
) -> Result<&'a LiveShare, String> {
    let Some(selector) = selector else {
        return match shares {
            [] => Err("no live video streams; run `chatt video-stream list`".into()),
            [share] => Ok(share),
            _ => Err(
                "multiple live video streams; choose a sender from `chatt video-stream list`"
                    .into(),
            ),
        };
    };
    let selector = selector.trim();
    if selector.is_empty() {
        return Err("video stream selector is empty".into());
    }

    let matches = if let Some(id) = selector.strip_prefix("id:") {
        let id = id
            .parse::<u64>()
            .map_err(|_| format!("invalid sender id selector {selector:?}"))?;
        shares
            .iter()
            .filter(|share| share.sender_id.0 == id)
            .collect::<Vec<_>>()
    } else {
        let folded = rpc::username::fold(selector);
        let exact = shares
            .iter()
            .filter(|share| rpc::username::fold(&share.sender_name) == folded)
            .collect::<Vec<_>>();
        if exact.is_empty() {
            shares
                .iter()
                .filter(|share| rpc::username::fold(&share.sender_name).starts_with(&folded))
                .collect()
        } else {
            exact
        }
    };

    match matches.as_slice() {
        [] => Err(format!(
            "no live video stream matches {selector:?}; run `chatt video-stream list`"
        )),
        [share] => Ok(share),
        _ => {
            let candidates = matches
                .iter()
                .map(|share| {
                    format!(
                        "{} (id:{}, stream:{})",
                        share.sender_name, share.sender_id.0, share.stream_id.0
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "video stream selector {selector:?} is ambiguous: {candidates}"
            ))
        }
    }
}

fn render_list(snapshot: &StateSnapshot) -> String {
    if snapshot.live_shares.is_empty() {
        return "No live video streams.\n".into();
    }
    let rooms = snapshot
        .rooms
        .iter()
        .map(|room| (room.id, room.name.as_str()))
        .collect::<HashMap<_, _>>();
    let mut shares = snapshot.live_shares.iter().collect::<Vec<_>>();
    shares.sort_by(|left, right| {
        rpc::username::fold(&left.sender_name)
            .cmp(&rpc::username::fold(&right.sender_name))
            .then(left.sender_id.cmp(&right.sender_id))
            .then(left.stream_id.cmp(&right.stream_id))
    });
    let mut rows = vec![[
        "SENDER ID".to_string(),
        "SENDER".to_string(),
        "ROOM".to_string(),
        "CODEC".to_string(),
        "RESOLUTION".to_string(),
    ]];
    for share in shares {
        rows.push([
            share.sender_id.0.to_string(),
            share.sender_name.clone(),
            rooms
                .get(&share.room_id)
                .map(|name| (*name).to_string())
                .unwrap_or_else(|| format!("id:{}", share.room_id.0)),
            share.codec.clone(),
            format!("{}x{}", share.coded_width, share.coded_height),
        ]);
    }
    render_table(&rows)
}

fn render_table(rows: &[[String; 5]]) -> String {
    let mut widths = [0usize; 5];
    for row in rows {
        for (index, value) in row.iter().enumerate() {
            widths[index] = widths[index].max(UnicodeWidthStr::width(value.as_str()));
        }
    }
    let mut out = String::new();
    for row in rows {
        for (index, value) in row.iter().enumerate() {
            out.push_str(value);
            if index + 1 != row.len() {
                let padding = widths[index] - UnicodeWidthStr::width(value.as_str()) + 2;
                out.extend(std::iter::repeat_n(' ', padding));
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rpc::ids::{RoomId, StreamId, UserId};

    fn share(stream_id: u32, sender_id: u64, sender: &str) -> LiveShare {
        LiveShare {
            room_id: RoomId(1),
            stream_id: StreamId(stream_id),
            generation: 1,
            sender_id: UserId(sender_id),
            sender_name: sender.into(),
            codec: "avc1.42C00D".into(),
            coded_width: 320,
            coded_height: 240,
            extradata: vec![1],
        }
    }

    #[test]
    fn sole_stream_needs_no_selector() {
        let shares = [share(1, 7, "Alice")];
        assert_eq!(select_share(&shares, None).unwrap().stream_id, StreamId(1));
        assert!(select_share(&[], None).is_err());
        assert!(select_share(&[share(1, 7, "Alice"), share(2, 8, "Bob")], None).is_err());
    }

    #[test]
    fn selector_accepts_sender_id_exact_name_and_unique_prefix() {
        let shares = [share(1, 7, "Alice"), share(2, 8, "Alicia")];
        assert_eq!(
            select_share(&shares, Some("id:8")).unwrap().stream_id,
            StreamId(2)
        );
        assert_eq!(
            select_share(&shares, Some("ALICE")).unwrap().stream_id,
            StreamId(1)
        );
        assert_eq!(
            select_share(&shares, Some("alic i"))
                .unwrap_err()
                .contains("no live"),
            true
        );
        assert!(
            select_share(&shares, Some("ali"))
                .unwrap_err()
                .contains("ambiguous")
        );
        assert_eq!(
            select_share(&shares, Some("alici")).unwrap().stream_id,
            StreamId(2)
        );
    }

    #[test]
    fn duplicate_streams_from_one_sender_are_ambiguous() {
        let shares = [share(1, 7, "Alice"), share(2, 7, "Alice")];
        assert!(
            select_share(&shares, Some("Alice"))
                .unwrap_err()
                .contains("ambiguous")
        );
        assert!(
            select_share(&shares, Some("id:7"))
                .unwrap_err()
                .contains("ambiguous")
        );
    }
}
