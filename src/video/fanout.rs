//! Local delivery for one decrypted screen-share stream: the point where the
//! publisher's self-view, a remote subscriber, the browser, and every native
//! renderer converge.
//!
//! Three invariants hold here, and everything downstream depends on them:
//!
//! - **Active-stream fence.** A frame is delivered only while its stream id is
//!   in `active_streams`. Servers restart stream ids at one, so this is what
//!   stops a reused id from inheriting the previous stream's cache, queues, and
//!   browser decoder state. [`VideoFrameFanout::close_stream`] drops all of it
//!   at once.
//! - **Keyframe-led caches.** The per-stream fast-start cache is empty or
//!   begins with the keyframe every later frame references. A keyframe resets
//!   it; overflow clears it wholesale, because a truncated GOP would hand a
//!   joining viewer a reference gap.
//! - **Bootstrap ordering.** A native viewer attaching while a subscriber is
//!   coming up must not begin its shallow live queue until the server's cached
//!   GOP has arrived. It is seeded from the local cache, then waits for the
//!   upstream `bootstrap_end` marker before the boundary is written to its
//!   socket. Frames the daemon already cached are suppressed during that replay
//!   so no decoder rewinds through a GOP it has seen.
//!
//! Every path that drops a frame counts it and logs at a bounded rate: a
//! picture that never reaches a renderer has to leave a trace somewhere, and a
//! broken stream produces them at frame rate.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Write,
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
};

use rpc::{ids::StreamId, video::SharedVideoFrame};

use crate::web_server::WebFeedSender;

const MAX_PENDING_FRAMES: usize = 2;
/// Frames a cached GOP may hold, and so also the depth a viewer's queue may
/// reach while it drains one: [`NativeQueue::seed`] loads the whole cache, so
/// the two have to move together.
const MAX_BOOTSTRAP_FRAMES: usize = rpc::video::FAST_START_MAX_FRAMES;
/// Byte bounds on a viewer queue and on the cached GOP. Both charge
/// `SharedVideoFrame::retained_bytes`, the whole record allocation a received
/// frame arrived in, so they are sized by the record and not by the frame: a
/// bound of one frame could not admit a single frame at the cap.
const MAX_PENDING_BYTES: usize = rpc::video::MAX_VIDEO_RECORD_LEN;
const FAST_START_MAX_BYTES: usize = rpc::video::MAX_VIDEO_RECORD_LEN;
const _: () = assert!(MAX_PENDING_BYTES >= rpc::video::MAX_VIDEO_RECORD_LEN);
const _: () = assert!(FAST_START_MAX_BYTES >= rpc::video::MAX_VIDEO_RECORD_LEN);
/// One log line per this many drops on a given path, about one per second of
/// video at 30 fps.
const DROP_LOG_INTERVAL: u64 = 30;

#[derive(Clone)]
pub struct VideoFrameFanout {
    inner: Arc<FanoutInner>,
}

struct FanoutInner {
    web: Option<WebFeedSender>,
    state: Mutex<FanoutState>,
}

#[derive(Default)]
struct FanoutState {
    active_streams: HashSet<StreamId>,
    native: HashMap<(u64, StreamId), Arc<NativeQueue>>,
    fast_start: HashMap<StreamId, NativeFastStart>,
    upstream_bootstrap: HashMap<StreamId, UpstreamBootstrap>,
    /// Frames dropped for not parsing as one whole plaintext frame. A picture
    /// that never appears has to leave a trace somewhere, but a broken stream
    /// produces them at frame rate, so only some are logged.
    malformed_frames: u64,
    /// Frames dropped because the active-stream fence was closed. A worker can
    /// outlive its share by a frame or two, but a steady count means one is
    /// still delivering into a stream nobody is listening to.
    inactive_frames: u64,
}

#[derive(Default)]
struct NativeFastStart {
    frames: VecDeque<SharedVideoFrame>,
    bytes: usize,
}

struct UpstreamBootstrap {
    replay_through_ts_ms: Option<i64>,
    skipped_frames: u64,
}

struct NativeQueue {
    viewer_id: u64,
    stream_id: StreamId,
    state: Mutex<QueueState>,
    ready: Condvar,
}

