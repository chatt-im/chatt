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
        if let Some(web) = &self.inner.web {
            web.send_video_frame(frame.clone());
        }
        let bytes = frame.as_slice();
        if bytes.len() < rpc::video::VIDEO_FRAME_HEADER_LEN {
            return;
        }
        let stream_id = StreamId(u32::from_le_bytes(bytes[13..17].try_into().unwrap()));
        let is_key = bytes[12] == 1;
        let mut state = self.inner.state.lock().unwrap();
        state
            .fast_start
            .entry(stream_id)
            .or_default()
            .push(stream_id, frame.clone(), is_key);
        for ((_, id), queue) in &state.native {
            if *id == stream_id {
                queue.push(frame.clone(), is_key);
            }
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
        if self.bytes.saturating_add(frame.retained_bytes()) > FAST_START_MAX_BYTES {
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
            state.frames.clear();
            state.awaiting_keyframe = false;
        } else if state.frames.len()
            >= if state.bootstrapping {
                MAX_BOOTSTRAP_FRAMES
            } else {
                MAX_PENDING_FRAMES
            }
        {
            if is_key {
                state.frames.clear();
            } else {
                state.awaiting_keyframe = true;
                state.bootstrapping = false;
                return;
            }
        }
        state.frames.push_back(frame);
        self.ready.notify_one();
    }

    fn pop(&self) -> Option<SharedVideoFrame> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(frame) = state.frames.pop_front() {
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
        state.frames.clear();
        self.ready.notify_all();
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
}
