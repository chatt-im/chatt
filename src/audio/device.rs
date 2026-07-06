use std::{
    mem,
    num::NonZeroUsize,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{Receiver, SyncSender},
    },
    time::{Duration, Instant},
};

use cpal::{
    BufferSize, ErrorKind, FromSample, Sample, SampleFormat, Stream, StreamConfig,
    SupportedBufferSize, SupportedStreamConfig,
    traits::{DeviceTrait, HostTrait},
};
use hashbrown::HashSet;

use crate::audio::{
    backend::with_audio_backend_stderr_suppressed,
    capture::EchoCancellationControl,
    diagnostics::LivePlaybackWavRecorderHandle,
    errors::{AudioErrorKind, AudioStartError},
    playback::{
        LivePlaybackMixer, LivePlaybackMixerEvent, LivePlaybackSharedSnapshot, MIX_FRAME_SAMPLES,
        SpscSwapQueue,
    },
    resample::PlaybackResampler,
    shared::{
        AudioStats, BufferRequest, PlaybackStats, SAMPLE_RATE, audio_callback_logging_enabled,
        audio_pop_logging_enabled, peak_i16_scale, rms_i16_scale, samples_to_duration,
    },
};

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub id: Option<String>,
    pub name: String,
    pub supported: bool,
    pub preview: Option<StreamPreview>,
    pub issue: Option<String>,
}

#[derive(Clone, Debug)]
pub struct StreamPreview {
    pub channels: u16,
    pub sample_format: SampleFormat,
    pub buffer_size: BufferSize,
    pub buffer_note: String,
}

pub fn input_devices(buffer_request: BufferRequest) -> Result<Vec<DeviceInfo>, String> {
    with_audio_backend_stderr_suppressed(|| input_devices_inner(buffer_request))
}

pub fn output_devices(buffer_request: BufferRequest) -> Result<Vec<DeviceInfo>, String> {
    with_audio_backend_stderr_suppressed(|| output_devices_inner(buffer_request))
}

pub fn stable_input_device_id(name: &str) -> String {
    stable_device_id(name)
}

pub fn stable_output_device_id(name: &str) -> String {
    stable_device_id(name)
}

pub(crate) fn stable_device_id(name: &str) -> String {
    let mut key = name.to_ascii_lowercase();
    for suffix in [", usb audio", ", loopback pcm"] {
        if let Some(stripped) = key.strip_suffix(suffix) {
            key = stripped.to_string();
        }
    }
    key.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn input_devices_inner(
    buffer_request: BufferRequest,
) -> Result<Vec<DeviceInfo>, String> {
    let host = cpal::default_host();
    let devices = host
        .input_devices()
        .map_err(|error| format!("failed to list input devices: {error}"))?;

    let mut infos = Vec::new();
    let mut seen_ids = HashSet::new();
    for device in devices {
        if !device_matches_picker_direction(&device, AudioDeviceDirection::Input) {
            continue;
        }
        let info = input_device_info(&device, None, buffer_request);
        if let Some(id) = &info.id {
            seen_ids.insert(id.clone());
        }
        infos.push(info);
    }
    append_alsa_physical_devices(
        &host,
        AudioDeviceDirection::Input,
        buffer_request,
        &mut seen_ids,
        &mut infos,
    );

    Ok(infos)
}

pub(crate) fn output_devices_inner(
    buffer_request: BufferRequest,
) -> Result<Vec<DeviceInfo>, String> {
    let host = cpal::default_host();
    let devices = host
        .output_devices()
        .map_err(|error| format!("failed to list output devices: {error}"))?;

    let mut infos = Vec::new();
    let mut seen_ids = HashSet::new();
    for device in devices {
        if !device_matches_picker_direction(&device, AudioDeviceDirection::Output) {
            continue;
        }
        let info = output_device_info(&device, None, buffer_request);
        if let Some(id) = &info.id {
            seen_ids.insert(id.clone());
        }
        infos.push(info);
    }
    append_alsa_physical_devices(
        &host,
        AudioDeviceDirection::Output,
        buffer_request,
        &mut seen_ids,
        &mut infos,
    );

    Ok(infos)
}

pub(crate) fn input_device_info(
    device: &cpal::Device,
    name_override: Option<String>,
    buffer_request: BufferRequest,
) -> DeviceInfo {
    let id = cpal_device_id(device);
    let name = name_override.unwrap_or_else(|| device.to_string());
    match select_input_config(device, buffer_request) {
        Ok(selection) => DeviceInfo {
            id,
            name,
            supported: true,
            preview: Some(selection.preview),
            issue: None,
        },
        Err(error) => DeviceInfo {
            id,
            name,
            supported: false,
            preview: None,
            issue: Some(error),
        },
    }
}

pub(crate) fn output_device_info(
    device: &cpal::Device,
    name_override: Option<String>,
    buffer_request: BufferRequest,
) -> DeviceInfo {
    let id = cpal_device_id(device);
    let name = name_override.unwrap_or_else(|| device.to_string());
    match select_output_config(device, buffer_request) {
        Ok(selection) => DeviceInfo {
            id,
            name,
            supported: true,
            preview: Some(selection.preview),
            issue: None,
        },
        Err(error) => DeviceInfo {
            id,
            name,
            supported: false,
            preview: None,
            issue: Some(error),
        },
    }
}

pub(crate) fn device_matches_picker_direction(
    device: &cpal::Device,
    direction: AudioDeviceDirection,
) -> bool {
    let Some(id) = cpal_device_id(device) else {
        return true;
    };
    let Some(node_name) = id.strip_prefix("pipewire:") else {
        return true;
    };
    if pipewire_device_id_is_hidden_from_picker(node_name) {
        return false;
    }
    pipewire_device_id_matches_picker_direction(node_name, direction)
        || device.description().is_ok_and(|description| {
            pipewire_description_matches_picker_direction(&description, direction)
        })
}

pub(crate) fn pipewire_device_id_is_hidden_from_picker(node_name: &str) -> bool {
    let node_name = node_name.to_ascii_lowercase();
    matches!(
        node_name.as_str(),
        "sink_default" | "input_default" | "output_default"
    ) || node_name.starts_with("alsa_capture.")
        || node_name.starts_with("alsa_playback.")
}

pub(crate) fn pipewire_device_id_matches_picker_direction(
    node_name: &str,
    direction: AudioDeviceDirection,
) -> bool {
    let node_name = node_name.to_ascii_lowercase();
    if pipewire_device_id_is_hidden_from_picker(&node_name) {
        return false;
    }

    match direction {
        AudioDeviceDirection::Input => {
            node_name.starts_with("alsa_input.") || node_name.starts_with("bluez_input.")
        }
        AudioDeviceDirection::Output => {
            node_name.starts_with("alsa_output.") || node_name.starts_with("bluez_output.")
        }
    }
}

pub(crate) fn pipewire_description_matches_picker_direction(
    description: &cpal::DeviceDescription,
    direction: AudioDeviceDirection,
) -> bool {
    match direction {
        AudioDeviceDirection::Input => {
            description.supports_input()
                && matches!(
                    description.device_type(),
                    cpal::DeviceType::Microphone
                        | cpal::DeviceType::Headset
                        | cpal::DeviceType::Handset
                )
        }
        AudioDeviceDirection::Output => {
            description.supports_output()
                && matches!(
                    description.device_type(),
                    cpal::DeviceType::Speaker
                        | cpal::DeviceType::Headphones
                        | cpal::DeviceType::Headset
                        | cpal::DeviceType::Earpiece
                        | cpal::DeviceType::Handset
                        | cpal::DeviceType::HearingAid
                )
        }
    }
}

pub(crate) fn cpal_device_id(device: &cpal::Device) -> Option<String> {
    device.id().ok().map(|id| id.to_string())
}

pub(crate) fn cpal_device_matches_config_id(device: &cpal::Device, configured_id: &str) -> bool {
    if let Some(device_id) = cpal_device_id(device) {
        if device_id == configured_id {
            return true;
        }
        if let Some(alsa_pcm_id) = device_id.strip_prefix("alsa:")
            && alsa_pcm_id == configured_id
        {
            return true;
        }
    }

    let Some(parsed_id) = parse_configured_device_id(configured_id) else {
        return false;
    };
    device.id().is_ok_and(|device_id| device_id == parsed_id)
}

/// Returns `true` when `configured_id` matches the current system default output
/// device by cpal DeviceId or by stable name. When the user selects the concrete
/// device that is also the OS default, prefer the backend's default-device path
/// so hosts that support default rerouting can follow device/profile changes.
pub fn configured_output_is_default(configured_id: &str) -> bool {
    let host = cpal::default_host();
    let Some(default_device) = host.default_output_device() else {
        return false;
    };
    if let Ok(device_id) = default_device.id().map(|id| id.to_string()) {
        if device_id == configured_id {
            return true;
        }
    }
    let name = default_device.to_string();
    stable_output_device_id(&name) == configured_id
}

pub(crate) fn cpal_device_from_config_id(
    host: &cpal::Host,
    configured_id: &str,
) -> Option<cpal::Device> {
    let id = parse_configured_device_id(configured_id)?;
    host.device_by_id(&id)
        .or_else(|| cpal::host_from_id(id.host()).ok()?.device_by_id(&id))
}

pub(crate) fn parse_configured_device_id(configured_id: &str) -> Option<cpal::DeviceId> {
    let configured_id = configured_id.trim();
    if configured_id.is_empty() {
        return None;
    }
    if let Some(alsa_pcm) = configured_id.strip_prefix("alsa/")
        && let Some(id) = forced_alsa_device_id_from_pcm_name(alsa_pcm)
    {
        return Some(id);
    }
    if let Ok(id) = cpal::DeviceId::from_str(configured_id) {
        return Some(id);
    }
    alsa_device_id_from_pcm_name(configured_id)
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
))]
pub(crate) fn forced_alsa_device_id_from_pcm_name(pcm_name: &str) -> Option<cpal::DeviceId> {
    (!pcm_name.is_empty()).then(|| cpal::DeviceId::new(cpal::HostId::Alsa, pcm_name))
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
)))]
pub(crate) fn forced_alsa_device_id_from_pcm_name(_pcm_name: &str) -> Option<cpal::DeviceId> {
    None
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
))]
pub(crate) fn alsa_device_id_from_pcm_name(pcm_name: &str) -> Option<cpal::DeviceId> {
    looks_like_alsa_pcm_name(pcm_name).then(|| cpal::DeviceId::new(cpal::HostId::Alsa, pcm_name))
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
)))]
pub(crate) fn alsa_device_id_from_pcm_name(_pcm_name: &str) -> Option<cpal::DeviceId> {
    None
}