#[derive(Default)]
struct QueueState {
    frames: VecDeque<SharedVideoFrame>,
    pending_bytes: usize,
    seen_keyframe: bool,
    awaiting_keyframe: bool,
    bootstrapping: bool,
    bootstrap_complete: bool,
    bootstrap_boundary_pending: bool,
    closed: bool,
    received_frames: u64,
    written_frames: u64,
    /// Frames this viewer never saw: deltas refused while waiting for a
    /// keyframe, plus whatever an overflow discarded. Without this a renderer
    /// that falls behind goes black leaving nothing behind but its absence.
    dropped_frames: u64,
    /// Times the queue gave up on the current GOP and waited for a fresh
    /// keyframe.
    overflows: u64,
}

pub struct NativeViewerHandle {
    fanout: VideoFrameFanout,
    viewer_id: u64,
    stream_id: StreamId,
    queue: Arc<NativeQueue>,
    control: Arc<UnixStream>,
    join: Option<JoinHandle<()>>,
}

impl VideoFrameFanout {
    pub fn new(web: Option<WebFeedSender>) -> Self {
        Self {
            inner: Arc::new(FanoutInner {
                web,
                state: Mutex::new(FanoutState::default()),
            }),
        }
    }

    pub fn send(&self, frame: SharedVideoFrame) {
        let Ok(Some((parsed, consumed))) = rpc::video::parse_video_frame(frame.as_slice()) else {
            self.count_malformed(frame.len(), "frame does not parse");
            return;
        };
        if consumed != frame.len() {
            self.count_malformed(frame.len(), "frame size does not match its record");
            return;
        }
        let stream_id = StreamId(parsed.stream_id);
        if parsed.bootstrap_end {
            self.finish_upstream_bootstrap(stream_id);
            return;
        }
        let is_key = parsed.is_key;
        let (queues, replayed) = {
            let mut state = self.inner.state.lock().unwrap();
            if !state.active_streams.contains(&stream_id) {
                state.inactive_frames += 1;
                if state.inactive_frames == 1 || state.inactive_frames % DROP_LOG_INTERVAL == 0 {
                    kvlog::warn!(
                        "dropping video frame for an inactive stream",
                        stream_id = stream_id.0,
                        frame_bytes = frame.len(),
                        dropped_frames = state.inactive_frames
                    );
                }
                return;
            }
            let replayed = if let Some(bootstrap) = state.upstream_bootstrap.get_mut(&stream_id)
                && bootstrap
                    .replay_through_ts_ms
                    .is_some_and(|timestamp| parsed.ts_ms <= timestamp)
            {
                bootstrap.skipped_frames += 1;
                true
            } else {
                false
            };
            if replayed {
                (Vec::new(), true)
            } else {
                // Inside the lock, and only for frames that are not a replay of
                // ones already delivered: the browser routes by stream id, so a
                // frame must not reach it before the share is announced, and a
                // re-sent GOP would rewind its decoder by the whole burst.
                if let Some(web) = &self.inner.web {
                    web.send_video_frame(frame.clone());
                }
                state.fast_start.entry(stream_id).or_default().push(
                    stream_id,
                    frame.clone(),
                    is_key,
                );
                let queues = state
                    .native
                    .iter()
                    .filter(|((_, id), _)| *id == stream_id)
                    .map(|(_, queue)| queue.clone())
                    .collect::<Vec<_>>();
                (queues, false)
            }
        };
        if replayed {
            return;
        }
        for queue in queues {
            queue.push(frame.clone(), is_key);
        }
    }

    /// Records a frame dropped for being unparseable, logging the first and
    /// then every [`DROP_LOG_INTERVAL`]th.
    fn count_malformed(&self, frame_bytes: usize, reason: &'static str) {
        let mut state = self.inner.state.lock().unwrap();
        state.malformed_frames += 1;
        if state.malformed_frames == 1 || state.malformed_frames % DROP_LOG_INTERVAL == 0 {
            kvlog::warn!(
                "dropping malformed video frame",
                reason,
                frame_bytes,
                dropped_frames = state.malformed_frames
            );
        }
    }

