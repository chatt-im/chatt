mod audio;
mod packet_log;

use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use audio::{
    AudioStats, BufferRequest, DeviceInfo, Playback, PlaybackStats, Recording, RecordingConfig,
    StatsSnapshot,
};
use extui::{
    AnsiColor, BoxStyle, Buffer, Ellipsis, Rect, Style, Terminal, TerminalFlags,
    event::{self, Event, Events, KeyCode, KeyEvent, KeyModifiers, polling::GlobalWakerConfig},
    vt::Modifier,
};

const BITRATES: [i32; 4] = [16_000, 24_000, 32_000, 48_000];
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Device,
    Bitrate,
    Denoise,
    Buffer,
    Output,
    Record,
    Play,
    Refresh,
}

impl Focus {
    const ORDER: [Focus; 8] = [
        Focus::Device,
        Focus::Bitrate,
        Focus::Denoise,
        Focus::Buffer,
        Focus::Output,
        Focus::Record,
        Focus::Play,
        Focus::Refresh,
    ];

    fn index(self) -> usize {
        Self::ORDER
            .iter()
            .position(|focus| *focus == self)
            .unwrap_or(0)
    }
}

enum Action {
    Continue,
    Quit,
}

struct App {
    devices: Vec<DeviceInfo>,
    selected_device: usize,
    focus: Focus,
    bitrate_index: usize,
    buffer_index: usize,
    denoise: bool,
    output_path: String,
    editing_output: bool,
    status: String,
    recording: Option<Recording>,
    playback: Option<Playback>,
}

impl App {
    fn new() -> Self {
        let mut app = Self {
            devices: Vec::new(),
            selected_device: 0,
            focus: Focus::Device,
            bitrate_index: 1,
            buffer_index: 2,
            denoise: true,
            output_path: default_output_path(),
            editing_output: false,
            status: String::from("ready"),
            recording: None,
            playback: None,
        };
        app.refresh_devices();
        app
    }

    fn refresh_devices(&mut self) {
        if self.recording.is_some() || self.playback.is_some() {
            self.status = String::from("stop audio before refreshing devices");
            return;
        }

        match audio::input_devices(self.buffer_request()) {
            Ok(devices) => {
                self.devices = devices;
                if self.devices.is_empty() {
                    self.selected_device = 0;
                    self.status = String::from("no input devices found");
                } else {
                    self.selected_device = self.selected_device.min(self.devices.len() - 1);
                    self.status = format!("found {} input device(s)", self.devices.len());
                }
            }
            Err(error) => {
                self.devices.clear();
                self.selected_device = 0;
                self.status = error;
            }
        }
    }

    fn stop_finished_audio(&mut self) {
        let recording_stopped = self
            .recording
            .as_ref()
            .map(|recording| recording.stats().snapshot().worker_stopped)
            .unwrap_or(false);

        if recording_stopped {
            let snapshot = self.stop_recording();
            if let Some(error) = snapshot.last_error {
                self.status = format!("recording stopped: {error}");
            } else {
                self.status = format!(
                    "recording stopped: {} packets, {} bytes",
                    snapshot.encoded_packets, snapshot.encoded_bytes
                );
            }
        }

        let playback_finished = self
            .playback
            .as_ref()
            .map(|playback| playback.stats().snapshot().finished)
            .unwrap_or(false);

        if playback_finished {
            let snapshot = self.stop_playback();
            if let Some(error) = snapshot.last_error {
                self.status = format!("playback stopped: {error}");
            } else {
                self.status = format!(
                    "playback complete: {}/{} samples",
                    snapshot.played_samples, snapshot.total_samples
                );
            }
        }
    }