pub fn looks_like_alsa_pcm_name(value: &str) -> bool {
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return false;
    }
    let head = value
        .split([':', ','])
        .next()
        .unwrap_or(value)
        .to_ascii_lowercase();

    matches!(
        head.as_str(),
        "default"
            | "sysdefault"
            | "hw"
            | "plughw"
            | "plug"
            | "front"
            | "center_lfe"
            | "side"
            | "iec958"
            | "spdif"
            | "dmix"
            | "dsnoop"
            | "pulse"
            | "pipewire"
            | "jack"
            | "oss"
            | "null"
            | "usbstream"
    ) || head.starts_with("surround")
        || head.starts_with("hdmi")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AudioDeviceDirection {
    Input,
    Output,
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
))]
pub(crate) fn append_alsa_physical_devices(
    host: &cpal::Host,
    direction: AudioDeviceDirection,
    buffer_request: BufferRequest,
    seen_ids: &mut HashSet<String>,
    infos: &mut Vec<DeviceInfo>,
) {
    for pcm in alsa_physical_pcm_devices(direction) {
        for prefix in ["plughw", "hw"] {
            let pcm_id = format!("{prefix}:CARD={},DEV={}", pcm.card, pcm.device);
            let id = format!("alsa:{pcm_id}");
            if !seen_ids.insert(id.clone()) {
                continue;
            }
            let Some(device) = cpal_device_from_config_id(host, &id) else {
                continue;
            };
            let name = format!("{} ({pcm_id})", pcm.name);
            let info = match direction {
                AudioDeviceDirection::Input => {
                    input_device_info(&device, Some(name), buffer_request)
                }
                AudioDeviceDirection::Output => {
                    output_device_info(&device, Some(name), buffer_request)
                }
            };
            infos.push(info);
        }
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
)))]
pub(crate) fn append_alsa_physical_devices(
    _host: &cpal::Host,
    _direction: AudioDeviceDirection,
    _buffer_request: BufferRequest,
    _seen_ids: &mut HashSet<String>,
    _infos: &mut Vec<DeviceInfo>,
) {
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AlsaPhysicalPcm {
    card: u32,
    device: u32,
    name: String,
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
))]
pub(crate) fn alsa_physical_pcm_devices(direction: AudioDeviceDirection) -> Vec<AlsaPhysicalPcm> {
    std::fs::read_to_string("/proc/asound/pcm")
        .map(|content| parse_alsa_physical_pcm_devices(&content, direction))
        .unwrap_or_default()
}

pub(crate) fn parse_alsa_physical_pcm_devices(
    content: &str,
    direction: AudioDeviceDirection,
) -> Vec<AlsaPhysicalPcm> {
    let mut devices = Vec::new();
    for line in content.lines() {
        let Some((address, rest)) = line.split_once(':') else {
            continue;
        };
        let Some((card, device)) = parse_alsa_pcm_address(address.trim()) else {
            continue;
        };
        let fields: Vec<&str> = rest.split(':').map(str::trim).collect();
        let supports_direction = match direction {
            AudioDeviceDirection::Input => fields.iter().any(|field| field.starts_with("capture")),
            AudioDeviceDirection::Output => {
                fields.iter().any(|field| field.starts_with("playback"))
            }
        };
        if !supports_direction {
            continue;
        }
        let name = fields
            .iter()
            .find(|field| !field.is_empty())
            .copied()
            .unwrap_or("ALSA PCM")
            .to_string();
        devices.push(AlsaPhysicalPcm { card, device, name });
    }
    devices
}

pub(crate) fn parse_alsa_pcm_address(address: &str) -> Option<(u32, u32)> {
    let (card, device) = address.split_once('-')?;
    Some((card.parse().ok()?, device.parse().ok()?))
}

/// Identity of one present audio device, as seen by a cheap enumeration scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceIdentity {
    /// cpal device id string, when the backend provides one.
    pub id: Option<String>,
    /// Normalized name identity (see [`stable_input_device_id`]).
    pub stable_id: String,
    pub name: String,
}

impl DeviceIdentity {
    fn from_device(device: &cpal::Device) -> Self {
        let name = device.to_string();
        Self {
            id: cpal_device_id(device),
            stable_id: stable_device_id(&name),
            name,
        }
    }

    /// True when this device matches a configured device id or a stream's
    /// stable id, accepting the same spellings as
    /// [`cpal_device_matches_config_id`]: raw cpal id, stable name, bare ALSA
    /// PCM name, or `alsa/<pcm>`.
    pub fn matches_target(&self, target: &str) -> bool {
        if self.stable_id == target {
            return true;
        }
        let Some(id) = self.id.as_deref() else {
            return false;
        };
        if id == target {
            return true;
        }
        let Some(alsa_pcm) = id.strip_prefix("alsa:") else {
            return false;
        };
        alsa_pcm == target || target.strip_prefix("alsa/") == Some(alsa_pcm)
    }
}

/// Snapshot of present devices and OS defaults, by identity only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceIdentityProbe {
    pub default_input: Option<DeviceIdentity>,
    pub default_output: Option<DeviceIdentity>,
    pub inputs: Vec<DeviceIdentity>,
    pub outputs: Vec<DeviceIdentity>,
}

impl DeviceIdentityProbe {
    pub fn inputs_contain(&self, target: &str) -> bool {
        self.inputs
            .iter()
            .any(|identity| identity.matches_target(target))
    }

    pub fn outputs_contain(&self, target: &str) -> bool {
        self.outputs
            .iter()
            .any(|identity| identity.matches_target(target))
    }
}

/// Enumerates device identities and OS defaults without negotiating stream
/// configs, so no PCM is opened. Cheap enough for periodic polling from a
/// background thread; still never call it on the UI thread.
pub fn probe_device_identities() -> Result<DeviceIdentityProbe, String> {
    with_audio_backend_stderr_suppressed(probe_device_identities_inner)
}