    /// Marks a new server subscription so an overlapping server-side cached
    /// GOP is not replayed after the daemon's own cached GOP.
    pub fn begin_upstream_bootstrap(&self, stream_id: StreamId) {
        let mut state = self.inner.state.lock().unwrap();
        if !state.active_streams.contains(&stream_id) {
            return;
        }
        let replay_through_ts_ms = state
            .fast_start
            .get(&stream_id)
            .and_then(NativeFastStart::latest_timestamp);
        state.upstream_bootstrap.insert(
            stream_id,
            UpstreamBootstrap {
                replay_through_ts_ms,
                skipped_frames: 0,
            },
        );
    }

    fn finish_upstream_bootstrap(&self, stream_id: StreamId) {
        let queues = {
            let mut state = self.inner.state.lock().unwrap();
            let Some(bootstrap) = state.upstream_bootstrap.remove(&stream_id) else {
                return;
            };
            kvlog::info!(
                "video subscriber fast-start complete",
                stream_id = stream_id.0,
                skipped_replayed_frames = bootstrap.skipped_frames
            );
            state
                .native
                .iter()
                .filter(|((_, id), _)| *id == stream_id)
                .map(|(_, queue)| queue.clone())
                .collect::<Vec<_>>()
        };
        for queue in queues {
            queue.finish_bootstrap();
        }
    }

    pub fn add_native(
        &self,
        viewer_id: u64,
        stream_id: StreamId,
        stream: UnixStream,
        wait_for_upstream_bootstrap: bool,
    ) -> Result<NativeViewerHandle, String> {
        let key = (viewer_id, stream_id);
        let control = Arc::new(
            stream
                .try_clone()
                .map_err(|error| format!("cannot clone native video socket: {error}"))?,
        );
        let queue = Arc::new(NativeQueue {
            viewer_id,
            stream_id,
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
        });
        {
            let mut state = self.inner.state.lock().unwrap();
            if !state.active_streams.contains(&stream_id) {
                return Err("screen share is no longer available".into());
            }
            if state.native.contains_key(&key) {
                return Err("screen share is already playing".into());
            }
            let cached = state.fast_start.get(&stream_id);
            let cached_frames = cached.map_or(0, |cache| cache.frames.len());
            let cached_bytes = cached.map_or(0, |cache| cache.bytes);
            let wait_for_upstream_bootstrap =
                wait_for_upstream_bootstrap || state.upstream_bootstrap.contains_key(&stream_id);
            queue.seed(
                cached.map(|cache| &cache.frames),
                !wait_for_upstream_bootstrap,
            );
            kvlog::info!(
                "native video viewer registered",
                viewer_id,
                stream_id = stream_id.0,
                cached_frames,
                cached_bytes,
                wait_for_upstream_bootstrap
            );
            state.native.insert(key, queue.clone());
        }
        let writer_queue = queue.clone();
        let join = thread::Builder::new()
            .name(format!("chatt-native-video-{}-{}", viewer_id, stream_id.0))
            .spawn(move || native_writer_loop(stream, writer_queue))
            .map_err(|error| {
                self.inner.state.lock().unwrap().native.remove(&key);
                format!("failed to start native video writer: {error}")
            })?;
        Ok(NativeViewerHandle {
            fanout: self.clone(),
            viewer_id,
            stream_id,
            queue,
            control,
            join: Some(join),
        })
    }

    #[cfg(test)]
    pub(crate) fn cached_frames(&self, stream_id: StreamId) -> usize {
        self.inner
            .state
            .lock()
            .unwrap()
            .fast_start
            .get(&stream_id)
            .map_or(0, |cache| cache.frames.len())
    }

    pub fn has_native(&self, stream_id: StreamId) -> bool {
        self.inner
            .state
            .lock()
            .unwrap()
            .native
            .keys()
            .any(|(_, id)| *id == stream_id)
    }

    /// Opens a stream id for frame delivery. Duplicate announcements are
    /// idempotent and preserve the current keyframe-led cache.
    pub fn start_stream(&self, stream_id: StreamId) {
        self.inner
            .state
            .lock()
            .unwrap()
            .active_streams
            .insert(stream_id);
    }