    fn process_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }

        if self.editing_output {
            return self.process_output_edit_key(key);
        }

        match key.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Char(' ') => self.toggle_recording(),
            KeyCode::Char('p') => self.toggle_playback(),
            KeyCode::Char('r') => self.refresh_devices(),
            KeyCode::Up => self.move_focus(-1),
            KeyCode::Down | KeyCode::Tab => self.move_focus(1),
            KeyCode::Left => self.adjust_focus(-1),
            KeyCode::Right => self.adjust_focus(1),
            KeyCode::Enter => self.activate_focus(),
            _ => {}
        }

        Action::Continue
    }

    fn process_output_edit_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Enter | KeyCode::Esc => {
                self.editing_output = false;
                self.status = String::from("output path updated");
            }
            KeyCode::Backspace => {
                self.output_path.pop();
            }
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.output_path.push(ch);
            }
            _ => {}
        }

        Action::Continue
    }

    fn move_focus(&mut self, delta: isize) {
        let len = Focus::ORDER.len() as isize;
        let next = (self.focus.index() as isize + delta).rem_euclid(len) as usize;
        self.focus = Focus::ORDER[next];
    }

    fn adjust_focus(&mut self, delta: isize) {
        match self.focus {
            Focus::Device => self.adjust_device(delta),
            Focus::Bitrate => {
                self.bitrate_index = cycle_index(self.bitrate_index, BITRATES.len(), delta);
            }
            Focus::Denoise => self.denoise = !self.denoise,
            Focus::Buffer => {
                self.buffer_index =
                    cycle_index(self.buffer_index, BufferRequest::OPTIONS.len(), delta);
                self.refresh_devices();
            }
            Focus::Output | Focus::Record | Focus::Play | Focus::Refresh => {}
        }
    }

    fn adjust_device(&mut self, delta: isize) {
        if self.devices.is_empty() {
            return;
        }
        self.selected_device = cycle_index(self.selected_device, self.devices.len(), delta);
    }

    fn activate_focus(&mut self) {
        match self.focus {
            Focus::Denoise => self.denoise = !self.denoise,
            Focus::Output => {
                if self.recording.is_none() && self.playback.is_none() {
                    self.editing_output = true;
                    self.status = String::from("editing output path");
                } else {
                    self.status = String::from("stop audio before editing output path");
                }
            }
            Focus::Record => self.toggle_recording(),
            Focus::Play => self.toggle_playback(),
            Focus::Refresh => self.refresh_devices(),
            Focus::Device | Focus::Bitrate | Focus::Buffer => self.adjust_focus(1),
        }
    }

    fn toggle_recording(&mut self) {
        if self.playback.is_some() {
            self.status = String::from("stop playback before recording");
            return;
        }

        if self.recording.is_some() {
            let snapshot = self.stop_recording();
            self.status = format!(
                "recorded {} packets ({} bytes)",
                snapshot.encoded_packets, snapshot.encoded_bytes
            );
        } else {
            self.start_recording();
        }
    }

    fn toggle_playback(&mut self) {
        if self.recording.is_some() {
            self.status = String::from("stop recording before playback");
            return;
        }

        if self.playback.is_some() {
            let snapshot = self.stop_playback();
            self.status = format!(
                "playback stopped: {}/{} samples",
                snapshot.played_samples, snapshot.total_samples
            );
        } else {
            self.start_playback();
        }
    }

    fn start_recording(&mut self) {
        if self.devices.is_empty() {
            self.status = String::from("no input device selected");
            return;
        }

        let Some(device) = self.devices.get(self.selected_device) else {
            self.status = String::from("selected input device is unavailable");
            return;
        };
        if !device.supported {
            self.status = device
                .issue
                .clone()
                .unwrap_or_else(|| String::from("selected device is unsupported"));
            return;
        }
        if self.output_path.trim().is_empty() {
            self.status = String::from("output path is empty");
            return;
        }

        let config = RecordingConfig {
            device_index: self.selected_device,
            bitrate_bps: BITRATES[self.bitrate_index],
            denoise: self.denoise,
            output_path: PathBuf::from(self.output_path.trim()),
            buffer_request: self.buffer_request(),
        };

        match audio::start_recording(config) {
            Ok(recording) => {
                self.recording = Some(recording);
                self.status = String::from("recording");
            }
            Err(error) => {
                self.status = error;
            }
        }
    }

    fn stop_recording(&mut self) -> StatsSnapshot {
        if let Some(recording) = self.recording.take() {
            recording.stop()
        } else {
            StatsSnapshot::default()
        }
    }

    fn start_playback(&mut self) {
        if self.output_path.trim().is_empty() {
            self.status = String::from("output path is empty");
            return;
        }

        match audio::start_playback(
            PathBuf::from(self.output_path.trim()).as_path(),
            self.buffer_request(),
        ) {
            Ok(playback) => {
                self.playback = Some(playback);
                self.status = String::from("playing output");
            }
            Err(error) => {
                self.status = error;
            }
        }
    }

    fn stop_playback(&mut self) -> audio::PlaybackSnapshot {
        if let Some(playback) = self.playback.take() {
            playback.stop()
        } else {
            audio::PlaybackSnapshot::default()
        }
    }

    fn bitrate_bps(&self) -> i32 {
        BITRATES[self.bitrate_index]
    }

    fn buffer_request(&self) -> BufferRequest {
        BufferRequest::OPTIONS[self.buffer_index]
    }

    fn active_stats(&self) -> Option<AudioStats> {
        self.recording.as_ref().map(Recording::stats)
    }

    fn playback_stats(&self) -> Option<PlaybackStats> {
        self.playback.as_ref().map(Playback::stats)
    }
}

