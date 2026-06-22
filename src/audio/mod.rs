use hashbrown::{HashMap, HashSet};
use std::{
    cell::UnsafeCell,
    collections::VecDeque,
    fmt, fs,
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    process::Command,
    ptr::NonNull,
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use cpal::{
    BufferSize, FromSample, Sample, SampleFormat, Stream, StreamConfig, SupportedBufferSize,
    SupportedStreamConfig, traits::DeviceTrait, traits::HostTrait, traits::StreamTrait,
};
use earshot::Detector as EarshotDetector;
use nnnoiseless::DenoiseState;
use opus_codec::{Channels, Complexity, Decoder, DredDecoder, DredState, SampleRate};
use sonora::config::EchoCanceller as Aec3Config;
use sonora::{AudioProcessing, Config as ApmConfig, StreamConfig as ApmStreamConfig};

use crate::network::{
    AudioPacketRef, EncoderNetworkProfile, EncoderNetworkTuning, InsertOutcome, JitterBuffer,
    JitterBufferConfig, PlayoutItem,
};
use crate::packet_log::{FLAG_DENOISE, PacketLogHeader, PacketLogReader, PacketLogWriter};

mod capture;
mod device;
mod lifecycle;
mod playback;
mod shared;
mod sim;

pub use capture::*;
pub use device::*;
pub use lifecycle::*;
pub use shared::*;
pub use sim::*;

pub(in crate::audio) use playback::*;

#[cfg(test)]
mod test_support;