    pub fn close_stream(&self, stream_id: StreamId) {
        let queues = {
            let mut state = self.inner.state.lock().unwrap();
            state.active_streams.remove(&stream_id);
            state.fast_start.remove(&stream_id);
            state.upstream_bootstrap.remove(&stream_id);
            let keys = state
                .native
                .keys()
                .filter(|(_, id)| *id == stream_id)
                .copied()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| state.native.remove(&key))
                .collect::<Vec<_>>()
        };
        for queue in queues {
            queue.close();
        }
    }
}

impl NativeFastStart {
    fn latest_timestamp(&self) -> Option<i64> {
        self.frames.back().and_then(|frame| {
            rpc::video::parse_video_frame_header(frame.as_slice())
                .ok()
                .flatten()
                .map(|header| header.ts_ms)
        })
    }

    fn push(&mut self, stream_id: StreamId, frame: SharedVideoFrame, is_key: bool) {
        if is_key {
            self.frames.clear();
            self.bytes = 0;
        } else if self.frames.is_empty() {
            return;
        }
        if self.frames.len() >= MAX_BOOTSTRAP_FRAMES
            || self.bytes.saturating_add(frame.retained_bytes()) > FAST_START_MAX_BYTES
        {
            kvlog::warn!(
                "native video fast start cache overflowed; waiting for keyframe",
                stream_id = stream_id.0,
                cached_frames = self.frames.len(),
                cached_bytes = self.bytes,
                frame_bytes = frame.len()
            );
            self.frames.clear();
            self.bytes = 0;
            return;
        }
        self.bytes += frame.retained_bytes();
        self.frames.push_back(frame);
    }
}

impl NativeQueue {
    fn seed(&self, frames: Option<&VecDeque<SharedVideoFrame>>, bootstrap_complete: bool) {
        let mut state = self.state.lock().unwrap();
        if let Some(frames) = frames {
            state.frames.extend(frames.iter().cloned());
        }
        if bootstrap_complete {
            state.push_bootstrap_boundary(self.stream_id);
        }
        state.pending_bytes = state
            .frames
            .iter()
            .map(SharedVideoFrame::retained_bytes)
            .sum();
        state.seen_keyframe = frames.is_some_and(|frames| !frames.is_empty());
        state.bootstrapping = true;
        state.bootstrap_complete = bootstrap_complete;
        self.ready.notify_one();
    }

    fn finish_bootstrap(&self) {
        let mut state = self.state.lock().unwrap();
        if state.bootstrap_complete {
            return;
        }
        state.bootstrap_complete = true;
        state.push_bootstrap_boundary(self.stream_id);
        self.ready.notify_one();
    }

    fn push(&self, frame: SharedVideoFrame, is_key: bool) {
        let mut state = self.state.lock().unwrap();
        if state.closed {
            return;
        }
        state.received_frames += 1;
        if state.received_frames == 1 {
            kvlog::info!(
                "native video first live frame queued",
                viewer_id = self.viewer_id,
                stream_id = self.stream_id.0,
                keyframe = is_key,
                frame_bytes = frame.len()
            );
        }
        if !state.seen_keyframe {
            if !is_key {
                state.dropped_frames += 1;
                return;
            }
            state.seen_keyframe = true;
        }
        if state.awaiting_keyframe {
            if !is_key {
                state.dropped_frames += 1;
                return;
            }
            state.dropped_frames += state.clear_frames();
            state.awaiting_keyframe = false;
            kvlog::info!(
                "native video viewer recovered at a keyframe",
                viewer_id = self.viewer_id,
                stream_id = self.stream_id.0,
                dropped_frames = state.dropped_frames,
                overflows = state.overflows
            );
        } else {
            let frame_limit = if state.bootstrapping {
                MAX_BOOTSTRAP_FRAMES
            } else {
                MAX_PENDING_FRAMES
            };
            let over_limit = state.frames.len() >= frame_limit
                || state.pending_bytes.saturating_add(frame.retained_bytes()) > MAX_PENDING_BYTES;
            if over_limit {
                if is_key {
                    state.dropped_frames += state.clear_frames();
                } else {
                    self.begin_awaiting_keyframe(
                        &mut state,
                        "viewer is not draining its socket",
                        frame.retained_bytes(),
                    );
                    return;
                }
            }
        }
        // Unreachable for a frame that parsed: `MAX_VIDEO_FRAME_LEN` reserves
        // room for the record overhead `retained_bytes` charges. Reaching it
        // means the size invariant was violated upstream, so it is loud.
        if frame.retained_bytes() > MAX_PENDING_BYTES {
            kvlog::error!(
                "native video frame exceeds the queue bound on its own",
                viewer_id = self.viewer_id,
                stream_id = self.stream_id.0,
                frame_bytes = frame.len(),
                retained_bytes = frame.retained_bytes(),
                limit_bytes = MAX_PENDING_BYTES
            );
            self.begin_awaiting_keyframe(
                &mut state,
                "frame exceeds the queue bound",
                frame.retained_bytes(),
            );
            return;
        }
        state.pending_bytes += frame.retained_bytes();
        state.frames.push_back(frame);
        self.ready.notify_one();
    }