fn cycle_index(current: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    (current as isize + delta).rem_euclid(len as isize) as usize
}

fn default_output_path() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("recordings/capture-{seconds}.tcopus")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    event::polling::initialize_global_waker(GlobalWakerConfig {
        resize: true,
        termination: true,
    })?;

    let flags = TerminalFlags::RAW_MODE
        | TerminalFlags::ALT_SCREEN
        | TerminalFlags::HIDE_CURSOR
        | TerminalFlags::EXTENDED_KEYBOARD_INPUTS;
    let mut terminal = Terminal::open(flags)?;
    let (w, h) = terminal.size()?;
    let mut buffer = Buffer::new(w, h);
    let mut events = Events::default();
    let stdin = std::io::stdin();
    let mut app = App::new();

    loop {
        app.stop_finished_audio();
        render(&app, &mut buffer);
        buffer.render(&mut terminal);

        if event::poll(&stdin, Some(POLL_INTERVAL))?.is_readable() {
            events.read_from(&stdin)?;
        }

        while let Some(event) = events.next(terminal.is_raw()) {
            match event {
                Event::Key(key) => {
                    if matches!(app.process_key(key), Action::Quit) {
                        app.stop_recording();
                        app.stop_playback();
                        return Ok(());
                    }
                }
                Event::Resized => {
                    let (new_w, new_h) = terminal.size()?;
                    buffer.resize(new_w, new_h);
                }
                _ => {}
            }
        }
    }
}

fn render(app: &App, buf: &mut Buffer) {
    let mut screen = buf.rect().inset(1, 0);
    draw_header(screen.take_top(3), buf);

    let mut body = BoxStyle::LIGHT.render(screen.take_top(12), buf).inset(1, 0);
    draw_config_rows(app, &mut body, buf);

    let mut status_box = BoxStyle::LIGHT.render(screen.take_top(9), buf).inset(1, 0);
    draw_status(app, &mut status_box, buf);

    draw_footer(screen, app, buf);
}

fn draw_header(mut area: Rect, buf: &mut Buffer) {
    area.take_top(1)
        .with(AnsiColor::White.with_fg(AnsiColor::Black) | Modifier::BOLD)
        .fill(buf)
        .text(buf, " tomchat audio configuration ");
    area.take_top(1).with(AnsiColor::Grey[20].as_fg()).text(
        buf,
        "48 kHz mono capture -> optional nnnoiseless -> Opus packet log",
    );
}