fn probe_device_identities_inner() -> Result<DeviceIdentityProbe, String> {
    let host = cpal::default_host();
    let mut probe = DeviceIdentityProbe::default();
    let inputs = host
        .input_devices()
        .map_err(|error| format!("failed to list input devices: {error}"))?;
    for device in inputs {
        if !device_matches_picker_direction(&device, AudioDeviceDirection::Input) {
            continue;
        }
        probe.inputs.push(DeviceIdentity::from_device(&device));
    }
    let outputs = host
        .output_devices()
        .map_err(|error| format!("failed to list output devices: {error}"))?;
    for device in outputs {
        if !device_matches_picker_direction(&device, AudioDeviceDirection::Output) {
            continue;
        }
        probe.outputs.push(DeviceIdentity::from_device(&device));
    }
    append_alsa_physical_identities(AudioDeviceDirection::Input, &mut probe.inputs);
    append_alsa_physical_identities(AudioDeviceDirection::Output, &mut probe.outputs);
    probe.default_input = host
        .default_input_device()
        .map(|device| DeviceIdentity::from_device(&device));
    probe.default_output = host
        .default_output_device()
        .map(|device| DeviceIdentity::from_device(&device));
    Ok(probe)
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
))]
fn append_alsa_physical_identities(
    direction: AudioDeviceDirection,
    list: &mut Vec<DeviceIdentity>,
) {
    for pcm in alsa_physical_pcm_devices(direction) {
        for prefix in ["plughw", "hw"] {
            let pcm_id = format!("{prefix}:CARD={},DEV={}", pcm.card, pcm.device);
            let name = format!("{} ({pcm_id})", pcm.name);
            list.push(DeviceIdentity {
                id: Some(format!("alsa:{pcm_id}")),
                stable_id: stable_device_id(&name),
                name,
            });
        }
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
)))]
fn append_alsa_physical_identities(
    _direction: AudioDeviceDirection,
    _list: &mut Vec<DeviceIdentity>,
) {
}

/// Why a configured device id failed to resolve to an opened config, split by
/// the recovery strategy the failure calls for.
pub(crate) enum DeviceSelectError {
    /// No present device matches the configured id.
    NotFound(String),
    /// A matching device exists but offers no usable config.
    Unsupported(String),
    /// Device enumeration itself failed.
    Backend(String),
}

impl From<DeviceSelectError> for String {
    fn from(error: DeviceSelectError) -> Self {
        match error {
            DeviceSelectError::NotFound(message)
            | DeviceSelectError::Unsupported(message)
            | DeviceSelectError::Backend(message) => message,
        }
    }
}

impl From<DeviceSelectError> for AudioStartError {
    fn from(error: DeviceSelectError) -> Self {
        match error {
            DeviceSelectError::NotFound(message) => {
                AudioStartError::new(AudioErrorKind::DeviceGone, message)
            }
            DeviceSelectError::Unsupported(message) => {
                AudioStartError::new(AudioErrorKind::ConfigInvalid, message)
            }
            DeviceSelectError::Backend(message) => AudioStartError::transient(message),
        }
    }
}