    /// Stops accepting deltas until a fresh keyframe arrives. Continuing with
    /// the next delta would leave the renderer decoding against frames it never
    /// received, so the queue waits for a self-contained restart point instead.
    fn begin_awaiting_keyframe(
        &self,
        state: &mut QueueState,
        reason: &'static str,
        frame_bytes: usize,
    ) {
        state.awaiting_keyframe = true;
        state.bootstrapping = false;
        state.dropped_frames += 1;
        state.overflows += 1;
        if state.overflows == 1 || state.overflows % DROP_LOG_INTERVAL == 0 {
            kvlog::warn!(
                "native video queue overflowed; waiting for a keyframe",
                viewer_id = self.viewer_id,
                stream_id = self.stream_id.0,
                reason,
                queued_frames = state.frames.len(),
                queued_bytes = state.pending_bytes,
                frame_bytes,
                dropped_frames = state.dropped_frames,
                overflows = state.overflows
            );
        }
    }

    fn pop(&self) -> Option<SharedVideoFrame> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(frame) = state.frames.pop_front() {
                state.pending_bytes -= frame.retained_bytes();
                state.written_frames += 1;
                let bootstrap_end = is_bootstrap_boundary(&frame);
                if bootstrap_end {
                    state.bootstrap_boundary_pending = false;
                }
                if state.frames.is_empty() && state.bootstrap_complete {
                    state.bootstrapping = false;
                }
                return Some(frame);
            }
            if state.closed {
                return None;
            }
            state = self.ready.wait(state).unwrap();
        }
    }

    /// Frames this viewer never received, and how many times the queue fell
    /// back to waiting for a keyframe.
    fn drop_counts(&self) -> (u64, u64) {
        let state = self.state.lock().unwrap();
        (state.dropped_frames, state.overflows)
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        state.frames.clear();
        state.pending_bytes = 0;
        state.bootstrap_boundary_pending = false;
        self.ready.notify_all();
    }
}

impl QueueState {
    /// Drops every queued frame, preserving an unwritten bootstrap boundary so
    /// the renderer still learns where the cached GOP ended. Returns how many
    /// encoded frames the viewer lost.
    fn clear_frames(&mut self) -> u64 {
        let boundary = self
            .bootstrap_boundary_pending
            .then(|| {
                self.frames
                    .iter()
                    .find(|frame| is_bootstrap_boundary(frame))
                    .cloned()
            })
            .flatten();
        let discarded = self
            .frames
            .iter()
            .filter(|frame| !is_bootstrap_boundary(frame))
            .count() as u64;
        self.frames.clear();
        self.pending_bytes = 0;
        if let Some(boundary) = boundary {
            self.pending_bytes = boundary.retained_bytes();
            self.frames.push_back(boundary);
        }
        discarded
    }

    fn push_bootstrap_boundary(&mut self, stream_id: StreamId) {
        let boundary =
            SharedVideoFrame::from_vec(rpc::video::encode_video_bootstrap_end(stream_id.0));
        self.pending_bytes += boundary.retained_bytes();
        self.frames.push_back(boundary);
        self.bootstrap_boundary_pending = true;
    }
}

fn is_bootstrap_boundary(frame: &SharedVideoFrame) -> bool {
    rpc::video::parse_video_frame_header(frame.as_slice())
        .ok()
        .flatten()
        .is_some_and(|header| header.bootstrap_end)
}