fn draw_config_rows(app: &App, area: &mut Rect, buf: &mut Buffer) {
    let device_value = selected_device_label(app);
    draw_row(
        area.take_top(1),
        buf,
        "Input device",
        &device_value,
        app.focus == Focus::Device,
    );
    draw_row(
        area.take_top(1),
        buf,
        "Bitrate",
        &format!("{} kbps", app.bitrate_bps() / 1000),
        app.focus == Focus::Bitrate,
    );
    draw_row(
        area.take_top(1),
        buf,
        "Denoise",
        if app.denoise { "on" } else { "off" },
        app.focus == Focus::Denoise,
    );
    draw_row(
        area.take_top(1),
        buf,
        "CPAL buffer",
        app.buffer_request().label(),
        app.focus == Focus::Buffer,
    );
    let output_label;
    let output_value = if app.editing_output {
        output_label = format!("{}_", app.output_path);
        output_label.as_str()
    } else {
        app.output_path.as_str()
    };
    draw_row(
        area.take_top(1),
        buf,
        "Output path",
        output_value,
        app.focus == Focus::Output,
    );

    area.take_top(1).display().text(buf, "");

    let record_label = if app.recording.is_some() {
        "Stop recording"
    } else {
        "Start recording"
    };
    draw_button_row(
        area.take_top(1),
        buf,
        record_label,
        app.focus == Focus::Record,
        app.recording.is_some(),
    );
    let play_label = if app.playback.is_some() {
        "Stop playback"
    } else {
        "Play output"
    };
    draw_button_row(
        area.take_top(1),
        buf,
        play_label,
        app.focus == Focus::Play,
        app.playback.is_some(),
    );
    draw_button_row(
        area.take_top(1),
        buf,
        "Refresh devices",
        app.focus == Focus::Refresh,
        false,
    );
}

fn draw_status(app: &App, area: &mut Rect, buf: &mut Buffer) {
    let snapshot = app
        .active_stats()
        .map(|stats| stats.snapshot())
        .unwrap_or_default();
    let selected = app.devices.get(app.selected_device);
    let playback = app.playback_stats().map(|stats| stats.snapshot());

    draw_row(area.take_top(1), buf, "Status", &app.status, false);
    draw_row(
        area.take_top(1),
        buf,
        "Stream",
        &stream_label(selected),
        false,
    );
    draw_row(
        area.take_top(1),
        buf,
        "RMS",
        &meter_label(snapshot.rms),
        false,
    );
    draw_row(
        area.take_top(1),
        buf,
        "VAD",
        &format!("{:>3}%", (snapshot.vad_probability * 100.0).round() as u32),
        false,
    );
    draw_row(
        area.take_top(1),
        buf,
        "Packets",
        &format!(
            "{} packets, {} bytes",
            snapshot.encoded_packets, snapshot.encoded_bytes
        ),
        false,
    );
    draw_row(
        area.take_top(1),
        buf,
        "Capture",
        &format!(
            "{} callbacks, {} samples",
            snapshot.callbacks, snapshot.captured_samples
        ),
        false,
    );
    draw_row(
        area.take_top(1),
        buf,
        "Playback",
        &playback_label(playback.as_ref()),
        false,
    );
    let playback_errors = playback
        .as_ref()
        .map(|snapshot| snapshot.stream_errors)
        .unwrap_or(0);
    draw_row(
        area.take_top(1),
        buf,
        "Drops/errors",
        &format!(
            "{}/{} rec, {} play",
            snapshot.dropped_chunks, snapshot.stream_errors, playback_errors
        ),
        false,
    );
    if let Some(error) = snapshot.last_error {
        draw_row(area.take_top(1), buf, "Last error", &error, false);
    }
}

fn draw_footer(area: Rect, app: &App, buf: &mut Buffer) {
    let controls = if app.editing_output {
        "enter/esc finish edit | backspace delete | ctrl-c quit"
    } else {
        "up/down focus | left/right adjust | enter activate | space record | p play | r refresh | q quit"
    };
    area.with(AnsiColor::Grey[16].as_fg())
        .with(Ellipsis(true))
        .text(buf, controls);
}

fn draw_row(area: Rect, buf: &mut Buffer, label: &str, value: &str, focused: bool) {
    let row_style = if focused {
        AnsiColor::Grey[8].with_fg(AnsiColor::White)
    } else {
        Style::DEFAULT
    };
    area.with(row_style).fill(buf);

    let mut row = area;
    row.take_left(16)
        .with(row_style.patch(AnsiColor::Grey[20].as_fg()))
        .with(Ellipsis(true))
        .text(buf, label);
    row.with(row_style).with(Ellipsis(true)).text(buf, value);
}

