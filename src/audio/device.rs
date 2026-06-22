use crate::audio::*;

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

pub(in crate::audio) fn stable_device_id(name: &str) -> String {
    let mut key = name.to_ascii_lowercase();
    for suffix in [", usb audio", ", loopback pcm"] {
        if let Some(stripped) = key.strip_suffix(suffix) {
            key = stripped.to_string();
        }
    }
    key.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(in crate::audio) fn input_devices_inner(
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

pub(in crate::audio) fn output_devices_inner(
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

pub(in crate::audio) fn input_device_info(
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

pub(in crate::audio) fn output_device_info(
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

pub(in crate::audio) fn device_matches_picker_direction(
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

pub(in crate::audio) fn pipewire_device_id_is_hidden_from_picker(node_name: &str) -> bool {
    let node_name = node_name.to_ascii_lowercase();
    matches!(
        node_name.as_str(),
        "sink_default" | "input_default" | "output_default"
    ) || node_name.starts_with("alsa_capture.")
        || node_name.starts_with("alsa_playback.")
}

pub(in crate::audio) fn pipewire_device_id_matches_picker_direction(
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

pub(in crate::audio) fn pipewire_description_matches_picker_direction(
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

pub(in crate::audio) fn cpal_device_id(device: &cpal::Device) -> Option<String> {
    device.id().ok().map(|id| id.to_string())
}

pub(in crate::audio) fn cpal_device_matches_config_id(
    device: &cpal::Device,
    configured_id: &str,
) -> bool {
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

pub(in crate::audio) fn cpal_device_from_config_id(
    host: &cpal::Host,
    configured_id: &str,
) -> Option<cpal::Device> {
    let id = parse_configured_device_id(configured_id)?;
    host.device_by_id(&id)
        .or_else(|| cpal::host_from_id(id.host()).ok()?.device_by_id(&id))
}

pub(in crate::audio) fn parse_configured_device_id(configured_id: &str) -> Option<cpal::DeviceId> {
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
pub(in crate::audio) fn forced_alsa_device_id_from_pcm_name(
    pcm_name: &str,
) -> Option<cpal::DeviceId> {
    (!pcm_name.is_empty()).then(|| cpal::DeviceId::new(cpal::HostId::Alsa, pcm_name))
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
)))]
pub(in crate::audio) fn forced_alsa_device_id_from_pcm_name(
    _pcm_name: &str,
) -> Option<cpal::DeviceId> {
    None
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
))]
pub(in crate::audio) fn alsa_device_id_from_pcm_name(pcm_name: &str) -> Option<cpal::DeviceId> {
    looks_like_alsa_pcm_name(pcm_name).then(|| cpal::DeviceId::new(cpal::HostId::Alsa, pcm_name))
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
)))]
pub(in crate::audio) fn alsa_device_id_from_pcm_name(_pcm_name: &str) -> Option<cpal::DeviceId> {
    None
}

pub(in crate::audio) fn looks_like_alsa_pcm_name(value: &str) -> bool {
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
pub(in crate::audio) enum AudioDeviceDirection {
    Input,
    Output,
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd"
))]
pub(in crate::audio) fn append_alsa_physical_devices(
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
pub(in crate::audio) fn append_alsa_physical_devices(
    _host: &cpal::Host,
    _direction: AudioDeviceDirection,
    _buffer_request: BufferRequest,
    _seen_ids: &mut HashSet<String>,
    _infos: &mut Vec<DeviceInfo>,
) {
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::audio) struct AlsaPhysicalPcm {
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
pub(in crate::audio) fn alsa_physical_pcm_devices(
    direction: AudioDeviceDirection,
) -> Vec<AlsaPhysicalPcm> {
    fs::read_to_string("/proc/asound/pcm")
        .map(|content| parse_alsa_physical_pcm_devices(&content, direction))
        .unwrap_or_default()
}

pub(in crate::audio) fn parse_alsa_physical_pcm_devices(
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

pub(in crate::audio) fn parse_alsa_pcm_address(address: &str) -> Option<(u32, u32)> {
    let (card, device) = address.split_once('-')?;
    Some((card.parse().ok()?, device.parse().ok()?))
}

pub(in crate::audio) fn select_input_device_by_id(
    host: &cpal::Host,
    id: &str,
    buffer_request: BufferRequest,
) -> Result<(cpal::Device, ConfigSelection), String> {
    let devices = host
        .input_devices()
        .map_err(|error| format!("failed to list input devices: {error}"))?;
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
        Err(format!(
            "selected input device `{id}` is present but unsupported: {}",
            first_error.unwrap_or_else(|| "no supported 48 kHz input config".to_string())
        ))
    } else if let Some(device) = cpal_device_from_config_id(host, id) {
        select_input_config(&device, buffer_request)
            .map(|selection| (device, selection))
            .map_err(|error| format!("configured input device `{id}` could not be opened: {error}"))
    } else {
        Err(format!("selected input device `{id}` is unavailable"))
    }
}

pub(in crate::audio) fn select_output_device_by_id(
    host: &cpal::Host,
    id: &str,
    buffer_request: BufferRequest,
) -> Result<(cpal::Device, ConfigSelection), String> {
    let devices = host
        .output_devices()
        .map_err(|error| format!("failed to list output devices: {error}"))?;
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
        Err(format!(
            "selected output device `{id}` is present but unsupported: {}",
            first_error.unwrap_or_else(|| "no supported 48 kHz output config".to_string())
        ))
    } else if let Some(device) = cpal_device_from_config_id(host, id) {
        select_output_config(&device, buffer_request)
            .map(|selection| (device, selection))
            .map_err(|error| {
                format!("configured output device `{id}` could not be opened: {error}")
            })
    } else {
        Err(format!("selected output device `{id}` is unavailable"))
    }
}

pub(in crate::audio) fn format_file_error(context: &str, path: &Path, error: io::Error) -> String {
    format!("{context} {}: {error}", path.display())
}

pub(in crate::audio) fn format_opus_error(context: &str, code: i32) -> String {
    format!("{context}: {} ({code})", opus_codec::strerror(code))
}

pub(in crate::audio) struct ConfigSelection {
    pub(in crate::audio) supported_config: SupportedStreamConfig,
    pub(in crate::audio) stream_config: StreamConfig,
    pub(in crate::audio) preview: StreamPreview,
}

pub(in crate::audio) fn select_input_config(
    device: &cpal::Device,
    buffer_request: BufferRequest,
) -> Result<ConfigSelection, String> {
    let mut candidates = Vec::new();
    let ranges = device
        .supported_input_configs()
        .map_err(|error| format!("failed to query input configs: {error}"))?;

    for range in ranges {
        if !range.contains_rate(SAMPLE_RATE) || range.sample_format().is_dsd() {
            continue;
        }
        let supported_config = range.with_sample_rate(SAMPLE_RATE);
        candidates.push((supported_config, *range.buffer_size()));
    }

    candidates.sort_by_key(|(config, _)| {
        (
            channel_rank(config.channels()),
            sample_format_rank(config.sample_format()),
        )
    });

    let (supported_config, supported_buffer_size) = candidates
        .into_iter()
        .next()
        .ok_or_else(|| "no 48 kHz PCM input config available".to_string())?;

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
        supported_config,
        stream_config,
    })
}