fn native_writer_loop(mut stream: UnixStream, queue: Arc<NativeQueue>) {
    let mut written = 0u64;
    let mut write_error = None;
    while let Some(frame) = queue.pop() {
        let header = rpc::video::parse_video_frame_header(frame.as_slice())
            .ok()
            .flatten();
        if let Err(error) = stream.write_all(frame.as_slice()) {
            write_error = Some(error);
            break;
        }
        if header.is_some_and(|header| header.bootstrap_end) {
            kvlog::debug!(
                "native video bootstrap boundary written",
                viewer_id = queue.viewer_id,
                stream_id = queue.stream_id.0,
                cached_frames = written
            );
            continue;
        }
        written += 1;
        if written == 1 {
            kvlog::info!(
                "native video first frame written",
                viewer_id = queue.viewer_id,
                stream_id = queue.stream_id.0,
                keyframe = header.is_some_and(|header| header.is_key),
                frame_bytes = frame.len()
            );
        }
    }
    // A viewer's whole session is accountable here: what it saw, and what the
    // queue had to discard to keep its decoder on a coherent reference chain.
    let (dropped_frames, overflows) = queue.drop_counts();
    match write_error {
        Some(error) => kvlog::warn!(
            "native video writer stopped",
            viewer_id = queue.viewer_id,
            stream_id = queue.stream_id.0,
            written_frames = written,
            dropped_frames,
            overflows,
            error = %error
        ),
        None => kvlog::info!(
            "native video writer finished",
            viewer_id = queue.viewer_id,
            stream_id = queue.stream_id.0,
            written_frames = written,
            dropped_frames,
            overflows
        ),
    }
    let _ = stream.shutdown(Shutdown::Both);
    queue.close();
}

impl Drop for NativeViewerHandle {
    fn drop(&mut self) {
        self.fanout
            .inner
            .state
            .lock()
            .unwrap()
            .native
            .remove(&(self.viewer_id, self.stream_id));
        self.queue.close();
        let _ = self.control.shutdown(Shutdown::Both);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn frame(stream_id: u32, key: bool, marker: u8) -> SharedVideoFrame {
        SharedVideoFrame::copy_from_slice(&rpc::video::encode_video_frame(
            marker as i64,
            key,
            stream_id,
            &[marker],
        ))
    }

    fn frame_with_payload(stream_id: u32, key: bool, payload_len: usize) -> SharedVideoFrame {
        SharedVideoFrame::from_vec(rpc::video::encode_video_frame(
            0,
            key,
            stream_id,
            &vec![0; payload_len],
        ))
    }

    #[test]
    fn slow_queue_resumes_only_at_a_fresh_keyframe() {
        let queue = NativeQueue {
            viewer_id: 1,
            stream_id: StreamId(1),
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
        };
        queue.push(frame(1, false, 0), false);
        assert!(queue.state.lock().unwrap().frames.is_empty());
        queue.push(frame(1, true, 1), true);
        for marker in 0..MAX_PENDING_FRAMES {
            queue.push(frame(1, false, marker as u8), false);
        }
        queue.push(frame(1, true, 5), true);
        let newest = queue.pop().unwrap();
        assert_eq!(newest.as_slice()[4], 5);
        assert!(queue.state.lock().unwrap().frames.is_empty());
    }

    #[test]
    fn late_native_viewer_is_seeded_from_latest_keyframe() {
        let fanout = VideoFrameFanout::new(None);
        fanout.start_stream(StreamId(7));
        fanout.send(frame(7, false, 0));
        fanout.send(frame(7, true, 1));
        fanout.send(frame(7, false, 2));
        fanout.send(frame(7, true, 3));
        fanout.send(frame(7, false, 4));

        let (daemon_stream, mut frontend_stream) = UnixStream::pair().unwrap();
        frontend_stream
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let _viewer = fanout
            .add_native(11, StreamId(7), daemon_stream, false)
            .unwrap();
        let mut first = vec![0; rpc::video::VIDEO_FRAME_HEADER_LEN + 1];
        frontend_stream.read_exact(&mut first).unwrap();
        let mut second = vec![0; rpc::video::VIDEO_FRAME_HEADER_LEN + 1];
        frontend_stream.read_exact(&mut second).unwrap();
        let mut boundary = vec![0; rpc::video::VIDEO_FRAME_HEADER_LEN];
        frontend_stream.read_exact(&mut boundary).unwrap();
        assert_eq!(first[4], 3);
        assert_eq!(first[12], 1);
        assert_eq!(second[4], 4);
        assert_eq!(second[12], 0);
        let (boundary, consumed) = rpc::video::parse_video_frame(&boundary).unwrap().unwrap();
        assert!(boundary.bootstrap_end);
        assert_eq!(consumed, rpc::video::VIDEO_FRAME_HEADER_LEN);
    }

    #[test]
    fn native_viewer_without_a_cached_gop_starts_with_a_bootstrap_boundary() {
        let fanout = VideoFrameFanout::new(None);
        fanout.start_stream(StreamId(7));
        let (daemon_stream, mut frontend_stream) = UnixStream::pair().unwrap();
        frontend_stream
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let _viewer = fanout
            .add_native(11, StreamId(7), daemon_stream, false)
            .unwrap();
        let mut bytes = vec![0; rpc::video::VIDEO_FRAME_HEADER_LEN];
        frontend_stream.read_exact(&mut bytes).unwrap();
        let (boundary, _) = rpc::video::parse_video_frame(&bytes).unwrap().unwrap();
        assert!(boundary.bootstrap_end);
        assert_eq!(boundary.stream_id, 7);
    }

    #[test]
    fn cold_native_viewer_waits_for_the_upstream_cached_gop_boundary() {
        let fanout = VideoFrameFanout::new(None);
        fanout.start_stream(StreamId(7));
        let (daemon_stream, mut frontend_stream) = UnixStream::pair().unwrap();
        frontend_stream
            .set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .unwrap();
        let _viewer = fanout
            .add_native(11, StreamId(7), daemon_stream, true)
            .unwrap();

        let mut header = [0; rpc::video::VIDEO_FRAME_HEADER_LEN];
        let error = frontend_stream.read_exact(&mut header).unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ));