fn draw_button_row(area: Rect, buf: &mut Buffer, label: &str, focused: bool, active: bool) {
    let marker = if active { "[x]" } else { "[ ]" };
    let style = if focused {
        AnsiColor::Grey[8].with_fg(AnsiColor::White)
    } else {
        Style::DEFAULT
    };
    area.with(style).fill(buf);
    area.with(style)
        .text(buf, marker)
        .skip(1)
        .with(Ellipsis(true))
        .text(buf, label);
}

fn selected_device_label(app: &App) -> String {
    match app.devices.get(app.selected_device) {
        None => String::from("none"),
        Some(device) if device.supported => {
            let preview = device
                .preview
                .as_ref()
                .expect("supported device has preview");
            format!(
                "{} ({} ch, {}, {})",
                device.name, preview.channels, preview.sample_format, preview.buffer_note
            )
        }
        Some(device) => format!(
            "{} ({})",
            device.name,
            device.issue.as_deref().unwrap_or("unsupported")
        ),
    }
}

fn stream_label(device: Option<&DeviceInfo>) -> String {
    let Some(device) = device else {
        return String::from("no selected device");
    };
    let Some(preview) = &device.preview else {
        return device
            .issue
            .clone()
            .unwrap_or_else(|| String::from("unsupported"));
    };

    format!(
        "{} Hz, {} ch, {}, {}",
        audio::SAMPLE_RATE,
        preview.channels,
        preview.sample_format,
        buffer_size_label(preview.buffer_size)
    )
}

fn playback_label(snapshot: Option<&audio::PlaybackSnapshot>) -> String {
    let Some(snapshot) = snapshot else {
        return String::from("idle");
    };
    if snapshot.total_samples == 0 {
        return String::from("loaded 0 samples");
    }

    let percent = (snapshot.played_samples as f32 / snapshot.total_samples as f32 * 100.0)
        .clamp(0.0, 100.0)
        .round() as u32;
    format!(
        "{} / {} samples ({}%, {} cb)",
        snapshot.played_samples, snapshot.total_samples, percent, snapshot.callbacks
    )
}

fn buffer_size_label(buffer_size: cpal::BufferSize) -> String {
    match buffer_size {
        cpal::BufferSize::Default => String::from("default buffer"),
        cpal::BufferSize::Fixed(frames) => format!("{frames} frame buffer"),
    }
}

fn meter_label(rms: f32) -> String {
    const WIDTH: usize = 20;
    let filled = ((rms * 2.0).clamp(0.0, 1.0) * WIDTH as f32).round() as usize;
    let mut meter = String::with_capacity(WIDTH + 8);
    meter.push('[');
    for index in 0..WIDTH {
        meter.push(if index < filled { '#' } else { '-' });
    }
    meter.push(']');
    format!(
        "{meter} {:>3}%",
        (rms.clamp(0.0, 1.0) * 100.0).round() as u32
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycles_indices_in_both_directions() {
        assert_eq!(cycle_index(0, 3, -1), 2);
        assert_eq!(cycle_index(2, 3, 1), 0);
        assert_eq!(cycle_index(1, 3, 1), 2);
    }

    #[test]
    fn renders_minimum_smoke_frame() {
        let app = App {
            devices: Vec::new(),
            selected_device: 0,
            focus: Focus::Device,
            bitrate_index: 1,
            buffer_index: 2,
            denoise: true,
            output_path: String::from("out.tcopus"),
            editing_output: false,
            status: String::from("test"),
            recording: None,
            playback: None,
        };
        let mut buffer = Buffer::new(72, 24);

        render(&app, &mut buffer);
    }

    #[test]
    fn renders_cramped_smoke_frame() {
        let app = App {
            devices: Vec::new(),
            selected_device: 0,
            focus: Focus::Output,
            bitrate_index: 1,
            buffer_index: 2,
            denoise: true,
            output_path: String::from("out.tcopus"),
            editing_output: true,
            status: String::from("test"),
            recording: None,
            playback: None,
        };
        let mut buffer = Buffer::new(32, 10);

        render(&app, &mut buffer);
    }
}