pub(in crate::audio) fn select_output_config(
    device: &cpal::Device,
    buffer_request: BufferRequest,
) -> Result<ConfigSelection, String> {
    let mut candidates = Vec::new();
    let ranges = device
        .supported_output_configs()
        .map_err(|error| format!("failed to query output configs: {error}"))?;

    for range in ranges {
        if !range.contains_rate(SAMPLE_RATE) || range.sample_format().is_dsd() {
            continue;
        }
        let supported_config = range.with_sample_rate(SAMPLE_RATE);
        candidates.push((supported_config, *range.buffer_size()));
    }

    candidates.sort_by_key(|(config, _)| {
        (
            output_channel_rank(config.channels()),
            sample_format_rank(config.sample_format()),
        )
    });

    let (supported_config, supported_buffer_size) = candidates
        .into_iter()
        .next()
        .ok_or_else(|| "no 48 kHz PCM output config available".to_string())?;

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
        supported_config,
        stream_config,
    })
}

pub(in crate::audio) fn channel_rank(channels: u16) -> u16 {
    match channels {
        1 => 0,
        2 => 1,
        other => other.saturating_add(2),
    }
}

pub(in crate::audio) fn output_channel_rank(channels: u16) -> u16 {
    match channels {
        2 => 0,
        1 => 1,
        other => other.saturating_add(2),
    }
}