pub(crate) fn select_input_device_by_id(
    host: &cpal::Host,
    id: &str,
    buffer_request: BufferRequest,
) -> Result<(cpal::Device, ConfigSelection), DeviceSelectError> {
    let devices = host.input_devices().map_err(|error| {
        DeviceSelectError::Backend(format!("failed to list input devices: {error}"))
    })?;
    let mut matched = false;
    let mut first_error = None;
    for device in devices {
        let name = device.to_string();
        if !cpal_device_matches_config_id(&device, id) && stable_input_device_id(&name) != id {
            continue;
        }
        matched = true;
        match select_input_config(&device, buffer_request) {
            Ok(selection) => return Ok((device, selection)),
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }

    if matched {
        Err(DeviceSelectError::Unsupported(format!(
            "selected input device `{id}` is present but unsupported: {}",
            first_error.unwrap_or_else(|| "no supported PCM input config".to_string())
        )))
    } else if let Some(device) = cpal_device_from_config_id(host, id) {
        select_input_config(&device, buffer_request)
            .map(|selection| (device, selection))
            .map_err(|error| {
                DeviceSelectError::Unsupported(format!(
                    "configured input device `{id}` could not be opened: {error}"
                ))
            })
    } else {
        Err(DeviceSelectError::NotFound(format!(
            "selected input device `{id}` is unavailable"
        )))
    }
}

pub(crate) fn select_output_device_by_id(
    host: &cpal::Host,
    id: &str,
    buffer_request: BufferRequest,
) -> Result<(cpal::Device, ConfigSelection), DeviceSelectError> {
    let devices = host.output_devices().map_err(|error| {
        DeviceSelectError::Backend(format!("failed to list output devices: {error}"))
    })?;
    let mut matched = false;
    let mut first_error = None;
    for device in devices {
        let name = device.to_string();
        if !cpal_device_matches_config_id(&device, id) && stable_output_device_id(&name) != id {
            continue;
        }
        matched = true;
        match select_output_config(&device, buffer_request) {
            Ok(selection) => return Ok((device, selection)),
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }

    if matched {
        Err(DeviceSelectError::Unsupported(format!(
            "selected output device `{id}` is present but unsupported: {}",
            first_error.unwrap_or_else(|| "no supported PCM output config".to_string())
        )))
    } else if let Some(device) = cpal_device_from_config_id(host, id) {
        select_output_config(&device, buffer_request)
            .map(|selection| (device, selection))
            .map_err(|error| {
                DeviceSelectError::Unsupported(format!(
                    "configured output device `{id}` could not be opened: {error}"
                ))
            })
    } else {
        Err(DeviceSelectError::NotFound(format!(
            "selected output device `{id}` is unavailable"
        )))
    }
}

pub(crate) struct ConfigSelection {
    pub(crate) supported_config: SupportedStreamConfig,
    pub(crate) stream_config: StreamConfig,
    /// Sample rate the device stream actually runs at. Equals [`SAMPLE_RATE`]
    /// for native devices, otherwise a fallback rate the capture/playback paths
    /// resample to and from 48 kHz.
    pub(crate) device_rate: u32,
    pub(crate) preview: StreamPreview,
}

/// Device sample rates the capture and playback paths can resample to and from
/// 48 kHz, in descending preference. Each is divisible by 100 so a 10 ms
/// resampler block is a whole number of samples on both sides of the
/// conversion, and each exceeds the sinc kernel's minimum block.
pub(crate) const RESAMPLE_FALLBACK_RATES: [u32; 7] =
    [96_000, 88_200, 44_100, 32_000, 24_000, 16_000, 8_000];

/// The rate one supported config range will run at: native 48 kHz when the
/// range covers it, otherwise the most preferred fallback rate it covers.
/// Returns `None` when the range covers no usable rate.
pub(crate) fn range_stream_rate(range: &cpal::SupportedStreamConfigRange) -> Option<u32> {
    if range.contains_rate(SAMPLE_RATE) {
        return Some(SAMPLE_RATE);
    }
    RESAMPLE_FALLBACK_RATES
        .iter()
        .copied()
        .find(|rate| range.contains_rate(*rate))
}

/// Sort rank for a chosen device rate. Native 48 kHz ranks first so a device
/// that also offers 48 kHz never resamples, then fallbacks in preference order.
fn rate_rank(rate: u32) -> usize {
    if rate == SAMPLE_RATE {
        return 0;
    }
    RESAMPLE_FALLBACK_RATES
        .iter()
        .position(|candidate| *candidate == rate)
        .map(|index| index + 1)
        .unwrap_or(usize::MAX)
}

pub(crate) fn select_input_config(
    device: &cpal::Device,
    buffer_request: BufferRequest,
) -> Result<ConfigSelection, String> {
    let mut candidates = Vec::new();
    let ranges = device
        .supported_input_configs()
        .map_err(|error| format!("failed to query input configs: {error}"))?;

    for range in ranges {
        if range.sample_format().is_dsd() {
            continue;
        }
        let Some(rate) = range_stream_rate(&range) else {
            continue;
        };
        let supported_config = range.with_sample_rate(rate);
        candidates.push((supported_config, *range.buffer_size()));
    }

    candidates.sort_by_key(|(config, _)| {
        (
            rate_rank(config.sample_rate()),
            channel_rank(config.channels()),
            sample_format_rank(config.sample_format()),
        )
    });

    let (supported_config, supported_buffer_size) = candidates
        .into_iter()
        .next()
        .ok_or_else(|| "no usable PCM input config available".to_string())?;

    let device_rate = supported_config.sample_rate();
    let (buffer_size, buffer_note) = select_buffer_size(buffer_request, supported_buffer_size);
    let mut stream_config = supported_config.config();
    stream_config.buffer_size = buffer_size;

    Ok(ConfigSelection {
        preview: StreamPreview {
            channels: supported_config.channels(),
            sample_format: supported_config.sample_format(),
            buffer_size,
            buffer_note,
        },
        device_rate,
        supported_config,
        stream_config,
    })
}

pub(crate) fn select_output_config(
    device: &cpal::Device,
    buffer_request: BufferRequest,
) -> Result<ConfigSelection, String> {
    let mut candidates = Vec::new();
    let ranges = device
        .supported_output_configs()
        .map_err(|error| format!("failed to query output configs: {error}"))?;

    for range in ranges {
        if range.sample_format().is_dsd() {
            continue;
        }
        let Some(rate) = range_stream_rate(&range) else {
            continue;
        };
        let supported_config = range.with_sample_rate(rate);
        candidates.push((supported_config, *range.buffer_size()));
    }

    candidates.sort_by_key(|(config, _)| {
        (
            rate_rank(config.sample_rate()),
            output_channel_rank(config.channels()),
            sample_format_rank(config.sample_format()),
        )
    });

    let (supported_config, supported_buffer_size) = candidates
        .into_iter()
        .next()
        .ok_or_else(|| "no usable PCM output config available".to_string())?;

    let device_rate = supported_config.sample_rate();
    let (buffer_size, buffer_note) = select_buffer_size(buffer_request, supported_buffer_size);
    let mut stream_config = supported_config.config();
    stream_config.buffer_size = buffer_size;

    Ok(ConfigSelection {
        preview: StreamPreview {
            channels: supported_config.channels(),
            sample_format: supported_config.sample_format(),
            buffer_size,
            buffer_note,
        },
        device_rate,
        supported_config,
        stream_config,
    })
}

pub(crate) fn channel_rank(channels: u16) -> u16 {
    match channels {
        1 => 0,
        2 => 1,
        other => other.saturating_add(2),
    }
}

pub(crate) fn output_channel_rank(channels: u16) -> u16 {
    match channels {
        2 => 0,
        1 => 1,
        other => other.saturating_add(2),
    }
}

pub(crate) fn sample_format_rank(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::F32 => 0,
        SampleFormat::I16 => 1,
        SampleFormat::I24 => 2,
        SampleFormat::I32 => 3,
        SampleFormat::F64 => 4,
        SampleFormat::U16 => 5,
        SampleFormat::U24 => 6,
        SampleFormat::U32 => 7,
        SampleFormat::I8 => 8,
        SampleFormat::U8 => 9,
        SampleFormat::I64 => 10,
        SampleFormat::U64 => 11,
        _ => 100,
    }
}

pub(crate) fn select_buffer_size(
    request: BufferRequest,
    supported: SupportedBufferSize,
) -> (BufferSize, String) {
    match request {
        BufferRequest::Default => (BufferSize::Default, "host default".to_string()),
        BufferRequest::Fixed(requested) => match supported {
            // The PipeWire host advertises 0..=0 until the server's clock
            // metadata arrives. Clamping into that range would request a
            // zero-sized buffer, so keep the requested size as a preference.
            SupportedBufferSize::Range { min: 0, max: 0 } => (
                BufferSize::Fixed(requested),
                format!("requested {requested}; quantum range unknown"),
            ),
            SupportedBufferSize::Range { min, max } if requested >= min && requested <= max => (
                BufferSize::Fixed(requested),
                format!("requested {requested} frames"),
            ),
            SupportedBufferSize::Range { min, max } => {
                let clamped = requested.clamp(min, max);
                (
                    BufferSize::Fixed(clamped),
                    format!("requested {requested}, using {clamped}"),
                )
            }
            SupportedBufferSize::Unknown => (
                BufferSize::Fixed(requested),
                format!("requested {requested}; support unknown"),
            ),
        },
    }
}

pub(crate) fn audio_buffer_size_label(buffer_size: BufferSize) -> String {
    match buffer_size {
        BufferSize::Default => "default".to_string(),
        BufferSize::Fixed(frames) => format!("{frames} frames"),
    }
}

#[derive(Clone, Copy, Debug)]
struct CallbackChannelCount(NonZeroUsize);

impl CallbackChannelCount {
    fn new(channels: usize, direction: &'static str) -> Result<Self, String> {
        NonZeroUsize::new(channels)
            .map(Self)
            .ok_or_else(|| format!("{direction} stream reported zero channels"))
    }

    fn get(self) -> usize {
        self.0.get()
    }

    fn frames_for_interleaved(self, interleaved_samples: usize) -> usize {
        interleaved_samples / self.get()
    }
}

pub(crate) struct AudioCallbackBufferObserver {
    direction: &'static str,
    last_frames: AtomicUsize,
}

impl AudioCallbackBufferObserver {
    pub(crate) fn new(direction: &'static str) -> Self {
        Self {
            direction,
            last_frames: AtomicUsize::new(usize::MAX),
        }
    }

    fn observe(&self, interleaved_samples: usize, channels: CallbackChannelCount) {
        let frames = channels.frames_for_interleaved(interleaved_samples);
        let previous = self.last_frames.swap(frames, Ordering::Relaxed);
        if previous == frames {
            return;
        }
        if !audio_callback_logging_enabled() {
            return;
        }

        kvlog::info!(
            "live audio callback buffer observed",
            direction = self.direction,
            observed_buffer_frames = frames,
            observed_buffer_ms = frames as f64 * 1000.0 / SAMPLE_RATE as f64,
            channels = channels.get(),
            interleaved_samples = interleaved_samples,
            changed = previous != usize::MAX
        );
    }
}

pub(crate) fn build_input_stream(
    device: &cpal::Device,
    sample_format: SampleFormat,
    stream_config: StreamConfig,
    channels: usize,
    sender: SyncSender<Vec<f32>>,
    recycle: Receiver<Vec<f32>>,
    stats: AudioStats,
    callback_buffer_observer: Option<Arc<AudioCallbackBufferObserver>>,
) -> Result<Stream, String> {
    let channels = CallbackChannelCount::new(channels, "input")?;
    match sample_format {
        SampleFormat::I8 => build_typed_input_stream::<i8>(
            device,
            stream_config,
            channels,
            sender,
            recycle,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::I16 => build_typed_input_stream::<i16>(
            device,
            stream_config,
            channels,
            sender,
            recycle,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::I24 => build_typed_input_stream::<cpal::I24>(
            device,
            stream_config,
            channels,
            sender,
            recycle,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::I32 => build_typed_input_stream::<i32>(
            device,
            stream_config,
            channels,
            sender,
            recycle,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::I64 => build_typed_input_stream::<i64>(
            device,
            stream_config,
            channels,
            sender,
            recycle,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::U8 => build_typed_input_stream::<u8>(
            device,
            stream_config,
            channels,
            sender,
            recycle,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::U16 => build_typed_input_stream::<u16>(
            device,
            stream_config,
            channels,
            sender,
            recycle,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::U24 => build_typed_input_stream::<cpal::U24>(
            device,
            stream_config,
            channels,
            sender,
            recycle,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::U32 => build_typed_input_stream::<u32>(
            device,
            stream_config,
            channels,
            sender,
            recycle,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::U64 => build_typed_input_stream::<u64>(
            device,
            stream_config,
            channels,
            sender,
            recycle,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::F32 => build_typed_input_stream::<f32>(
            device,
            stream_config,
            channels,
            sender,
            recycle,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::F64 => build_typed_input_stream::<f64>(
            device,
            stream_config,
            channels,
            sender,
            recycle,
            stats,
            callback_buffer_observer,
        ),
        _ => Err(format!("unsupported sample format: {sample_format}")),
    }
}

fn build_typed_input_stream<T>(
    device: &cpal::Device,
    stream_config: StreamConfig,
    channels: CallbackChannelCount,
    sender: SyncSender<Vec<f32>>,
    recycle: Receiver<Vec<f32>>,
    stats: AudioStats,
    callback_buffer_observer: Option<Arc<AudioCallbackBufferObserver>>,
) -> Result<Stream, String>
where
    T: Sample + cpal::SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let data_stats = stats.clone();
    let error_stats = stats.clone();
    let _ = audio_pop_logging_enabled();
    let _ = audio_callback_logging_enabled();
    device
        .build_input_stream(
            stream_config,
            move |input: &[T], _| {
                if let Some(observer) = callback_buffer_observer.as_ref() {
                    observer.observe(input.len(), channels);
                }
                let mono = recycle.try_recv().unwrap_or_default();
                capture_callback(input, channels, mono, &sender, &data_stats);
            },
            move |error| {
                if error.kind() == ErrorKind::RealtimeDenied && audio_callback_logging_enabled() {
                    let error_message = error.to_string();
                    kvlog::warn!(
                        "audio realtime priority denied",
                        direction = "capture",
                        error = error_message.as_str(),
                        hint = "grant rtprio or build with audio-realtime-dbus on rtkit systems"
                    );
                }
                error_stats.record_stream_error(
                    AudioErrorKind::from_cpal(error.kind()),
                    format!("stream error: {error}"),
                );
            },
            None,
        )
        .map_err(|error| format!("failed to build input stream: {error}"))
}

pub(crate) fn build_output_stream(
    device: &cpal::Device,
    sample_format: SampleFormat,
    stream_config: StreamConfig,
    channels: usize,
    samples: Arc<Vec<i16>>,
    stats: PlaybackStats,
) -> Result<Stream, String> {
    let channels = CallbackChannelCount::new(channels, "output")?;
    match sample_format {
        SampleFormat::I8 => {
            build_typed_output_stream::<i8>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::I16 => {
            build_typed_output_stream::<i16>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::I24 => {
            build_typed_output_stream::<cpal::I24>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::I32 => {
            build_typed_output_stream::<i32>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::I64 => {
            build_typed_output_stream::<i64>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::U8 => {
            build_typed_output_stream::<u8>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::U16 => {
            build_typed_output_stream::<u16>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::U24 => {
            build_typed_output_stream::<cpal::U24>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::U32 => {
            build_typed_output_stream::<u32>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::U64 => {
            build_typed_output_stream::<u64>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::F32 => {
            build_typed_output_stream::<f32>(device, stream_config, channels, samples, stats)
        }
        SampleFormat::F64 => {
            build_typed_output_stream::<f64>(device, stream_config, channels, samples, stats)
        }
        _ => Err(format!("unsupported output sample format: {sample_format}")),
    }
}

pub(crate) fn build_live_output_stream(
    device: &cpal::Device,
    sample_format: SampleFormat,
    stream_config: StreamConfig,
    channels: usize,
    mixer: LivePlaybackMixer,
    mixer_events: Arc<SpscSwapQueue<LivePlaybackMixerEvent>>,
    shared_snapshot: Arc<LivePlaybackSharedSnapshot>,
    echo_control: Option<Arc<EchoCancellationControl>>,
    callback_buffer_observer: Option<Arc<AudioCallbackBufferObserver>>,
    device_rate: u32,
    playback_recorder: Option<LivePlaybackWavRecorderHandle>,
) -> Result<Stream, String> {
    let channels = CallbackChannelCount::new(channels, "live output")?;
    match sample_format {
        SampleFormat::I8 => build_typed_live_output_stream::<i8>(
            device,
            stream_config,
            channels,
            mixer,
            mixer_events,
            shared_snapshot,
            echo_control,
            callback_buffer_observer,
            device_rate,
            playback_recorder,
        ),
        SampleFormat::I16 => build_typed_live_output_stream::<i16>(
            device,
            stream_config,
            channels,
            mixer,
            mixer_events,
            shared_snapshot,
            echo_control,
            callback_buffer_observer,
            device_rate,
            playback_recorder,
        ),
        SampleFormat::I24 => build_typed_live_output_stream::<cpal::I24>(
            device,
            stream_config,
            channels,
            mixer,
            mixer_events,
            shared_snapshot,
            echo_control,
            callback_buffer_observer,
            device_rate,
            playback_recorder,
        ),
        SampleFormat::I32 => build_typed_live_output_stream::<i32>(
            device,
            stream_config,
            channels,
            mixer,
            mixer_events,
            shared_snapshot,
            echo_control,
            callback_buffer_observer,
            device_rate,
            playback_recorder,
        ),
        SampleFormat::I64 => build_typed_live_output_stream::<i64>(
            device,
            stream_config,
            channels,
            mixer,
            mixer_events,
            shared_snapshot,
            echo_control,
            callback_buffer_observer,
            device_rate,
            playback_recorder,
        ),
        SampleFormat::U8 => build_typed_live_output_stream::<u8>(
            device,
            stream_config,
            channels,
            mixer,
            mixer_events,
            shared_snapshot,
            echo_control,
            callback_buffer_observer,
            device_rate,
            playback_recorder,
        ),
        SampleFormat::U16 => build_typed_live_output_stream::<u16>(
            device,
            stream_config,
            channels,
            mixer,
            mixer_events,
            shared_snapshot,
            echo_control,
            callback_buffer_observer,
            device_rate,
            playback_recorder,
        ),
        SampleFormat::U24 => build_typed_live_output_stream::<cpal::U24>(
            device,
            stream_config,
            channels,
            mixer,
            mixer_events,
            shared_snapshot,
            echo_control,
            callback_buffer_observer,
            device_rate,
            playback_recorder,
        ),
        SampleFormat::U32 => build_typed_live_output_stream::<u32>(
            device,
            stream_config,
            channels,
            mixer,
            mixer_events,
            shared_snapshot,
            echo_control,
            callback_buffer_observer,
            device_rate,
            playback_recorder,
        ),
        SampleFormat::U64 => build_typed_live_output_stream::<u64>(
            device,
            stream_config,
            channels,
            mixer,
            mixer_events,
            shared_snapshot,
            echo_control,
            callback_buffer_observer,
            device_rate,
            playback_recorder,
        ),
        SampleFormat::F32 => build_typed_live_output_stream::<f32>(
            device,
            stream_config,
            channels,
            mixer,
            mixer_events,
            shared_snapshot,
            echo_control,
            callback_buffer_observer,
            device_rate,
            playback_recorder,
        ),
        SampleFormat::F64 => build_typed_live_output_stream::<f64>(
            device,
            stream_config,
            channels,
            mixer,
            mixer_events,
            shared_snapshot,
            echo_control,
            callback_buffer_observer,
            device_rate,
            playback_recorder,
        ),
        _ => Err(format!("unsupported output sample format: {sample_format}")),
    }
}

fn build_typed_live_output_stream<T>(
    device: &cpal::Device,
    stream_config: StreamConfig,
    channels: CallbackChannelCount,
    mut mixer: LivePlaybackMixer,
    mixer_events: Arc<SpscSwapQueue<LivePlaybackMixerEvent>>,
    shared_snapshot: Arc<LivePlaybackSharedSnapshot>,
    echo_control: Option<Arc<EchoCancellationControl>>,
    callback_buffer_observer: Option<Arc<AudioCallbackBufferObserver>>,
    device_rate: u32,
    playback_recorder: Option<LivePlaybackWavRecorderHandle>,
) -> Result<Stream, String>
where
    T: Sample + cpal::SizedSample + FromSample<f32> + Send + 'static,
{
    let error_snapshot = Arc::clone(&shared_snapshot);
    // The worker now publishes the stream snapshot, so the callback never
    // touches `shared_snapshot` except on a backend error.
    let _ = shared_snapshot;
    let _ = audio_pop_logging_enabled();
    let _ = audio_callback_logging_enabled();
    let mut resampler = PlaybackResampler::new(device_rate);
    let mut pending_event = LivePlaybackMixerEvent::default();
    let mut mix_adapter = LivePlaybackMixAdapter::new();
    let mut playback_record_block = Vec::with_capacity(SAMPLE_RATE as usize);
    device
        .build_output_stream(
            stream_config,
            move |output: &mut [T], _| {
                if let Some(observer) = callback_buffer_observer.as_ref() {
                    observer.observe(output.len(), channels);
                }
                let now = Instant::now();
                let drained_events =
                    drain_live_playback_mixer_events(&mut mixer, &mixer_events, &mut pending_event);
                live_playback_callback(
                    output,
                    channels,
                    &mut mixer,
                    echo_control.as_ref(),
                    playback_recorder.as_ref(),
                    &mut playback_record_block,
                    &mut mix_adapter,
                    resampler.as_mut(),
                    device_rate,
                    now,
                );
                mixer.note_callback_metrics(
                    now.elapsed(),
                    callback_period(channels.frames_for_interleaved(output.len()), device_rate),
                    drained_events,
                );
            },
            move |error| {
                record_live_playback_stream_error(&error_snapshot, error);
            },
            None,
        )
        .map_err(|error| format!("failed to build live output stream: {error}"))
}

fn record_live_playback_stream_error(
    shared_snapshot: &Arc<LivePlaybackSharedSnapshot>,
    error: cpal::Error,
) {
    let kind = AudioErrorKind::from_cpal(error.kind());
    let error = error.to_string();
    if kind == AudioErrorKind::RealtimeDenied && audio_callback_logging_enabled() {
        kvlog::warn!(
            "audio realtime priority denied",
            direction = "live playback",
            error = error.as_str(),
            hint = "grant rtprio or build with audio-realtime-dbus on rtkit systems"
        );
    }
    shared_snapshot.record_backend_stream_error(error, kind, Instant::now());
}

pub(crate) fn drain_live_playback_mixer_events(
    mixer: &mut LivePlaybackMixer,
    mixer_events: &SpscSwapQueue<LivePlaybackMixerEvent>,
    pending_event: &mut LivePlaybackMixerEvent,
) -> u64 {
    let mut drained = 0u64;
    while mixer_events.remove(pending_event) {
        drained = drained.saturating_add(1);
        match mem::take(pending_event) {
            LivePlaybackMixerEvent::Empty => {}
            LivePlaybackMixerEvent::EnsureStream { stream_id, source } => {
                mixer.ensure_stream(stream_id, source);
            }
            LivePlaybackMixerEvent::StopStream { stream_id } => {
                mixer.apply_stop_stream_event(stream_id);
            }
            LivePlaybackMixerEvent::SetStreamControl { stream_id, control } => {
                mixer.set_stream_control(stream_id, control);
            }
        }
    }
    drained
}

pub(crate) struct LivePlaybackCallbackBench {
    channels: CallbackChannelCount,
    output: Vec<f32>,
    playback_record_block: Vec<f32>,
    mix_adapter: LivePlaybackMixAdapter,
    device_rate: u32,
}

impl LivePlaybackCallbackBench {
    pub(crate) fn new_48khz_f32_stereo(frames: usize) -> Self {
        let channels = CallbackChannelCount::new(2, "live playback benchmark")
            .expect("stereo benchmark channel count should be non-zero");
        Self {
            channels,
            output: vec![0.0; frames.saturating_mul(channels.get())],
            playback_record_block: Vec::new(),
            mix_adapter: LivePlaybackMixAdapter::new(),
            device_rate: SAMPLE_RATE,
        }
    }

    pub(crate) fn run(&mut self, mixer: &mut LivePlaybackMixer, now: Instant) -> f64 {
        live_playback_callback::<f32>(
            &mut self.output,
            self.channels,
            mixer,
            None,
            None,
            &mut self.playback_record_block,
            &mut self.mix_adapter,
            None,
            self.device_rate,
            now,
        );
        self.output
            .iter()
            .enumerate()
            .map(|(index, sample)| f64::from(*sample) * ((index % 251 + 1) as f64))
            .sum()
    }

    pub(crate) fn output_frames(&self) -> usize {
        self.channels.frames_for_interleaved(self.output.len())
    }

    pub(crate) fn output(&self) -> &[f32] {
        &self.output
    }
}

fn callback_period(frames: usize, device_rate: u32) -> Duration {
    let nanos = frames as u128 * 1_000_000_000u128 / u128::from(device_rate.max(1));
    Duration::from_nanos(nanos.min(u128::from(u64::MAX)) as u64)
}

struct LivePlaybackMixAdapter {
    carry: [f32; MIX_FRAME_SAMPLES],
    carry_cursor: usize,
    carry_len: usize,
    callback_source_cursor: usize,
}

impl LivePlaybackMixAdapter {
    fn new() -> Self {
        Self {
            carry: [0.0; MIX_FRAME_SAMPLES],
            carry_cursor: MIX_FRAME_SAMPLES,
            carry_len: MIX_FRAME_SAMPLES,
            callback_source_cursor: 0,
        }
    }

    fn begin_callback(&mut self) {
        self.callback_source_cursor = 0;
    }

    fn block_time(&self, callback_start: Instant) -> Instant {
        callback_start + samples_to_duration(self.callback_source_cursor)
    }

    #[cfg(test)]
    fn fill(&mut self, mixer: &mut LivePlaybackMixer, callback_start: Instant, out: &mut [f32]) {
        self.fill_from(callback_start, out, |now, carry| mixer.mix_10ms(now, carry));
    }

    #[cfg(test)]
    fn fill_from(
        &mut self,
        callback_start: Instant,
        out: &mut [f32],
        mut render: impl FnMut(Instant, &mut [f32; MIX_FRAME_SAMPLES]),
    ) {
        let mut written = 0;
        while written < out.len() {
            if self.carry_cursor >= self.carry_len {
                render(self.block_time(callback_start), &mut self.carry);
                self.carry_cursor = 0;
                self.carry_len = MIX_FRAME_SAMPLES;
            }

            let available = self.carry_len - self.carry_cursor;
            let count = available.min(out.len() - written);
            out[written..written + count]
                .copy_from_slice(&self.carry[self.carry_cursor..self.carry_cursor + count]);
            self.carry_cursor += count;
            self.callback_source_cursor = self.callback_source_cursor.saturating_add(count);
            written += count;
        }
    }

    fn next_sample(&mut self, mixer: &mut LivePlaybackMixer, callback_start: Instant) -> f32 {
        self.next_sample_from(callback_start, |now, carry| mixer.mix_10ms(now, carry))
    }

    fn next_sample_from(
        &mut self,
        callback_start: Instant,
        mut render: impl FnMut(Instant, &mut [f32; MIX_FRAME_SAMPLES]),
    ) -> f32 {
        if self.carry_cursor >= self.carry_len {
            render(self.block_time(callback_start), &mut self.carry);
            self.carry_cursor = 0;
            self.carry_len = MIX_FRAME_SAMPLES;
        }
        let sample = self.carry[self.carry_cursor];
        self.carry_cursor += 1;
        self.callback_source_cursor = self.callback_source_cursor.saturating_add(1);
        sample
    }

    fn staged_samples(&self) -> usize {
        self.carry_len.saturating_sub(self.carry_cursor)
    }
}

fn build_typed_output_stream<T>(
    device: &cpal::Device,
    stream_config: StreamConfig,
    channels: CallbackChannelCount,
    samples: Arc<Vec<i16>>,
    stats: PlaybackStats,
) -> Result<Stream, String>
where
    T: Sample + cpal::SizedSample + FromSample<f32> + Send + 'static,
{
    let data_stats = stats.clone();
    let error_stats = stats.clone();
    let mut cursor = 0usize;
    let _ = audio_callback_logging_enabled();
    device
        .build_output_stream(
            stream_config,
            move |output: &mut [T], _| {
                playback_callback(output, channels, &samples, &mut cursor, &data_stats);
            },
            move |error| {
                if error.kind() == ErrorKind::RealtimeDenied && audio_callback_logging_enabled() {
                    let error_message = error.to_string();
                    kvlog::warn!(
                        "audio realtime priority denied",
                        direction = "playback",
                        error = error_message.as_str(),
                        hint = "grant rtprio or build with audio-realtime-dbus on rtkit systems"
                    );
                }
                error_stats.record_stream_error(format!("playback stream error: {error}"));
            },
            None,
        )
        .map_err(|error| format!("failed to build output stream: {error}"))
}

fn playback_callback<T>(
    output: &mut [T],
    channels: CallbackChannelCount,
    samples: &[i16],
    cursor: &mut usize,
    stats: &PlaybackStats,
) where
    T: Sample + FromSample<f32>,
{
    stats.record_callback();

    for frame in output.chunks_mut(channels.get()) {
        let sample = samples.get(*cursor).copied().unwrap_or(0);
        if *cursor < samples.len() {
            *cursor += 1;
        } else {
            stats.mark_finished();
        }

        let output_sample = T::from_sample((sample as f32 / 32768.0).clamp(-1.0, 1.0));
        for channel in frame {
            *channel = output_sample;
        }
    }

    if *cursor >= samples.len() {
        stats.mark_finished();
    }
    stats.store_played_samples(*cursor);
}

fn live_playback_callback<T>(
    output: &mut [T],
    channels: CallbackChannelCount,
    mixer: &mut LivePlaybackMixer,
    echo_control: Option<&Arc<EchoCancellationControl>>,
    playback_recorder: Option<&LivePlaybackWavRecorderHandle>,
    playback_record_block: &mut Vec<f32>,
    mix_adapter: &mut LivePlaybackMixAdapter,
    mut resampler: Option<&mut PlaybackResampler>,
    device_rate: u32,
    now: Instant,
) where
    T: Sample + FromSample<f32>,
{
    let output_frames = channels.frames_for_interleaved(output.len());
    mix_adapter.begin_callback();
    let mut echo_writer = match echo_control {
        Some(control) if control.enabled() => Some(control.reference().writer()),
        _ => None,
    };
    match resampler.as_mut() {
        // Non-48 kHz device: pull 48 kHz blocks from the mixer, resample each to
        // the device rate. The echo reference still receives the pre-resample
        // 48 kHz samples, so AEC keeps working against a 48 kHz render signal.
        Some(resampler) => {
            let source_block = resampler.source_block_samples(output_frames);
            mixer.note_device_callback_frames(source_block);
            let mut device_frame_index = 0usize;
            for frame in output.chunks_mut(channels.get()) {
                let sample = resampler.next_sample(|block| {
                    let block_start = now + callback_period(device_frame_index, device_rate);
                    let block: &mut [f32; MIX_FRAME_SAMPLES] = block
                        .try_into()
                        .expect("playback resampler source block must be one mixer frame");
                    mixer.mix_10ms(block_start, block);
                    for &mixed in block.iter() {
                        if let Some(writer) = echo_writer.as_mut() {
                            writer.push(mixed);
                        }
                    }
                    if let Some(recorder) = playback_recorder {
                        recorder.record_samples(block);
                    }
                });
                let output_sample = T::from_sample(sample.clamp(-1.0, 1.0));
                for channel in frame {
                    *channel = output_sample;
                }
                device_frame_index += 1;
            }
        }
        None => {
            mixer.note_device_callback_frames(output_frames);
            if playback_recorder.is_some() {
                playback_record_block.resize(output_frames, 0.0);
            }
            for (index, frame) in output.chunks_mut(channels.get()).enumerate() {
                let sample = mix_adapter.next_sample(mixer, now);
                if let Some(writer) = echo_writer.as_mut() {
                    writer.push(sample);
                }
                if !playback_record_block.is_empty() {
                    playback_record_block[index] = sample;
                }
                let output_sample = T::from_sample(sample.clamp(-1.0, 1.0));
                for channel in frame {
                    *channel = output_sample;
                }
            }
            if let Some(recorder) = playback_recorder {
                recorder.record_samples(playback_record_block);
            }
        }
    }
    if let Some(writer) = echo_writer {
        writer.commit();
    }
    mixer.note_staged_samples(mix_adapter.staged_samples());
}

fn capture_callback<T>(
    input: &[T],
    channels: CallbackChannelCount,
    mut mono: Vec<f32>,
    sender: &SyncSender<Vec<f32>>,
    stats: &AudioStats,
) where
    T: Sample,
    f32: FromSample<T>,
{
    downmix_to_mono_i16_scale_into(input, channels.get(), &mut mono);
    let samples = mono.len() as u64;
    let rms = rms_i16_scale(&mono);
    let peak = peak_i16_scale(&mono);
    stats.record_capture_callback(samples, rms, peak);

    if sender.try_send(mono).is_err() {
        // The encoder worker is behind, so this chunk is lost. Surface the
        // backpressure (throttled to powers of two so a sustained overload does
        // not flood the log) instead of dropping it silently, and account the
        // dropped duration so the worker leaves a concealable timestamp gap
        // rather than splicing the media clock across the hole.
        let dropped = stats.record_dropped_chunk(samples);
        if dropped.is_power_of_two() && audio_callback_logging_enabled() {
            kvlog::warn!(
                "capture worker backpressure dropped chunk",
                dropped_chunks = dropped
            );
        }
    }
}

fn downmix_to_mono_i16_scale_into<T>(input: &[T], channels: usize, out: &mut Vec<f32>)
where
    T: Sample,
    f32: FromSample<T>,
{
    out.clear();
    if channels == 0 {
        return;
    }

    out.reserve(input.len() / channels);
    for frame in input.chunks_exact(channels) {
        let mut sum = 0.0f32;
        for sample in frame {
            sum += sample.to_sample::<f32>() * i16::MAX as f32;
        }
        out.push(sum / channels as f32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::playback::MixerStreamSource;
    #[allow(unused_imports)]
    use crate::audio::test_support::*;
    use cpal::{SupportedBufferSize, SupportedStreamConfigRange};

    fn range(min_rate: u32, max_rate: u32) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(
            1,
            min_rate,
            max_rate,
            SupportedBufferSize::Unknown,
            SampleFormat::F32,
        )
    }

    #[test]
    fn range_stream_rate_prefers_native_48k() {
        // A range that covers 48 kHz always resolves to 48 kHz, never a fallback,
        // so a capable device never resamples.
        assert_eq!(range_stream_rate(&range(44_100, 96_000)), Some(SAMPLE_RATE));
        assert_eq!(rate_rank(SAMPLE_RATE), 0);
    }

    #[test]
    fn range_stream_rate_falls_back_to_best_supported_rate() {
        // 44.1 kHz-only hardware resolves to its supported fallback rate, ranked
        // after native 48 kHz so it loses to any 48 kHz-capable range.
        assert_eq!(range_stream_rate(&range(44_100, 44_100)), Some(44_100));
        assert!(rate_rank(44_100) > rate_rank(SAMPLE_RATE));
        // A range covering no usable rate is rejected.
        assert_eq!(range_stream_rate(&range(22_050, 22_050)), None);
    }

    #[test]
    fn zero_quantum_range_keeps_requested_buffer() {
        // The PipeWire host reports Range { 0, 0 } before clock metadata
        // arrives; the request must pass through instead of clamping to zero.
        let (size, note) = select_buffer_size(
            BufferRequest::Fixed(480),
            SupportedBufferSize::Range { min: 0, max: 0 },
        );
        assert_eq!(size, cpal::BufferSize::Fixed(480));
        assert!(note.contains("quantum range unknown"));

        let (size, _) = select_buffer_size(
            BufferRequest::Fixed(480),
            SupportedBufferSize::Range { min: 32, max: 8192 },
        );
        assert_eq!(size, cpal::BufferSize::Fixed(480));
    }

    #[test]
    fn parses_proc_asound_pcm_for_requested_direction() {
        let content = "\
00-03: HDMI 0 : HDMI 0 : playback 1
01-00: USB Audio : USB Audio : playback 1 : capture 1
02-00: ALC897 Analog : ALC897 Analog : capture 1
";

        assert_eq!(
            parse_alsa_physical_pcm_devices(content, AudioDeviceDirection::Output),
            vec![
                AlsaPhysicalPcm {
                    card: 0,
                    device: 3,
                    name: "HDMI 0".to_string(),
                },
                AlsaPhysicalPcm {
                    card: 1,
                    device: 0,
                    name: "USB Audio".to_string(),
                },
            ]
        );
        assert_eq!(
            parse_alsa_physical_pcm_devices(content, AudioDeviceDirection::Input),
            vec![
                AlsaPhysicalPcm {
                    card: 1,
                    device: 0,
                    name: "USB Audio".to_string(),
                },
                AlsaPhysicalPcm {
                    card: 2,
                    device: 0,
                    name: "ALC897 Analog".to_string(),
                },
            ]
        );
    }

    #[test]
    fn recognizes_bare_alsa_pcm_names() {
        assert!(looks_like_alsa_pcm_name("surround2"));
        assert!(looks_like_alsa_pcm_name("hw:0,0"));
        assert!(looks_like_alsa_pcm_name("plughw:CARD=PCH,DEV=0"));
        assert_eq!(
            parse_configured_device_id("alsa/hw:0,0")
                .map(|id| id.to_string())
                .as_deref(),
            Some("alsa:hw:0,0")
        );
        assert_eq!(
            parse_configured_device_id("alsa/my_custom_pcm")
                .map(|id| id.to_string())
                .as_deref(),
            Some("alsa:my_custom_pcm")
        );
        assert_eq!(parse_configured_device_id("my_custom_pcm"), None);
        assert!(!looks_like_alsa_pcm_name("usb microphone"));
        assert!(!looks_like_alsa_pcm_name(""));
    }

    #[test]
    fn pipewire_picker_filter_keeps_endpoints_in_matching_direction() {
        assert!(pipewire_device_id_matches_picker_direction(
            "alsa_input.usb-DCMT_Technology_USB_Condenser_Microphone_214b206000000178-00.mono-fallback",
            AudioDeviceDirection::Input,
        ));
        assert!(!pipewire_device_id_matches_picker_direction(
            "alsa_input.usb-DCMT_Technology_USB_Condenser_Microphone_214b206000000178-00.mono-fallback",
            AudioDeviceDirection::Output,
        ));
        assert!(pipewire_device_id_matches_picker_direction(
            "alsa_output.usb-BEHRINGER_UMC204HD_192k-00.pro-output-0",
            AudioDeviceDirection::Output,
        ));
        assert!(!pipewire_device_id_matches_picker_direction(
            "alsa_output.usb-BEHRINGER_UMC204HD_192k-00.pro-output-0",
            AudioDeviceDirection::Input,
        ));
        assert!(pipewire_device_id_matches_picker_direction(
            "bluez_output.20_F4_D4_61_20_AD.1",
            AudioDeviceDirection::Output,
        ));
        assert!(!pipewire_device_id_matches_picker_direction(
            "bluez_output.20_F4_D4_61_20_AD.1",
            AudioDeviceDirection::Input,
        ));
    }

    #[test]
    fn pipewire_picker_filter_hides_defaults_and_client_streams() {
        for node_name in [
            "sink_default",
            "input_default",
            "output_default",
            "alsa_capture.chatt",
            "alsa_playback.chatt",
            "Mumble",
            "chatt",
        ] {
            assert!(
                !pipewire_device_id_matches_picker_direction(
                    node_name,
                    AudioDeviceDirection::Input
                ),
                "{node_name} should not be listed as an input endpoint"
            );
            assert!(
                !pipewire_device_id_matches_picker_direction(
                    node_name,
                    AudioDeviceDirection::Output
                ),
                "{node_name} should not be listed as an output endpoint"
            );
        }
    }

    #[test]
    fn dropped_capture_chunk_records_dropped_samples() {
        let (sender, _receiver) = std::sync::mpsc::sync_channel::<Vec<f32>>(1);
        sender.try_send(vec![0.0]).unwrap();
        let stats = AudioStats::new();
        let channels = CallbackChannelCount::new(2, "input").unwrap();

        // The channel is full, so this stereo chunk (48 mono frames) is dropped
        // and its sample count must be surfaced for the media clock.
        capture_callback(&[0.1f32; 96], channels, Vec::new(), &sender, &stats);

        assert_eq!(stats.take_dropped_capture_samples(), 48);
        assert_eq!(
            stats.take_dropped_capture_samples(),
            0,
            "taking the dropped samples must drain the counter"
        );
        assert_eq!(stats.snapshot().dropped_chunks, 1);
    }

    #[test]
    fn downmixes_interleaved_samples_to_mono_i16_scale() {
        let mut mono = Vec::new();
        downmix_to_mono_i16_scale_into(&[0.5f32, -0.5, 0.25, 0.75], 2, &mut mono);

        assert_eq!(mono.len(), 2);
        assert!(mono[0].abs() < 0.01);
        assert!((mono[1] - 0.5 * i16::MAX as f32).abs() < 1.0);

        // Mono input passes each sample through at i16 scale.
        let mut mono_in = Vec::new();
        downmix_to_mono_i16_scale_into(&[0.25f32, -0.5], 1, &mut mono_in);
        assert_eq!(mono_in.len(), 2);
        assert!((mono_in[0] - 0.25 * i16::MAX as f32).abs() < 1.0);
        assert!((mono_in[1] + 0.5 * i16::MAX as f32).abs() < 1.0);

        // Reusing the same buffer for an equal-size fill grows no capacity, so the
        // recycled callback path allocates nothing after the first fill.
        let capacity_before = mono.capacity();
        downmix_to_mono_i16_scale_into(&[0.1f32, 0.2, 0.3, 0.4], 2, &mut mono);
        assert_eq!(mono.len(), 2);
        assert_eq!(mono.capacity(), capacity_before);
    }

    fn sample_ring_with(samples: &[f32]) -> Arc<crate::audio::playback::SampleRing> {
        let ring = Arc::new(crate::audio::playback::SampleRing::with_capacity(
            samples.len().max(1) * 2,
        ));
        ring.write_samples(samples);
        ring
    }

    #[test]
    fn adapter_serves_arbitrary_callback_sizes_deterministically() {
        let callback_sizes = [137, 480, 843, 33, 511, 960, 17];
        let total = callback_sizes.iter().sum::<usize>();
        let samples: Vec<f32> = (0..total + MIX_FRAME_SAMPLES * 2)
            .map(|index| (index % 97) as f32 / 1000.0)
            .collect();

        let mut reference_mixer = LivePlaybackMixer::new();
        reference_mixer.ensure_stream(1, sample_ring_with(&samples));
        let mut reference = Vec::new();
        while reference.len() < total {
            let mut frame = [0.0; MIX_FRAME_SAMPLES];
            reference_mixer.mix_10ms(Instant::now(), &mut frame);
            reference.extend_from_slice(&frame);
        }

        let mut mixer = LivePlaybackMixer::new();
        mixer.ensure_stream(1, sample_ring_with(&samples));
        let mut adapter = LivePlaybackMixAdapter::new();
        let mut served = Vec::new();
        for callback_frames in callback_sizes {
            let start = served.len();
            served.resize(start + callback_frames, 0.0);
            adapter.fill(
                &mut mixer,
                Instant::now(),
                &mut served[start..start + callback_frames],
            );
        }

        assert_eq!(served.len(), total);
        for (index, (&served, &expected)) in served.iter().zip(reference.iter()).enumerate() {
            assert!(
                (served - expected).abs() < 1e-6,
                "sample {index}: served {served}, expected {expected}"
            );
        }
    }

    #[test]
    fn adapter_timestamps_each_refill_at_its_callback_source_offset() {
        let start = Instant::now();
        let mut adapter = LivePlaybackMixAdapter::new();
        let mut out = vec![0.0; 1_200];
        let mut refills = Vec::new();

        adapter.begin_callback();
        adapter.fill_from(start, &mut out, |now, block| {
            refills.push(now);
            block.fill(0.0);
        });

        assert_eq!(refills.len(), 3);
        assert_eq!(refills[0], start);
        assert_eq!(refills[1], start + Duration::from_millis(10));
        assert_eq!(refills[2], start + Duration::from_millis(20));

        let next = start + Duration::from_millis(100);
        refills.clear();
        let mut out = vec![0.0; 200];
        adapter.begin_callback();
        adapter.fill_from(next, &mut out, |now, block| {
            refills.push(now);
            block.fill(0.0);
        });
        assert!(
            refills.is_empty(),
            "callback should first drain carry from the previous refill"
        );

        let mut out = vec![0.0; 41];
        adapter.fill_from(next, &mut out, |now, block| {
            refills.push(now);
            block.fill(0.0);
        });
        assert_eq!(
            refills,
            vec![next + samples_to_duration(240)],
            "new block should be timestamped after the carried samples"
        );
    }

    #[test]
    fn adapter_next_sample_timestamps_refills_by_ten_ms() {
        let start = Instant::now();
        let mut adapter = LivePlaybackMixAdapter::new();
        let mut refills = Vec::new();

        adapter.begin_callback();
        for _ in 0..(MIX_FRAME_SAMPLES * 2 + 1) {
            let _ = adapter.next_sample_from(start, |now, block| {
                refills.push(now);
                block.fill(0.0);
            });
        }

        assert_eq!(refills.len(), 3);
        assert_eq!(refills[0], start);
        assert_eq!(refills[1], start + Duration::from_millis(10));
        assert_eq!(refills[2], start + Duration::from_millis(20));
    }

    #[test]
    fn live_mixer_events_register_ring_and_mix_from_it() {
        use std::sync::Arc;
        let mixer_events = SpscSwapQueue::<LivePlaybackMixerEvent>::with_capacity(4);
        let ring = Arc::new(crate::audio::playback::SampleRing::with_capacity(
            crate::audio::shared::FRAME_SAMPLES * 4,
        ));
        ring.write_samples(&vec![0.25; crate::audio::shared::FRAME_SAMPLES]);
        let mut event = LivePlaybackMixerEvent::EnsureStream {
            stream_id: 7,
            source: MixerStreamSource::Ring(Arc::clone(&ring)),
        };
        assert!(mixer_events.insert(&mut event));

        let mut mixer = LivePlaybackMixer::with_tuning(test_tuning());
        let mut pending_event = LivePlaybackMixerEvent::default();
        drain_live_playback_mixer_events(&mut mixer, &mixer_events, &mut pending_event);

        assert_eq!(mixer.active_streams(), 1);
        let mut out = vec![0.0; crate::audio::shared::FRAME_SAMPLES];
        mixer.fill_block(Instant::now(), &mut out);
        // Read past the declick ramp so the per-stream envelope is at unity.
        let steady = out[crate::audio::shared::FRAME_SAMPLES - 1];
        assert!((steady - 0.25).abs() < 1e-6, "mixed sample {steady}");
        assert_eq!(ring.depth(), 0, "consumer drained the ring");
    }
}
