use std::{
    collections::{HashMap, VecDeque},
    io::Write,
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
};

use rpc::{ids::StreamId, video::SharedVideoFrame};

use crate::web_server::WebFeedSender;

const MAX_PENDING_FRAMES: usize = 2;
const MAX_BOOTSTRAP_FRAMES: usize = 90;
const MAX_PENDING_BYTES: usize = rpc::video::MAX_VIDEO_FRAME_LEN;
const FAST_START_MAX_BYTES: usize = rpc::video::MAX_VIDEO_FRAME_LEN;

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
    native: HashMap<(u64, StreamId), Arc<NativeQueue>>,
    fast_start: HashMap<StreamId, NativeFastStart>,
}

#[derive(Default)]
struct NativeFastStart {
    frames: VecDeque<SharedVideoFrame>,
    bytes: usize,
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
    closed: bool,
    received_frames: u64,
    written_frames: u64,
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
            return;
        };
        if consumed != frame.len() {
            return;
        }
        let stream_id = StreamId(parsed.stream_id);
        let is_key = parsed.is_key;
        if let Some(web) = &self.inner.web {
            web.send_video_frame(frame.clone());
        }
        let queues = {
            let mut state = self.inner.state.lock().unwrap();
            state
                .fast_start
                .entry(stream_id)
                .or_default()
                .push(stream_id, frame.clone(), is_key);
            state
                .native
                .iter()
                .filter(|((_, id), _)| *id == stream_id)
                .map(|(_, queue)| queue.clone())
                .collect::<Vec<_>>()
        };
        for queue in queues {
            queue.push(frame.clone(), is_key);
        }
    }

    pub fn add_native(
        &self,
        viewer_id: u64,
        stream_id: StreamId,
        stream: UnixStream,
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
            if state.native.contains_key(&key) {
                return Err("screen share is already playing".into());
            }
            let cached = state.fast_start.get(&stream_id);
            let cached_frames = cached.map_or(0, |cache| cache.frames.len());
            let cached_bytes = cached.map_or(0, |cache| cache.bytes);
            if let Some(cached) = cached {
                queue.seed(&cached.frames);
            }
            kvlog::info!(
                "native video viewer registered",
                viewer_id,
                stream_id = stream_id.0,
                cached_frames,
                cached_bytes
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

    pub fn has_native(&self, stream_id: StreamId) -> bool {
        self.inner
            .state
            .lock()
            .unwrap()
            .native
            .keys()
            .any(|(_, id)| *id == stream_id)
    }

    pub fn close_stream(&self, stream_id: StreamId) {
        let queues = {
            let mut state = self.inner.state.lock().unwrap();
            state.fast_start.remove(&stream_id);
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
    fn seed(&self, frames: &VecDeque<SharedVideoFrame>) {
        let mut state = self.state.lock().unwrap();
        state.frames.extend(frames.iter().cloned());
        state.pending_bytes = state
            .frames
            .iter()
            .map(SharedVideoFrame::retained_bytes)
            .sum();
        state.seen_keyframe = !state.frames.is_empty();
        state.bootstrapping = !state.frames.is_empty();
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
                return;
            }
            state.seen_keyframe = true;
        }
        if state.awaiting_keyframe {
            if !is_key {
                return;
            }
            state.clear_frames();
            state.awaiting_keyframe = false;
        } else {
            let frame_limit = if state.bootstrapping {
                MAX_BOOTSTRAP_FRAMES
            } else {
                MAX_PENDING_FRAMES
            };
            let over_limit = state.frames.len() >= frame_limit
                || state.pending_bytes.saturating_add(frame.retained_bytes())
                    > MAX_PENDING_BYTES;
            if over_limit {
                if is_key {
                    state.clear_frames();
                } else {
                    state.awaiting_keyframe = true;
                    state.bootstrapping = false;
                    return;
                }
            }
        }
        if frame.retained_bytes() > MAX_PENDING_BYTES {
            state.awaiting_keyframe = true;
            state.bootstrapping = false;
            return;
        }
        state.pending_bytes += frame.retained_bytes();
        state.frames.push_back(frame);
        self.ready.notify_one();
    }

    fn pop(&self) -> Option<SharedVideoFrame> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(frame) = state.frames.pop_front() {
                state.pending_bytes -= frame.retained_bytes();
                state.written_frames += 1;
                if state.frames.is_empty() {
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

    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.closed = true;
        state.clear_frames();
        self.ready.notify_all();
    }
}

impl QueueState {
    fn clear_frames(&mut self) {
        self.frames.clear();
        self.pending_bytes = 0;
    }
}

fn native_writer_loop(mut stream: UnixStream, queue: Arc<NativeQueue>) {
    let mut written = 0u64;
    while let Some(frame) = queue.pop() {
        match stream.write_all(frame.as_slice()) {
            Ok(()) => {
                written += 1;
                if written == 1 {
                    kvlog::info!(
                        "native video first frame written",
                        viewer_id = queue.viewer_id,
                        stream_id = queue.stream_id.0,
                        keyframe = frame.as_slice().get(12) == Some(&1),
                        frame_bytes = frame.len()
                    );
                }
            }
            Err(error) => {
                kvlog::warn!(
                    "native video writer stopped",
                    viewer_id = queue.viewer_id,
                    stream_id = queue.stream_id.0,
                    written_frames = written,
                    error = %error
                );
                break;
            }
        }
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
            .add_native(11, StreamId(7), daemon_stream)
            .unwrap();
        let mut first = vec![0; rpc::video::VIDEO_FRAME_HEADER_LEN + 1];
        frontend_stream.read_exact(&mut first).unwrap();
        let mut second = vec![0; rpc::video::VIDEO_FRAME_HEADER_LEN + 1];
        frontend_stream.read_exact(&mut second).unwrap();
        assert_eq!(first[4], 3);
        assert_eq!(first[12], 1);
        assert_eq!(second[4], 4);
        assert_eq!(second[12], 0);
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