pub(in crate::audio) fn sample_format_rank(format: SampleFormat) -> u8 {
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

pub(in crate::audio) fn select_buffer_size(
    request: BufferRequest,
    supported: SupportedBufferSize,
) -> (BufferSize, String) {
    match request {
        BufferRequest::Default => (BufferSize::Default, "host default".to_string()),
        BufferRequest::Fixed(requested) => match supported {
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

pub(in crate::audio) fn audio_buffer_size_label(buffer_size: BufferSize) -> String {
    match buffer_size {
        BufferSize::Default => "default".to_string(),
        BufferSize::Fixed(frames) => format!("{frames} frames"),
    }
}

pub(in crate::audio) struct AudioCallbackBufferObserver {
    direction: &'static str,
    last_frames: AtomicUsize,
}

impl AudioCallbackBufferObserver {
    pub(in crate::audio) fn new(direction: &'static str) -> Self {
        Self {
            direction,
            last_frames: AtomicUsize::new(usize::MAX),
        }
    }

    fn observe(&self, interleaved_samples: usize, channels: usize) {
        let channels = channels.max(1);
        let frames = interleaved_samples / channels;
        let previous = self.last_frames.swap(frames, Ordering::Relaxed);
        if previous == frames {
            return;
        }

        kvlog::info!(
            "live audio callback buffer observed",
            direction = self.direction,
            observed_buffer_frames = frames,
            observed_buffer_ms = frames as f64 * 1000.0 / SAMPLE_RATE as f64,
            channels = channels,
            interleaved_samples = interleaved_samples,
            changed = previous != usize::MAX
        );
    }
}

pub(in crate::audio) fn build_input_stream(
    device: &cpal::Device,
    sample_format: SampleFormat,
    stream_config: StreamConfig,
    channels: usize,
    sender: SyncSender<Vec<f32>>,
    stats: AudioStats,
    callback_buffer_observer: Option<Arc<AudioCallbackBufferObserver>>,
) -> Result<Stream, String> {
    match sample_format {
        SampleFormat::I8 => build_typed_input_stream::<i8>(
            device,
            stream_config,
            channels,
            sender,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::I16 => build_typed_input_stream::<i16>(
            device,
            stream_config,
            channels,
            sender,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::I24 => build_typed_input_stream::<cpal::I24>(
            device,
            stream_config,
            channels,
            sender,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::I32 => build_typed_input_stream::<i32>(
            device,
            stream_config,
            channels,
            sender,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::I64 => build_typed_input_stream::<i64>(
            device,
            stream_config,
            channels,
            sender,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::U8 => build_typed_input_stream::<u8>(
            device,
            stream_config,
            channels,
            sender,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::U16 => build_typed_input_stream::<u16>(
            device,
            stream_config,
            channels,
            sender,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::U24 => build_typed_input_stream::<cpal::U24>(
            device,
            stream_config,
            channels,
            sender,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::U32 => build_typed_input_stream::<u32>(
            device,
            stream_config,
            channels,
            sender,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::U64 => build_typed_input_stream::<u64>(
            device,
            stream_config,
            channels,
            sender,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::F32 => build_typed_input_stream::<f32>(
            device,
            stream_config,
            channels,
            sender,
            stats,
            callback_buffer_observer,
        ),
        SampleFormat::F64 => build_typed_input_stream::<f64>(
            device,
            stream_config,
            channels,
            sender,
            stats,
            callback_buffer_observer,
        ),
        _ => Err(format!("unsupported sample format: {sample_format}")),
    }
}

pub(in crate::audio) fn build_typed_input_stream<T>(
    device: &cpal::Device,
    stream_config: StreamConfig,
    channels: usize,
    sender: SyncSender<Vec<f32>>,
    stats: AudioStats,
    callback_buffer_observer: Option<Arc<AudioCallbackBufferObserver>>,
) -> Result<Stream, String>
where
    T: Sample + cpal::SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let data_stats = stats.clone();
    let error_stats = stats.clone();
    device
        .build_input_stream(
            stream_config,
            move |input: &[T], _| {
                if let Some(observer) = callback_buffer_observer.as_ref() {
                    observer.observe(input.len(), channels);
                }
                capture_callback(input, channels, &sender, &data_stats);
            },
            move |error| {
                error_stats
                    .inner
                    .stream_errors
                    .fetch_add(1, Ordering::Relaxed);
                error_stats.set_error(format!("stream error: {error}"));
            },
            None,
        )
        .map_err(|error| format!("failed to build input stream: {error}"))
}

pub(in crate::audio) fn build_output_stream(
    device: &cpal::Device,
    sample_format: SampleFormat,
    stream_config: StreamConfig,
    channels: usize,
    samples: Arc<Vec<i16>>,
    stats: PlaybackStats,
) -> Result<Stream, String> {
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

pub(in crate::audio) fn build_live_output_stream(
    device: &cpal::Device,
    sample_format: SampleFormat,
    stream_config: StreamConfig,
    channels: usize,
    mixer: Arc<Mutex<LivePlaybackMixer>>,
    echo_control: Option<Arc<EchoCancellationControl>>,
    callback_buffer_observer: Option<Arc<AudioCallbackBufferObserver>>,
) -> Result<Stream, String> {
    match sample_format {
        SampleFormat::I8 => build_typed_live_output_stream::<i8>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
            callback_buffer_observer,
        ),
        SampleFormat::I16 => build_typed_live_output_stream::<i16>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
            callback_buffer_observer,
        ),
        SampleFormat::I24 => build_typed_live_output_stream::<cpal::I24>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
            callback_buffer_observer,
        ),
        SampleFormat::I32 => build_typed_live_output_stream::<i32>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
            callback_buffer_observer,
        ),
        SampleFormat::I64 => build_typed_live_output_stream::<i64>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
            callback_buffer_observer,
        ),
        SampleFormat::U8 => build_typed_live_output_stream::<u8>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
            callback_buffer_observer,
        ),
        SampleFormat::U16 => build_typed_live_output_stream::<u16>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
            callback_buffer_observer,
        ),
        SampleFormat::U24 => build_typed_live_output_stream::<cpal::U24>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
            callback_buffer_observer,
        ),
        SampleFormat::U32 => build_typed_live_output_stream::<u32>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
            callback_buffer_observer,
        ),
        SampleFormat::U64 => build_typed_live_output_stream::<u64>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
            callback_buffer_observer,
        ),
        SampleFormat::F32 => build_typed_live_output_stream::<f32>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
            callback_buffer_observer,
        ),
        SampleFormat::F64 => build_typed_live_output_stream::<f64>(
            device,
            stream_config,
            channels,
            mixer,
            echo_control,
            callback_buffer_observer,
        ),
        _ => Err(format!("unsupported output sample format: {sample_format}")),
    }
}

pub(in crate::audio) fn build_typed_live_output_stream<T>(
    device: &cpal::Device,
    stream_config: StreamConfig,
    channels: usize,
    mixer: Arc<Mutex<LivePlaybackMixer>>,
    echo_control: Option<Arc<EchoCancellationControl>>,
    callback_buffer_observer: Option<Arc<AudioCallbackBufferObserver>>,
) -> Result<Stream, String>
where
    T: Sample + cpal::SizedSample + FromSample<f32> + Send + 'static,
{
    device
        .build_output_stream(
            stream_config,
            move |output: &mut [T], _| {
                if let Some(observer) = callback_buffer_observer.as_ref() {
                    observer.observe(output.len(), channels);
                }
                live_playback_callback(output, channels, &mixer, echo_control.as_ref());
            },
            move |error| {
                eprintln!("live playback stream error: {error}");
            },
            None,
        )
        .map_err(|error| format!("failed to build live output stream: {error}"))
}

pub(in crate::audio) fn build_typed_output_stream<T>(
    device: &cpal::Device,
    stream_config: StreamConfig,
    channels: usize,
    samples: Arc<Vec<i16>>,
    stats: PlaybackStats,
) -> Result<Stream, String>
where
    T: Sample + cpal::SizedSample + FromSample<f32> + Send + 'static,
{
    let data_stats = stats.clone();
    let error_stats = stats.clone();
    let mut cursor = 0usize;
    device
        .build_output_stream(
            stream_config,
            move |output: &mut [T], _| {
                playback_callback(output, channels, &samples, &mut cursor, &data_stats);
            },
            move |error| {
                error_stats
                    .inner
                    .stream_errors
                    .fetch_add(1, Ordering::Relaxed);
                error_stats.set_error(format!("playback stream error: {error}"));
            },
            None,
        )
        .map_err(|error| format!("failed to build output stream: {error}"))
}

pub(in crate::audio) fn playback_callback<T>(
    output: &mut [T],
    channels: usize,
    samples: &[i16],
    cursor: &mut usize,
    stats: &PlaybackStats,
) where
    T: Sample + FromSample<f32>,
{
    stats.inner.callbacks.fetch_add(1, Ordering::Relaxed);

    for frame in output.chunks_mut(channels.max(1)) {
        let sample = samples.get(*cursor).copied().unwrap_or(0);
        if *cursor < samples.len() {
            *cursor += 1;
        } else {
            stats.inner.finished.store(true, Ordering::Relaxed);
        }

        let output_sample = T::from_sample((sample as f32 / 32768.0).clamp(-1.0, 1.0));
        for channel in frame {
            *channel = output_sample;
        }
    }

    if *cursor >= samples.len() {
        stats.inner.finished.store(true, Ordering::Relaxed);
    }
    stats.inner.played_samples.store(*cursor, Ordering::Relaxed);
}

pub(in crate::audio) fn live_playback_callback<T>(
    output: &mut [T],
    channels: usize,
    mixer: &Arc<Mutex<LivePlaybackMixer>>,
    echo_control: Option<&Arc<EchoCancellationControl>>,
) where
    T: Sample + FromSample<f32>,
{
    let Ok(mut mixer) = mixer.lock() else {
        for sample in output {
            *sample = T::from_sample(0.0);
        }
        return;
    };

    let now = Instant::now();
    let output_frames = output.len() / channels.max(1);
    let mut echo_writer = match echo_control {
        Some(control) if control.enabled() => Some(control.reference().writer()),
        _ => None,
    };
    for frame in output.chunks_mut(channels.max(1)) {
        let sample = mixer.pop_mixed_output_sample(now, output_frames);
        if let Some(writer) = echo_writer.as_mut() {
            writer.push(sample);
        }
        let output_sample = T::from_sample(sample.clamp(-1.0, 1.0));
        for channel in frame {
            *channel = output_sample;
        }
    }
    if let Some(writer) = echo_writer {
        writer.commit();
    }
}

pub(in crate::audio) fn capture_callback<T>(
    input: &[T],
    channels: usize,
    sender: &SyncSender<Vec<f32>>,
    stats: &AudioStats,
) where
    T: Sample,
    f32: FromSample<T>,
{
    let mono = downmix_to_mono_i16_scale(input, channels);
    let samples = mono.len() as u64;
    let rms = rms_i16_scale(&mono);
    let peak = peak_i16_scale(&mono);
    stats.inner.callbacks.fetch_add(1, Ordering::Relaxed);
    stats
        .inner
        .captured_samples
        .fetch_add(samples, Ordering::Relaxed);
    stats.inner.rms_bits.store(rms.to_bits(), Ordering::Relaxed);
    stats
        .inner
        .peak_bits
        .store(peak.to_bits(), Ordering::Relaxed);

    if sender.try_send(mono).is_err() {
        stats.inner.dropped_chunks.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn downmix_to_mono_i16_scale<T>(input: &[T], channels: usize) -> Vec<f32>
where
    T: Sample,
    f32: FromSample<T>,
{
    if channels == 0 {
        return Vec::new();
    }

    let mut mono = Vec::with_capacity(input.len() / channels);
    for frame in input.chunks_exact(channels) {
        let mut sum = 0.0f32;
        for sample in frame {
            sum += sample.to_sample::<f32>() * i16::MAX as f32;
        }
        mono.push(sum / channels as f32);
    }
    mono
}

pub(in crate::audio) fn with_audio_backend_stderr_suppressed<T>(f: impl FnOnce() -> T) -> T {
    static STDERR_REDIRECT_LOCK: Mutex<()> = Mutex::new(());
    let _guard = STDERR_REDIRECT_LOCK.lock().ok();

    let Ok(dev_null) = std::fs::File::options().write(true).open("/dev/null") else {
        return f();
    };

    unsafe {
        let saved_stderr = libc::dup(libc::STDERR_FILENO);
        if saved_stderr < 0 {
            return f();
        }

        let _ = libc::fflush(std::ptr::null_mut());
        if libc::dup2(
            std::os::fd::AsRawFd::as_raw_fd(&dev_null),
            libc::STDERR_FILENO,
        ) < 0
        {
            let _ = libc::close(saved_stderr);
            return f();
        }

        let result = f();
        let _ = libc::fflush(std::ptr::null_mut());
        let _ = libc::dup2(saved_stderr, libc::STDERR_FILENO);
        let _ = libc::close(saved_stderr);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::audio::test_support::*;

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
    fn downmixes_interleaved_samples_to_mono_i16_scale() {
        let mono = downmix_to_mono_i16_scale(&[0.5f32, -0.5, 0.25, 0.75], 2);

        assert_eq!(mono.len(), 2);
        assert!(mono[0].abs() < 0.01);
        assert!((mono[1] - 0.5 * i16::MAX as f32).abs() < 1.0);
    }
}