        fanout.begin_upstream_bootstrap(StreamId(7));
        fanout.send(frame(7, true, 1));
        fanout.send(frame(7, false, 2));
        fanout.send(SharedVideoFrame::from_vec(
            rpc::video::encode_video_bootstrap_end(7),
        ));

        frontend_stream
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let mut key = vec![0; rpc::video::VIDEO_FRAME_HEADER_LEN + 1];
        frontend_stream.read_exact(&mut key).unwrap();
        let mut delta = vec![0; rpc::video::VIDEO_FRAME_HEADER_LEN + 1];
        frontend_stream.read_exact(&mut delta).unwrap();
        frontend_stream.read_exact(&mut header).unwrap();
        assert_eq!(key[4], 1);
        assert_eq!(delta[4], 2);
        assert!(
            rpc::video::parse_video_frame(&header)
                .unwrap()
                .unwrap()
                .0
                .bootstrap_end
        );
    }

    #[test]
    fn upstream_fast_start_skips_frames_already_in_the_daemon_cache() {
        let fanout = VideoFrameFanout::new(None);
        fanout.start_stream(StreamId(7));
        fanout.send(frame(7, true, 1));
        fanout.send(frame(7, false, 2));

        let (daemon_stream, mut frontend_stream) = UnixStream::pair().unwrap();
        frontend_stream
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let _viewer = fanout
            .add_native(11, StreamId(7), daemon_stream, true)
            .unwrap();
        fanout.begin_upstream_bootstrap(StreamId(7));
        fanout.send(frame(7, true, 1));
        fanout.send(frame(7, false, 2));
        fanout.send(frame(7, false, 3));
        fanout.send(SharedVideoFrame::from_vec(
            rpc::video::encode_video_bootstrap_end(7),
        ));

        for marker in [1, 2, 3] {
            let mut bytes = vec![0; rpc::video::VIDEO_FRAME_HEADER_LEN + 1];
            frontend_stream.read_exact(&mut bytes).unwrap();
            assert_eq!(bytes[4], marker);
        }
        let mut boundary = [0; rpc::video::VIDEO_FRAME_HEADER_LEN];
        frontend_stream.read_exact(&mut boundary).unwrap();
        assert!(
            rpc::video::parse_video_frame(&boundary)
                .unwrap()
                .unwrap()
                .0
                .bootstrap_end
        );
    }

    #[test]
    fn closed_stream_ignores_late_frames_and_reopens_empty() {
        let fanout = VideoFrameFanout::new(None);
        let stream_id = StreamId(7);
        fanout.start_stream(stream_id);
        fanout.send(frame(7, true, 1));
        assert_eq!(
            fanout.inner.state.lock().unwrap().fast_start[&stream_id]
                .frames
                .len(),
            1
        );

        fanout.close_stream(stream_id);
        fanout.send(frame(7, true, 2));
        assert!(
            !fanout
                .inner
                .state
                .lock()
                .unwrap()
                .fast_start
                .contains_key(&stream_id)
        );

        fanout.start_stream(stream_id);
        assert!(
            !fanout
                .inner
                .state
                .lock()
                .unwrap()
                .fast_start
                .contains_key(&stream_id)
        );
        fanout.send(frame(7, true, 3));
        let state = fanout.inner.state.lock().unwrap();
        let cached = state.fast_start[&stream_id].frames.front().unwrap();
        assert_eq!(cached.as_slice()[4], 3);
    }

    #[test]
    fn duplicate_stream_start_preserves_the_live_cache() {
        let fanout = VideoFrameFanout::new(None);
        let stream_id = StreamId(7);
        fanout.start_stream(stream_id);
        fanout.send(frame(7, true, 1));

        fanout.start_stream(stream_id);

        assert_eq!(
            fanout.inner.state.lock().unwrap().fast_start[&stream_id]
                .frames
                .len(),
            1
        );
    }

    #[test]
    fn overflowing_queue_counts_what_the_viewer_lost_and_recovers_at_a_keyframe() {
        let queue = NativeQueue {
            viewer_id: 1,
            stream_id: StreamId(1),
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
        };
        queue.push(frame(1, true, 1), true);
        for marker in 0..MAX_PENDING_FRAMES + 2 {
            queue.push(frame(1, false, marker as u8), false);
        }
        {
            let state = queue.state.lock().unwrap();
            assert!(state.awaiting_keyframe);
            assert_eq!(state.overflows, 1);
            assert!(state.dropped_frames >= 1);
        }

        queue.push(frame(1, true, 9), true);
        let (dropped_frames, overflows) = queue.drop_counts();
        assert_eq!(overflows, 1);
        // The refused deltas plus the queued frames the reset discarded.
        assert_eq!(dropped_frames, 5);
        let state = queue.state.lock().unwrap();
        assert!(!state.awaiting_keyframe);
        assert_eq!(state.frames.len(), 1);
    }

    #[test]
    fn frames_for_an_inactive_stream_are_counted() {
        let fanout = VideoFrameFanout::new(None);
        fanout.send(frame(7, true, 1));
        fanout.send(frame(7, false, 2));
        assert_eq!(fanout.inner.state.lock().unwrap().inactive_frames, 2);
        assert_eq!(fanout.cached_frames(StreamId(7)), 0);
    }

    #[test]
    fn native_queue_byte_overflow_waits_for_a_fresh_keyframe() {
        let queue = NativeQueue {
            viewer_id: 1,
            stream_id: StreamId(1),
            state: Mutex::new(QueueState::default()),
            ready: Condvar::new(),
        };
        let payload_len = MAX_PENDING_BYTES / 2;
        queue.push(frame_with_payload(1, true, payload_len), true);
        queue.push(frame_with_payload(1, false, payload_len), false);
        {
            let state = queue.state.lock().unwrap();
            assert!(state.awaiting_keyframe);
            assert_eq!(state.frames.len(), 1);
            assert!(state.pending_bytes <= MAX_PENDING_BYTES);
        }

        queue.push(frame(1, true, 9), true);
        let recovered = queue.pop().unwrap();
        assert_eq!(recovered.as_slice()[4], 9);
        let state = queue.state.lock().unwrap();
        assert_eq!(state.pending_bytes, 0);
        assert!(!state.awaiting_keyframe);
    }
}
