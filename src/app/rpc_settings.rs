use std::collections::HashSet;

use local_rpc::{
    frame::{Operation, RequestOutcome, RequestResult},
    model::RequestId,
    settings as wire,
};

use crate::{
    audio,
    client_channel::ClientId,
    config::{self, RuntimeSaveError},
    settings,
};

use super::{App, audio_restart_flags};

pub(super) struct RpcSettingsSession {
    pub(super) owner: ClientId,
    id: wire::SettingsSessionId,
    revision: u64,
    source_exists: bool,
    document: config::Config,
    restore_audio: config::AudioConfig,
    preview_seq: u64,
    preview_active: bool,
}

#[derive(Clone, Copy)]
struct SectionSpec {
    id: &'static str,
    title: &'static str,
    help: &'static str,
}

#[derive(Clone, Copy)]
enum ChoiceSpec {
    Static(&'static [(&'static str, &'static str)]),
    Dynamic {
        list: fn(&App, &config::Config) -> Vec<wire::SettingsChoice>,
        refresh: fn(&mut App, &[wire::SettingsChange]) -> Vec<wire::SettingsDiagnostic>,
    },
}

#[derive(Clone, Copy)]
struct ControlSpec {
    kind: u8,
    choices: ChoiceSpec,
    min: Option<f32>,
    max: Option<f32>,
    step: Option<f32>,
    unit: &'static str,
    placeholder: &'static str,
}

#[derive(Clone, Copy)]
enum FieldAccess {
    Audio {
        read: fn(&config::AudioConfig) -> wire::SettingsValue,
        write:
            fn(&mut config::AudioConfig, &wire::SettingsValue) -> Result<(), String>,
    },
    Config {
        read: fn(&config::Config) -> wire::SettingsValue,
        write: fn(&mut config::Config, &wire::SettingsValue) -> Result<(), String>,
    },
}

#[derive(Clone, Copy)]
struct FieldSpec {
    id: wire::SettingsFieldId,
    section: usize,
    key: &'static str,
    label: &'static str,
    help: &'static str,
    flags: u8,
    control: ControlSpec,
    access: FieldAccess,
}

impl FieldSpec {
    fn read(self, config: &config::Config) -> wire::SettingsValue {
        match self.access {
            FieldAccess::Audio { read, .. } => read(&config.audio),
            FieldAccess::Config { read, .. } => read(config),
        }
    }

    fn write(
        self,
        config: &mut config::Config,
        value: &wire::SettingsValue,
    ) -> Result<(), String> {
        match self.access {
            FieldAccess::Audio { write, .. } => write(&mut config.audio, value),
            FieldAccess::Config { write, .. } => write(config, value),
        }
    }

    fn write_audio(
        self,
        audio: &mut config::AudioConfig,
        value: &wire::SettingsValue,
    ) -> Result<(), String> {
        match self.access {
            FieldAccess::Audio { write, .. } => write(audio, value),
            FieldAccess::Config { .. } => {
                Err("non-audio field is not valid in an audio preview".into())
            }
        }
    }

    fn wire_control(self, app: &App, config: &config::Config) -> wire::SettingsControl {
        let choices = match self.control.choices {
            ChoiceSpec::Static(choices) => choices
                .iter()
                .map(|(value, label)| wire::SettingsChoice {
                    value: (*value).into(),
                    label: (*label).into(),
                    detail: String::new(),
                    search: String::new(),
                    enabled: true,
                })
                .collect(),
            ChoiceSpec::Dynamic { list, .. } => list(app, config),
        };
        wire::SettingsControl {
            kind: self.control.kind,
            choices,
            min: self.control.min,
            max: self.control.max,
            step: self.control.step,
            unit: self.control.unit.into(),
            placeholder: self.control.placeholder.into(),
        }
    }

    fn refresh_choices(
        self,
    ) -> Option<fn(&mut App, &[wire::SettingsChange]) -> Vec<wire::SettingsDiagnostic>> {
        match self.control.choices {
            ChoiceSpec::Dynamic { refresh, .. } => Some(refresh),
            ChoiceSpec::Static(_) => None,
        }
    }
}

impl App {
    pub(crate) fn handle_rpc_settings(
        &mut self,
        owner: ClientId,
        request_id: RequestId,
        command: wire::SettingsCommand,
    ) -> wire::SettingsResult {
        let operation = command.operation();
        match command {
            wire::SettingsCommand::Open => self.open_rpc_settings(owner, request_id),
            wire::SettingsCommand::SetAudioPreviewActive { session_id, active } => {
                if let Err(result) =
                    self.require_rpc_settings_owner(owner, request_id, operation, session_id)
                {
                    return result;
                }
                if active && !self.allow_settings_preview_capture {
                    return rejected_settings(
                        request_id,
                        operation,
                        409,
                        "microphone preview is unavailable while the soundboard owns capture",
                        vec![diagnostic(
                            "audio.input-device-id",
                            "microphone preview is unavailable in this daemon mode",
                        )],
                        wire::SettingsResultPayload::None,
                    );
                }
                if active {
                    self.start_settings_preview_capture();
                } else {
                    self.set_loopback_enabled(false);
                    self.stop_settings_preview_capture();
                }
                if let Some(session) = &mut self.rpc_settings {
                    session.preview_active = active;
                }
                wire::SettingsResult::accepted(
                    request_id,
                    operation,
                    wire::SettingsResultPayload::PreviewApplied {
                        session_id,
                        runtime: self.rpc_audio_runtime(),
                    },
                )
            }
            wire::SettingsCommand::PreviewAudio {
                session_id,
                preview_seq,
                changes,
                loopback,
            } => {
                if let Err(result) =
                    self.require_rpc_settings_owner(owner, request_id, operation, session_id)
                {
                    return result;
                }
                let last_seq = self
                    .rpc_settings
                    .as_ref()
                    .map_or(0, |session| session.preview_seq);
                if preview_seq > last_seq {
                    let mut audio = self
                        .rpc_settings
                        .as_ref()
                        .expect("validated settings session exists")
                        .document
                        .audio
                        .clone();
                    let mut diagnostics = data_apply_audio_changes(&mut audio, &changes);
                    diagnostics.extend(validate_audio(&audio));
                    if has_errors(&diagnostics) {
                        return rejected_settings(
                            request_id,
                            operation,
                            422,
                            "audio settings are invalid",
                            diagnostics,
                            wire::SettingsResultPayload::None,
                        );
                    }
                    self.apply_rpc_audio(audio);
                    let preview_active = self
                        .rpc_settings
                        .as_ref()
                        .is_some_and(|session| session.preview_active);
                    self.set_loopback_enabled(
                        self.allow_settings_preview_capture && preview_active && loopback,
                    );
                    if let Some(session) = &mut self.rpc_settings {
                        session.preview_seq = preview_seq;
                    }
                }
                wire::SettingsResult::accepted(
                    request_id,
                    operation,
                    wire::SettingsResultPayload::PreviewApplied {
                        session_id,
                        runtime: self.rpc_audio_runtime(),
                    },
                )
            }
            wire::SettingsCommand::RefreshChoices {
                session_id,
                field,
                changes,
            } => {
                if let Err(result) =
                    self.require_rpc_settings_owner(owner, request_id, operation, session_id)
                {
                    return result;
                }
                let Some(refresh) = data_field(field).and_then(FieldSpec::refresh_choices) else {
                    return rejected_settings(
                        request_id,
                        operation,
                        422,
                        "setting choices are not refreshable",
                        Vec::new(),
                        wire::SettingsResultPayload::None,
                    );
                };
                let diagnostics = refresh(self, &changes);
                if has_errors(&diagnostics) {
                    return rejected_settings(
                        request_id,
                        operation,
                        422,
                        "settings are invalid",
                        diagnostics,
                        wire::SettingsResultPayload::None,
                    );
                }
                wire::SettingsResult::accepted(
                    request_id,
                    operation,
                    wire::SettingsResultPayload::Document(self.rpc_settings_document()),
                )
            }
            wire::SettingsCommand::Reload { session_id } => {
                if let Err(result) =
                    self.require_rpc_settings_owner(owner, request_id, operation, session_id)
                {
                    return result;
                }
                self.reload_rpc_settings(request_id, operation)
            }
            wire::SettingsCommand::Save {
                session_id,
                expected_revision,
                changes,
                force,
            } => self.save_rpc_settings(
                owner,
                request_id,
                operation,
                session_id,
                expected_revision,
                changes,
                force,
            ),
            wire::SettingsCommand::Close { session_id } => {
                if let Err(result) =
                    self.require_rpc_settings_owner(owner, request_id, operation, session_id)
                {
                    return result;
                }
                self.finish_rpc_settings_session();
                wire::SettingsResult::accepted(
                    request_id,
                    operation,
                    wire::SettingsResultPayload::Closed { session_id },
                )
            }
        }
    }

    fn open_rpc_settings(
        &mut self,
        owner: ClientId,
        request_id: RequestId,
    ) -> wire::SettingsResult {
        if self
            .rpc_settings
            .as_ref()
            .is_some_and(|session| session.owner == owner)
        {
            return wire::SettingsResult::accepted(
                request_id,
                Operation::OpenSettings,
                wire::SettingsResultPayload::Document(self.rpc_settings_document()),
            );
        }
        if self.room.settings_owner.is_some() || self.rpc_settings.is_some() {
            return rejected_settings(
                request_id,
                Operation::OpenSettings,
                409,
                "settings are already open in another client",
                Vec::new(),
                wire::SettingsResultPayload::None,
            );
        }
        let document = match self.config.load_runtime_document() {
            Ok(document) => document,
            Err(error) => {
                return rejected_settings(
                    request_id,
                    Operation::OpenSettings,
                    500,
                    &error,
                    vec![diagnostic("settings", &error)],
                    wire::SettingsResultPayload::None,
                );
            }
        };
        let id = wire::SettingsSessionId(self.next_rpc_settings_session_id.max(1));
        self.next_rpc_settings_session_id =
            self.next_rpc_settings_session_id.wrapping_add(1).max(1);
        self.rpc_settings = Some(RpcSettingsSession {
            owner,
            id,
            revision: document.revision,
            source_exists: document.source_exists,
            document: document.config,
            restore_audio: self.config.audio.clone(),
            preview_seq: 0,
            preview_active: false,
        });
        self.room.settings_owner = Some(owner);
        wire::SettingsResult::accepted(
            request_id,
            Operation::OpenSettings,
            wire::SettingsResultPayload::Document(self.rpc_settings_document()),
        )
    }

    fn require_rpc_settings_owner(
        &self,
        owner: ClientId,
        request_id: RequestId,
        operation: Operation,
        session_id: wire::SettingsSessionId,
    ) -> Result<(), wire::SettingsResult> {
        if self
            .rpc_settings
            .as_ref()
            .is_some_and(|session| session.owner == owner && session.id == session_id)
        {
            return Ok(());
        }
        Err(rejected_settings(
            request_id,
            operation,
            409,
            "settings session is no longer active",
            Vec::new(),
            wire::SettingsResultPayload::None,
        ))
    }

    fn save_rpc_settings(
        &mut self,
        owner: ClientId,
        request_id: RequestId,
        operation: Operation,
        session_id: wire::SettingsSessionId,
        expected_revision: u64,
        changes: Vec<wire::SettingsChange>,
        force: bool,
    ) -> wire::SettingsResult {
        if let Err(result) =
            self.require_rpc_settings_owner(owner, request_id, operation, session_id)
        {
            return result;
        }
        let revision = self
            .rpc_settings
            .as_ref()
            .expect("validated session exists")
            .revision;
        if expected_revision != revision {
            return rejected_settings(
                request_id,
                operation,
                409,
                "settings snapshot is stale",
                Vec::new(),
                wire::SettingsResultPayload::Conflict {
                    latest: self.rpc_settings_document(),
                },
            );
        }
        let latest = match self.config.load_runtime_document() {
            Ok(document) => document,
            Err(error) => {
                return rejected_settings(
                    request_id,
                    operation,
                    500,
                    &error,
                    vec![diagnostic("settings", &error)],
                    wire::SettingsResultPayload::None,
                );
            }
        };
        if latest.revision != revision && !force {
            if let Some(session) = &mut self.rpc_settings {
                session.document = latest.config;
                session.revision = latest.revision;
                session.source_exists = latest.source_exists;
            }
            return rejected_settings(
                request_id,
                operation,
                409,
                "configuration changed outside Chatt",
                Vec::new(),
                wire::SettingsResultPayload::Conflict {
                    latest: self.rpc_settings_document(),
                },
            );
        }
        let save_revision = latest.revision;
        let mut candidate = latest.config;
        let mut diagnostics = data_apply_changes(&mut candidate, &changes);
        diagnostics.extend(validate_settings(&candidate));
        if has_errors(&diagnostics) {
            return rejected_settings(
                request_id,
                operation,
                422,
                "settings are invalid",
                diagnostics,
                wire::SettingsResultPayload::None,
            );
        }

        let save = candidate.save_runtime_at_revision(save_revision, force);
        let (path, saved_revision) = match save {
            Ok(saved) => saved,
            Err(RuntimeSaveError::Conflict { .. }) => {
                let latest = match self.config.load_runtime_document() {
                    Ok(document) => document,
                    Err(error) => {
                        return rejected_settings(
                            request_id,
                            operation,
                            409,
                            "configuration changed outside Chatt",
                            vec![diagnostic("settings", &error)],
                            wire::SettingsResultPayload::None,
                        );
                    }
                };
                if let Some(session) = &mut self.rpc_settings {
                    session.document = latest.config;
                    session.revision = latest.revision;
                    session.source_exists = latest.source_exists;
                }
                return rejected_settings(
                    request_id,
                    operation,
                    409,
                    "configuration changed outside Chatt",
                    Vec::new(),
                    wire::SettingsResultPayload::Conflict {
                        latest: self.rpc_settings_document(),
                    },
                );
            }
            Err(RuntimeSaveError::Other(error)) => {
                return rejected_settings(
                    request_id,
                    operation,
                    500,
                    &error,
                    vec![diagnostic("settings", &error)],
                    wire::SettingsResultPayload::None,
                );
            }
        };
        candidate.config_path = Some(path.clone());
        self.apply_rpc_settings_config(&candidate);
        self.config.config_path = Some(path);
        if let Some(session) = &mut self.rpc_settings {
            session.document = candidate;
            session.revision = saved_revision;
            session.source_exists = true;
            session.restore_audio = self.config.audio.clone();
        }
        wire::SettingsResult::accepted(
            request_id,
            operation,
            wire::SettingsResultPayload::Document(self.rpc_settings_document()),
        )
    }

    fn reload_rpc_settings(
        &mut self,
        request_id: RequestId,
        operation: Operation,
    ) -> wire::SettingsResult {
        let latest = match self.config.load_runtime_document() {
            Ok(document) => document,
            Err(error) => {
                return rejected_settings(
                    request_id,
                    operation,
                    422,
                    "configuration on disk is invalid",
                    vec![diagnostic("settings", &error)],
                    wire::SettingsResultPayload::None,
                );
            }
        };
        self.apply_rpc_settings_config(&latest.config);
        if let Some(session) = &mut self.rpc_settings {
            session.document = latest.config;
            session.revision = latest.revision;
            session.source_exists = latest.source_exists;
            session.restore_audio = self.config.audio.clone();
            session.preview_seq = 0;
        }
        wire::SettingsResult::accepted(
            request_id,
            operation,
            wire::SettingsResultPayload::Document(self.rpc_settings_document()),
        )
    }

    fn apply_rpc_settings_config(&mut self, candidate: &config::Config) {
        let old_audio = self.config.audio.clone();
        let old_web = self.config.web.clone();
        let old_files = self.config.files.clone();
        let old_p2p_enabled = self.config.p2p.enabled;
        let old_history_enabled = self.config.history.enabled;
        self.config.audio = candidate.audio.clone();
        self.config.notifications = candidate.notifications.clone();
        self.config.files = candidate.files.clone();
        self.config.history = candidate.history.clone();
        self.config.p2p = candidate.p2p.clone();
        self.config.web = candidate.web.clone();
        self.apply_echo_cancellation_setting();
        self.apply_output_volume_setting();
        self.apply_active_capture_amplification(self.config.audio.max_amplification);
        let (capture, playback) = audio_restart_flags(&old_audio, &self.config.audio);
        if capture || playback {
            self.schedule_audio_apply(capture, playback);
        }
        self.apply_web_setting(&old_web, old_files.max_upload_bytes());
        self.apply_p2p_setting(old_p2p_enabled);
        self.apply_history_setting(old_history_enabled);
        self.apply_file_settings(&old_files);
        self.drop_notification_playback();
        self.mark_daemon_config_changed();
    }

    fn apply_rpc_audio(&mut self, audio: config::AudioConfig) {
        let old = self.config.audio.clone();
        self.config.audio = audio;
        self.apply_echo_cancellation_setting();
        self.apply_output_volume_setting();
        self.apply_active_capture_amplification(self.config.audio.max_amplification);
        let (capture, playback) = audio_restart_flags(&old, &self.config.audio);
        if capture || playback {
            self.schedule_audio_apply(capture, playback);
        }
        if playback || self.config.notifications.sounds != config::NotificationSoundMode::Always {
            self.drop_notification_playback();
        }
    }

    pub(super) fn finish_rpc_settings_session(&mut self) {
        let Some(session) = self.rpc_settings.take() else {
            return;
        };
        self.room.settings_owner = None;
        self.set_loopback_enabled(false);
        self.stop_settings_preview_capture();
        self.settings_preview_refresh_id = None;
        self.apply_rpc_audio(session.restore_audio);
    }

    fn rpc_settings_document(&self) -> wire::SettingsDocument {
        let session = self
            .rpc_settings
            .as_ref()
            .expect("RPC settings document requires an active session");
        wire::SettingsDocument {
            session_id: session.id,
            revision: session.revision,
            source: if session.source_exists {
                wire::SettingsSourceStatus::File
            } else {
                wire::SettingsSourceStatus::Defaults
            },
            sections: data_settings_sections(self, &session.document),
            actions: wire::SettingsActions {
                audio_preview: self.allow_settings_preview_capture,
                audio_loopback: self.allow_settings_preview_capture,
            },
            audio_runtime: self.rpc_audio_runtime(),
            diagnostics: Vec::new(),
        }
    }

    pub(crate) fn rpc_settings_document_event(
        &self,
        owner: ClientId,
        previous_generation: u64,
    ) -> Option<(u64, wire::SettingsEvent)> {
        self.rpc_settings
            .as_ref()
            .filter(|session| session.owner == owner)?;
        let generation = self.room.audio_devices.generation;
        (generation != previous_generation).then(|| {
            (
                generation,
                wire::SettingsEvent::Document(self.rpc_settings_document()),
            )
        })
    }

    pub(crate) fn rpc_settings_device_generation(&self) -> u64 {
        self.room.audio_devices.generation
    }

    pub(crate) fn rpc_settings_audio_events(
        &self,
        owner: ClientId,
    ) -> Option<(wire::SettingsEvent, wire::SettingsEvent)> {
        let session = self
            .rpc_settings
            .as_ref()
            .filter(|session| session.owner == owner && session.preview_active)?;
        let snapshot = self
            .capture
            .as_ref()
            .map(|capture| capture.stats().snapshot())
            .unwrap_or_default();
        Some((
            wire::SettingsEvent::AudioMeter {
                session_id: session.id,
                rms: snapshot.rms.max(0.0),
                peak: snapshot.peak.max(0.0),
                voice_active: snapshot.voice_active,
            },
            wire::SettingsEvent::AudioRuntime {
                session_id: session.id,
                runtime: self.rpc_audio_runtime(),
            },
        ))
    }

    fn rpc_audio_runtime(&self) -> wire::AudioRuntimeState {
        let mut diagnostics = Vec::new();
        if let Some(error) = &self.mic_error {
            diagnostics.push(diagnostic("audio.input-device-id", error));
        }
        if let Some(error) = &self.playback_error {
            diagnostics.push(diagnostic("audio.output-device-id", error));
        }
        wire::AudioRuntimeState {
            preview_active: self
                .rpc_settings
                .as_ref()
                .is_some_and(|session| session.preview_active),
            loopback: self.loopback_tap.is_active(),
            applying: self.pending_audio_apply.is_some(),
            preview_seq: self
                .rpc_settings
                .as_ref()
                .map_or(0, |session| session.preview_seq),
            diagnostics,
        }
    }

}

fn device_choices(
    devices: Vec<settings::AudioDeviceItem>,
    current: &Option<String>,
) -> Vec<wire::SettingsChoice> {
    let mut choices: Vec<_> = devices
        .into_iter()
        .map(|device| {
            let detail = device.primary_metadata();
            let selection = current
                .as_ref()
                .filter(|current| {
                    device.selection.as_ref() == Some(*current)
                        || device.aliases.contains(*current)
                })
                .cloned()
                .or_else(|| device.selection.clone());
            let mut search = device.search_text;
            for alias in &device.aliases {
                search.push(' ');
                search.push_str(alias);
            }
            if let Some(selection) = &device.selection {
                search.push(' ');
                search.push_str(selection);
            }
            wire::SettingsChoice {
                value: selection.unwrap_or_default(),
                label: device.name,
                detail,
                search,
                enabled: device.supported,
            }
        })
        .collect();
    if choices.len() > local_rpc::MAX_SETTINGS_CHOICES {
        if let Some(selected) = current
            .as_ref()
            .and_then(|current| choices.iter().position(|choice| choice.value == *current))
            .filter(|selected| *selected >= local_rpc::MAX_SETTINGS_CHOICES)
        {
            choices.swap(local_rpc::MAX_SETTINGS_CHOICES - 1, selected);
        }
        choices.truncate(local_rpc::MAX_SETTINGS_CHOICES);
    }
    if let Some(current) = current
        && !choices.iter().any(|choice| choice.value == *current)
    {
        if choices.len() == local_rpc::MAX_SETTINGS_CHOICES {
            choices.pop();
        }
        choices.push(wire::SettingsChoice {
            value: current.clone(),
            label: format!("Unavailable device ({current})"),
            detail: "This device is not currently reported by the audio backend.".into(),
            search: current.clone(),
            enabled: false,
        });
    }
    choices
}

fn input_device_choices(app: &App, config: &config::Config) -> Vec<wire::SettingsChoice> {
    device_choices(
        settings::audio_input_items(&app.room.audio_devices.input_devices),
        &config.audio.input_device_id,
    )
}

fn output_device_choices(app: &App, config: &config::Config) -> Vec<wire::SettingsChoice> {
    device_choices(
        settings::audio_output_items(&app.room.audio_devices.output_devices),
        &config.audio.output_device_id,
    )
}

fn refresh_audio_device_choices(
    app: &mut App,
    changes: &[wire::SettingsChange],
) -> Vec<wire::SettingsDiagnostic> {
    let mut audio = app
        .rpc_settings
        .as_ref()
        .expect("validated settings session exists")
        .document
        .audio
        .clone();
    let diagnostics = apply_data_changes(changes, |field, value| match field.access {
        FieldAccess::Audio { write, .. } => write(&mut audio, value),
        FieldAccess::Config { .. } => Ok(()),
    });
    if has_errors(&diagnostics) {
        return diagnostics;
    }
    app.refresh_audio_devices_with(
        audio
            .input_buffer
            .to_request(config::DEFAULT_INPUT_TARGET_LATENCY),
        audio
            .output_buffer
            .to_request(config::DEFAULT_OUTPUT_TARGET_LATENCY),
    );
    diagnostics
}

const fn simple_control(kind: u8) -> ControlSpec {
    ControlSpec {
        kind,
        choices: ChoiceSpec::Static(&[]),
        min: None,
        max: None,
        step: None,
        unit: "",
        placeholder: "",
    }
}

const fn number_control(
    min: Option<f32>,
    max: Option<f32>,
    step: Option<f32>,
    unit: &'static str,
) -> ControlSpec {
    ControlSpec {
        kind: wire::CONTROL_NUMBER,
        choices: ChoiceSpec::Static(&[]),
        min,
        max,
        step,
        unit,
        placeholder: "",
    }
}

const fn text_control(kind: u8, placeholder: &'static str) -> ControlSpec {
    ControlSpec {
        kind,
        choices: ChoiceSpec::Static(&[]),
        min: None,
        max: None,
        step: None,
        unit: "",
        placeholder,
    }
}

const fn dynamic_choice_control(
    list: fn(&App, &config::Config) -> Vec<wire::SettingsChoice>,
    refresh: fn(&mut App, &[wire::SettingsChange]) -> Vec<wire::SettingsDiagnostic>,
    placeholder: &'static str,
) -> ControlSpec {
    ControlSpec {
        kind: wire::CONTROL_SEARCHABLE_CHOICE,
        choices: ChoiceSpec::Dynamic { list, refresh },
        min: None,
        max: None,
        step: None,
        unit: "",
        placeholder,
    }
}

const fn choice_control(
    choices: &'static [(&'static str, &'static str)],
) -> ControlSpec {
    ControlSpec {
        kind: wire::CONTROL_CHOICE,
        choices: ChoiceSpec::Static(choices),
        min: None,
        max: None,
        step: None,
        unit: "",
        placeholder: "",
    }
}

macro_rules! audio_spec {
    ($id:literal, $key:literal, $label:literal, $help:literal, $flags:expr,
     $control:expr, $read:expr, $write:expr) => {
        FieldSpec {
            id: wire::SettingsFieldId($id),
            section: 0,
            key: $key,
            label: $label,
            help: $help,
            flags: wire::FIELD_AUDIO | $flags,
            control: $control,
            access: FieldAccess::Audio {
                read: $read,
                write: $write,
            },
        }
    };
}

macro_rules! config_spec {
    ($id:literal, $section:literal, $key:literal, $label:literal, $help:literal,
     $control:expr, $read:expr, $write:expr) => {
        FieldSpec {
            id: wire::SettingsFieldId($id),
            section: $section,
            key: $key,
            label: $label,
            help: $help,
            flags: 0,
            control: $control,
            access: FieldAccess::Config {
                read: $read,
                write: $write,
            },
        }
    };
}

macro_rules! audio_bool {
    ($id:literal, $key:literal, $label:literal, $help:literal, $flags:expr,
     $($path:ident).+) => {
        audio_spec!(
            $id, $key, $label, $help, $flags,
            simple_control(wire::CONTROL_TOGGLE),
            |audio| wire::SettingsValue::Bool(audio.$($path).+),
            |audio, value| {
                audio.$($path).+ = boolean(value)?;
                Ok(())
            }
        )
    };
}

macro_rules! audio_float {
    ($id:literal, $key:literal, $label:literal, $help:literal, $flags:expr,
     $control:expr, $($path:ident).+) => {
        audio_spec!(
            $id, $key, $label, $help, $flags, $control,
            |audio| wire::SettingsValue::Float(audio.$($path).+),
            |audio, value| {
                audio.$($path).+ = float(value)?;
                Ok(())
            }
        )
    };
}

macro_rules! audio_u64 {
    ($id:literal, $key:literal, $label:literal, $unit:literal, $($path:ident).+) => {
        audio_spec!(
            $id, $key, $label, "Advanced live-audio latency tuning.",
            wire::FIELD_ADVANCED, number_control(Some(0.0), None, Some(1.0), $unit),
            |audio| wire::SettingsValue::Unsigned(audio.$($path).+),
            |audio, value| {
                audio.$($path).+ = unsigned(value)?;
                Ok(())
            }
        )
    };
}

macro_rules! config_bool {
    ($id:literal, $section:literal, $key:literal, $label:literal, $help:literal,
     $($path:ident).+) => {
        config_spec!(
            $id, $section, $key, $label, $help,
            simple_control(wire::CONTROL_TOGGLE),
            |config| wire::SettingsValue::Bool(config.$($path).+),
            |config, value| {
                config.$($path).+ = boolean(value)?;
                Ok(())
            }
        )
    };
}

macro_rules! config_float {
    ($id:literal, $section:literal, $key:literal, $label:literal, $help:literal,
     $control:expr, $($path:ident).+) => {
        config_spec!(
            $id, $section, $key, $label, $help, $control,
            |config| wire::SettingsValue::Float(config.$($path).+),
            |config, value| {
                config.$($path).+ = float(value)?;
                Ok(())
            }
        )
    };
}

macro_rules! config_u64 {
    ($id:literal, $section:literal, $key:literal, $label:literal, $help:literal,
     $min:expr, $unit:literal, $($path:ident).+) => {
        config_spec!(
            $id, $section, $key, $label, $help,
            number_control(Some($min), None, Some(1.0), $unit),
            |config| wire::SettingsValue::Unsigned(config.$($path).+),
            |config, value| {
                config.$($path).+ = unsigned(value)?;
                Ok(())
            }
        )
    };
}

const DATA_SECTIONS: &[SectionSpec] = &[
    SectionSpec {
        id: "audio",
        title: "Audio",
        help: "Daemon capture, playback, processing, and live microphone preview.",
    },
    SectionSpec {
        id: "notifications",
        title: "Notifications",
        help: "When notification sounds play and their relative levels.",
    },
    SectionSpec {
        id: "files-history",
        title: "Files & history",
        help: "Incoming file storage, transfer limits, and chat persistence.",
    },
    SectionSpec {
        id: "p2p",
        title: "Peer to peer",
        help: "Direct media connectivity and local-address privacy.",
    },
    SectionSpec {
        id: "web",
        title: "Web",
        help: "Optional local browser view and its access policy.",
    },
];

const DATA_FIELDS: &[FieldSpec] = &[
    audio_spec!(
        1, "audio.input-device-id", "Microphone",
        "System default follows operating-system device changes.", 0,
        dynamic_choice_control(
            input_device_choices,
            refresh_audio_device_choices,
            "Search microphones"
        ),
        |audio| wire::SettingsValue::Text(audio.input_device_id.clone().unwrap_or_default()),
        |audio, value| { audio.input_device_id = optional_text(value)?; Ok(()) }
    ),
    audio_spec!(
        2, "audio.output-device-id", "Speakers",
        "System default follows operating-system device changes.", 0,
        dynamic_choice_control(
            output_device_choices,
            refresh_audio_device_choices,
            "Search output devices"
        ),
        |audio| wire::SettingsValue::Text(audio.output_device_id.clone().unwrap_or_default()),
        |audio, value| { audio.output_device_id = optional_text(value)?; Ok(()) }
    ),
    audio_float!(
        3, "audio.output-volume", "Output volume",
        "Playback gain, from 0 through 130 percent.", 0,
        number_control(Some(0.0), Some(config::MAX_OUTPUT_VOLUME_PERCENT), Some(1.0), "%"),
        output_volume
    ),
    audio_spec!(
        4, "audio.bitrate-bps", "Voice bitrate", "Opus voice bitrate in bits per second.", 0,
        number_control(Some(8_000.0), Some(96_000.0), Some(1_000.0), "bps"),
        |audio| wire::SettingsValue::Signed(i64::from(audio.bitrate_bps)),
        |audio, value| {
            audio.bitrate_bps = i32::try_from(signed(value)?)
                .map_err(|_| "value is outside the supported integer range".to_string())?;
            Ok(())
        }
    ),
    audio_spec!(
        5, "audio.denoise", "Noise suppression",
        "RNNoise is the default high-quality microphone denoiser.", 0,
        choice_control(&[("none", "off"), ("spectral", "spectral"), ("rnnoise", "RNNoise")]),
        |audio| wire::SettingsValue::Text(denoise_name(audio.denoise).into()),
        |audio, value| {
            audio.denoise = match text(value)? {
                "none" => audio::DenoiseConfig::None,
                "spectral" => audio::DenoiseConfig::Spectral,
                "rnnoise" => audio::DenoiseConfig::RnnNoise,
                _ => return Err("unknown noise suppression choice".into()),
            };
            Ok(())
        }
    ),
    audio_spec!(
        6, "audio.dred", "Packet-loss recovery",
        "Adds in-band redundancy that peers can use after packet loss.", wire::FIELD_ADVANCED,
        choice_control(&[("off", "off"), ("auto", "auto"), ("on", "on")]),
        |audio| wire::SettingsValue::Text(dred_name(audio.dred).into()),
        |audio, value| {
            audio.dred = match text(value)? {
                "off" => audio::DredConfig::Off,
                "auto" => audio::DredConfig::Auto,
                "on" => audio::DredConfig::On,
                _ => return Err("unknown packet-loss recovery choice".into()),
            };
            Ok(())
        }
    ),
    audio_bool!(
        7, "audio.echo-cancellation", "Echo cancellation",
        "Useful when speakers can feed back into the microphone.", 0, echo_cancellation
    ),
    audio_float!(
        8, "audio.max-amplification", "Automatic gain ceiling",
        "Maximum adaptive microphone gain in decibels.", 0,
        number_control(
            Some(settings::MAX_AMPLIFICATION_DB_RANGE.0),
            Some(settings::MAX_AMPLIFICATION_DB_RANGE.1),
            Some(1.0), "dB"
        ),
        max_amplification
    ),
    audio_float!(
        9, "audio.denoise-suppression", "RNNoise suppression",
        "Advanced RNNoise shaping.", wire::FIELD_ADVANCED,
        number_control(None, None, Some(0.05), ""), denoise_suppression
    ),
    audio_float!(
        10, "audio.denoise-release", "RNNoise release",
        "Advanced RNNoise shaping.", wire::FIELD_ADVANCED,
        number_control(None, None, Some(0.05), ""), denoise_release
    ),
    audio_bool!(
        11, "audio.denoise-typing-suppression", "Typing suppression",
        "Advanced keyboard-noise gate.", wire::FIELD_ADVANCED, denoise_typing_suppression
    ),
    audio_float!(
        12, "audio.denoise-typing-vad-enter", "Typing VAD enter",
        "Advanced keyboard-noise gate.", wire::FIELD_ADVANCED,
        number_control(Some(0.0), Some(1.0), Some(0.01), ""), denoise_typing_vad_enter
    ),
    audio_float!(
        13, "audio.denoise-typing-vad-release", "Typing VAD release",
        "Advanced keyboard-noise gate.", wire::FIELD_ADVANCED,
        number_control(Some(0.0), Some(1.0), Some(0.01), ""), denoise_typing_vad_release
    ),
    audio_spec!(
        14, "audio.input-buffer.samples", "Input buffer",
        "Use default unless diagnosing a device-period problem.", wire::FIELD_ADVANCED,
        text_control(wire::CONTROL_TEXT, "default or 32-8192"),
        |audio| wire::SettingsValue::Text(buffer_text(audio.input_buffer)),
        |audio, value| { audio.input_buffer = parse_buffer(text(value)?)?; Ok(()) }
    ),
    audio_spec!(
        15, "audio.output-buffer.samples", "Output buffer",
        "Use default unless diagnosing a device-period problem.", wire::FIELD_ADVANCED,
        text_control(wire::CONTROL_TEXT, "default or 32-8192"),
        |audio| wire::SettingsValue::Text(buffer_text(audio.output_buffer)),
        |audio, value| { audio.output_buffer = parse_buffer(text(value)?)?; Ok(()) }
    ),
    audio_bool!(
        16, "audio.latency.capture-silence-gate", "Capture silence gate",
        "Advanced live-audio latency tuning.", wire::FIELD_ADVANCED,
        latency.capture_silence_gate
    ),
    audio_bool!(
        17, "audio.latency.render-assist", "Render assist",
        "Advanced live-audio latency tuning.", wire::FIELD_ADVANCED, latency.render_assist
    ),
    audio_u64!(18, "audio.latency.neteq-start-delay-ms", "NetEq start delay", "ms", latency.neteq_start_delay_ms),
    audio_u64!(19, "audio.latency.neteq-min-delay-ms", "NetEq minimum delay", "ms", latency.neteq_min_delay_ms),
    audio_u64!(20, "audio.latency.neteq-base-minimum-delay-ms", "NetEq base minimum", "ms", latency.neteq_base_minimum_delay_ms),
    audio_u64!(21, "audio.latency.neteq-max-delay-ms", "NetEq maximum delay", "ms", latency.neteq_max_delay_ms),
    audio_u64!(22, "audio.latency.hard-queue-bound-ms", "Hard queue bound", "ms", latency.hard_queue_bound_ms),
    audio_u64!(23, "audio.latency.initial-buffer-ms", "Initial buffer", "ms", latency.initial_buffer_ms),
    audio_u64!(24, "audio.latency.max-reorder-delay-ms", "Maximum reorder delay", "ms", latency.max_reorder_delay_ms),
    audio_u64!(25, "audio.latency.device-period-margin-ms", "Device period margin", "ms", latency.device_period_margin_ms),
    audio_spec!(
        26, "audio.latency.silence-vad-max", "Silence VAD maximum",
        "Advanced live-audio latency tuning.", wire::FIELD_ADVANCED,
        number_control(Some(0.0), Some(255.0), Some(1.0), ""),
        |audio| wire::SettingsValue::Unsigned(u64::from(audio.latency.silence_vad_max)),
        |audio, value| {
            audio.latency.silence_vad_max = u8::try_from(unsigned(value)?)
                .map_err(|_| "value must be between 0 and 255".to_string())?;
            Ok(())
        }
    ),
    audio_u64!(27, "audio.latency.capture-long-silence-stop-ms", "Long-silence stop", "ms", latency.capture_long_silence_stop_ms),
    audio_u64!(28, "audio.latency.capture-silence-preroll-ms", "Silence preroll", "ms", latency.capture_silence_preroll_ms),
    audio_u64!(29, "audio.latency.capture-silence-ramp-ms", "Silence ramp", "ms", latency.capture_silence_ramp_ms),
    config_spec!(
        30, 1, "notifications.sounds", "Play sounds",
        "Deafen always suppresses notification sounds.",
        choice_control(&[("never", "never"), ("in-calls", "in calls"), ("always", "always")]),
        |config| wire::SettingsValue::Text(notification_name(config.notifications.sounds).into()),
        |config, value| {
            config.notifications.sounds = match text(value)? {
                "never" => config::NotificationSoundMode::Never,
                "in-calls" => config::NotificationSoundMode::InCalls,
                "always" => config::NotificationSoundMode::Always,
                _ => return Err("unknown notification sound choice".into()),
            };
            Ok(())
        }
    ),
    config_float!(
        31, 1, "notifications.message-volume-db", "Message volume",
        "Relative level in decibels.",
        number_control(Some(config::MIN_NOTIFICATION_VOLUME_DB), Some(config::MAX_NOTIFICATION_VOLUME_DB), Some(1.0), "dB"),
        notifications.message_volume_db
    ),
    config_float!(
        32, 1, "notifications.peer-join-volume-db", "Peer joined volume",
        "Relative level in decibels.",
        number_control(Some(config::MIN_NOTIFICATION_VOLUME_DB), Some(config::MAX_NOTIFICATION_VOLUME_DB), Some(1.0), "dB"),
        notifications.peer_join_volume_db
    ),
    config_float!(
        33, 1, "notifications.peer-leave-volume-db", "Peer left volume",
        "Relative level in decibels.",
        number_control(Some(config::MIN_NOTIFICATION_VOLUME_DB), Some(config::MAX_NOTIFICATION_VOLUME_DB), Some(1.0), "dB"),
        notifications.peer_leave_volume_db
    ),
    config_spec!(
        34, 2, "files.download", "Incoming files",
        "Reject, keep in memory, or persist incoming files.",
        choice_control(&[("off", "off"), ("memory", "memory"), ("persistent", "persistent")]),
        |config| wire::SettingsValue::Text(download_name(config.files.download).into()),
        |config, value| {
            config.files.download = match text(value)? {
                "off" => config::DownloadMode::Off,
                "memory" => config::DownloadMode::Memory,
                "persistent" => config::DownloadMode::Persistent,
                _ => return Err("unknown download mode".into()),
            };
            Ok(())
        }
    ),
    config_spec!(
        35, 2, "files.download-dir", "Download directory",
        "Empty uses the platform downloads directory.",
        text_control(wire::CONTROL_TEXT, "platform default"),
        |config| wire::SettingsValue::Text(config.files.download_dir.clone()),
        |config, value| { config.files.download_dir = text(value)?.trim().into(); Ok(()) }
    ),
    config_u64!(36, 2, "files.download-memory-mb", "Memory cache", "Shared in-memory ring size in MiB.", 1.0, "MiB", files.download_memory_mb),
    config_u64!(37, 2, "files.max-download-mb", "Maximum download", "Transfer limit in MiB.", 1.0, "MiB", files.max_download_mb),
    config_u64!(38, 2, "files.max-upload-mb", "Maximum upload", "Transfer limit in MiB.", 1.0, "MiB", files.max_upload_mb),
    config_u64!(39, 2, "files.upload-rate-bytes", "Upload rate limit", "Bytes per second; 0 means unlimited.", 0.0, "B/s", files.upload_rate_bytes),
    config_bool!(40, 2, "history.enabled", "Persist chat history", "Applies to future connections.", history.enabled),
    config_spec!(
        41, 2, "history.location", "History location",
        "Empty uses Chatt's platform data directory.",
        text_control(wire::CONTROL_TEXT, "platform default"),
        |config| wire::SettingsValue::Text(config.history.location.clone().unwrap_or_default()),
        |config, value| { config.history.location = optional_text(value)?; Ok(()) }
    ),
    config_bool!(42, 3, "p2p.enabled", "Enable P2P", "Attempts direct media paths before falling back to relay.", p2p.enabled),
    config_spec!(
        43, 3, "p2p.candidate-privacy", "Local-address privacy",
        "mDNS hides literal local IPs from remote peers.",
        choice_control(&[("mdns", "mDNS"), ("ip-address", "IP address"), ("no-host", "no host candidates")]),
        |config| wire::SettingsValue::Text(privacy_name(config.p2p.candidate_privacy).into()),
        |config, value| {
            config.p2p.candidate_privacy = match text(value)? {
                "mdns" => config::CandidatePrivacy::Mdns,
                "ip-address" => config::CandidatePrivacy::Disabled,
                "no-host" => config::CandidatePrivacy::NoHost,
                _ => return Err("unknown candidate privacy choice".into()),
            };
            Ok(())
        }
    ),
    config_bool!(44, 3, "p2p.prefer-ipv6", "Prefer IPv6", "Prefer native IPv6 when candidate quality is otherwise equal.", p2p.prefer_ipv6),
    config_bool!(45, 4, "web.enabled", "Enable browser view", "Starts the optional local browser chat-log server.", web.enabled),
    config_spec!(
        46, 4, "web.bind", "Listen address", "IP address and port, such as 127.0.0.1:8080.",
        text_control(wire::CONTROL_TEXT, "127.0.0.1:8080"),
        |config| wire::SettingsValue::Text(config.web.bind.clone()),
        |config, value| { config.web.bind = text(value)?.trim().into(); Ok(()) }
    ),
    config_spec!(
        47, 4, "web.allowed-origins", "Allowed origins",
        "HTTP(S) origins without paths; empty derives from the bind address.",
        text_control(wire::CONTROL_TEXT_LIST, "https://example.com"),
        |config| wire::SettingsValue::TextList(config.web.allowed_origins.clone()),
        |config, value| {
            config.web.allowed_origins = text_list(value)?
                .iter().map(|origin| origin.trim().to_string())
                .filter(|origin| !origin.is_empty()).collect();
            Ok(())
        }
    ),
    config_bool!(48, 4, "web.readonly", "Read only", "Prevents browser clients from sending messages or files.", web.readonly),
    config_spec!(
        49, 4, "web.autoplay", "Video autoplay", "Browsers may still block autoplay with audio.",
        choice_control(&[("disabled", "off"), ("muted", "muted"), ("with-audio", "with audio")]),
        |config| wire::SettingsValue::Text(autoplay_name(config.web.autoplay).into()),
        |config, value| {
            config.web.autoplay = match text(value)? {
                "disabled" => config::WebAutoplay::Disabled,
                "muted" => config::WebAutoplay::Muted,
                "with-audio" => config::WebAutoplay::WithAudio,
                _ => return Err("unknown web autoplay choice".into()),
            };
            Ok(())
        }
    ),
    config_spec!(
        50, 4, "web.viewer", "File viewer",
        "Open file previews in the side panel or a new browser tab.",
        choice_control(&[("panel", "side panel"), ("tab", "browser tab")]),
        |config| wire::SettingsValue::Text(viewer_name(config.web.viewer).into()),
        |config, value| {
            config.web.viewer = match text(value)? {
                "panel" => config::WebViewer::Panel,
                "tab" => config::WebViewer::Tab,
                _ => return Err("unknown web viewer choice".into()),
            };
            Ok(())
        }
    ),
];

fn data_settings_sections(app: &App, config: &config::Config) -> Vec<wire::SettingsSection> {
    let defaults = config::Config::default();
    DATA_SECTIONS
        .iter()
        .enumerate()
        .map(|(section_index, section)| wire::SettingsSection {
            id: section.id.into(),
            title: section.title.into(),
            help: section.help.into(),
            fields: DATA_FIELDS
                .iter()
                .copied()
                .filter(|field| field.section == section_index)
                .map(|field| wire::SettingsField {
                    id: field.id,
                    key: field.key.into(),
                    label: field.label.into(),
                    help: field.help.into(),
                    flags: field.flags,
                    value: field.read(config),
                    default: field.read(&defaults),
                    control: field.wire_control(app, config),
                })
                .collect(),
        })
        .collect()
}

fn data_field(id: wire::SettingsFieldId) -> Option<FieldSpec> {
    DATA_FIELDS.iter().copied().find(|field| field.id == id)
}

fn data_apply_changes(
    config: &mut config::Config,
    changes: &[wire::SettingsChange],
) -> Vec<wire::SettingsDiagnostic> {
    apply_data_changes(changes, |field, value| field.write(config, value))
}

fn data_apply_audio_changes(
    audio: &mut config::AudioConfig,
    changes: &[wire::SettingsChange],
) -> Vec<wire::SettingsDiagnostic> {
    apply_data_changes(changes, |field, value| field.write_audio(audio, value))
}

fn apply_data_changes(
    changes: &[wire::SettingsChange],
    mut apply: impl FnMut(FieldSpec, &wire::SettingsValue) -> Result<(), String>,
) -> Vec<wire::SettingsDiagnostic> {
    let mut diagnostics = Vec::new();
    let mut seen = HashSet::new();
    for change in changes {
        let Some(field) = data_field(change.field) else {
            diagnostics.push(diagnostic(
                "settings",
                &format!("unknown setting field {}", change.field.0),
            ));
            continue;
        };
        if !seen.insert(field.id) {
            diagnostics.push(diagnostic(
                field.key,
                "field appears more than once in one update",
            ));
            continue;
        }
        if let Err(error) = apply(field, &change.value) {
            diagnostics.push(diagnostic(field.key, &error));
        }
    }
    diagnostics
}

fn validate_settings(config: &config::Config) -> Vec<wire::SettingsDiagnostic> {
    let mut diagnostics = validate_audio(&config.audio);
    for (field, value) in [
        (
            "notifications.message-volume-db",
            config.notifications.message_volume_db,
        ),
        (
            "notifications.peer-join-volume-db",
            config.notifications.peer_join_volume_db,
        ),
        (
            "notifications.peer-leave-volume-db",
            config.notifications.peer_leave_volume_db,
        ),
    ] {
        if !(config::MIN_NOTIFICATION_VOLUME_DB..=config::MAX_NOTIFICATION_VOLUME_DB)
            .contains(&value)
        {
            diagnostics.push(diagnostic(field, "must be between -24 and 12 dB"));
        }
    }
    for (field, value) in [
        ("files.download-memory-mb", config.files.download_memory_mb),
        ("files.max-download-mb", config.files.max_download_mb),
        ("files.max-upload-mb", config.files.max_upload_mb),
    ] {
        if value == 0 {
            diagnostics.push(diagnostic(field, "must be a positive MiB count"));
        }
    }
    if config.web.bind.parse::<std::net::SocketAddr>().is_err() {
        diagnostics.push(diagnostic(
            "web.bind",
            "must be an IP socket address such as 127.0.0.1:8080",
        ));
    }
    for origin in &config.web.allowed_origins {
        if !config::valid_web_origin(origin) {
            diagnostics.push(diagnostic(
                "web.allowed-origins",
                "origins must use http(s) and contain no path",
            ));
        }
    }
    diagnostics
}

fn validate_audio(audio: &config::AudioConfig) -> Vec<wire::SettingsDiagnostic> {
    let mut diagnostics = Vec::new();
    if !(8_000..=96_000).contains(&audio.bitrate_bps) {
        diagnostics.push(diagnostic(
            "audio.bitrate-bps",
            "must be between 8000 and 96000",
        ));
    }
    if !(settings::MAX_AMPLIFICATION_DB_RANGE.0..=settings::MAX_AMPLIFICATION_DB_RANGE.1)
        .contains(&audio.max_amplification)
    {
        diagnostics.push(diagnostic(
            "audio.max-amplification",
            "must be between 0 and 30 dB",
        ));
    }
    if !(0.0..=config::MAX_OUTPUT_VOLUME_PERCENT).contains(&audio.output_volume) {
        diagnostics.push(diagnostic(
            "audio.output-volume",
            "must be between 0 and 130 percent",
        ));
    }
    for (field, buffer) in [
        ("audio.input-buffer.samples", audio.input_buffer),
        ("audio.output-buffer.samples", audio.output_buffer),
    ] {
        if let config::BufferSize::Samples(samples) = buffer
            && !(settings::MIN_BUFFER_SAMPLES..=settings::MAX_BUFFER_SAMPLES).contains(&samples)
        {
            diagnostics.push(diagnostic(field, "must be default or 32-8192 samples"));
        }
    }
    if let Err(error) = audio.latency.to_tuning().validate() {
        diagnostics.push(diagnostic("audio.latency", &error));
    }
    diagnostics
}

fn has_errors(diagnostics: &[wire::SettingsDiagnostic]) -> bool {
    diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == wire::DiagnosticSeverity::Error)
}

fn boolean(value: &wire::SettingsValue) -> Result<bool, String> {
    match value {
        wire::SettingsValue::Bool(value) => Ok(*value),
        _ => Err("expected a boolean value".into()),
    }
}

fn signed(value: &wire::SettingsValue) -> Result<i64, String> {
    match value {
        wire::SettingsValue::Signed(value) => Ok(*value),
        _ => Err("expected a signed integer value".into()),
    }
}

fn unsigned(value: &wire::SettingsValue) -> Result<u64, String> {
    match value {
        wire::SettingsValue::Unsigned(value) => Ok(*value),
        _ => Err("expected an unsigned integer value".into()),
    }
}

fn float(value: &wire::SettingsValue) -> Result<f32, String> {
    match value {
        wire::SettingsValue::Float(value) if value.is_finite() => Ok(*value),
        _ => Err("expected a finite numeric value".into()),
    }
}

fn text(value: &wire::SettingsValue) -> Result<&str, String> {
    match value {
        wire::SettingsValue::Text(value) => Ok(value),
        _ => Err("expected a text value".into()),
    }
}

fn text_list(value: &wire::SettingsValue) -> Result<&[String], String> {
    match value {
        wire::SettingsValue::TextList(value) => Ok(value),
        _ => Err("expected a text-list value".into()),
    }
}

fn optional_text(value: &wire::SettingsValue) -> Result<Option<String>, String> {
    let value = text(value)?.trim();
    Ok((!value.is_empty()).then(|| value.to_string()))
}

fn parse_buffer(value: &str) -> Result<config::BufferSize, String> {
    if value.trim().eq_ignore_ascii_case("default") {
        return Ok(config::BufferSize::Default);
    }
    let samples = value
        .trim()
        .parse::<u32>()
        .map_err(|_| "expected default or a sample count".to_string())?;
    Ok(config::BufferSize::Samples(samples))
}

fn buffer_text(buffer: config::BufferSize) -> String {
    match buffer {
        config::BufferSize::Default => "default".into(),
        config::BufferSize::Samples(samples) => samples.to_string(),
    }
}

fn denoise_name(value: audio::DenoiseConfig) -> &'static str {
    match value {
        audio::DenoiseConfig::None => "none",
        audio::DenoiseConfig::Spectral => "spectral",
        audio::DenoiseConfig::RnnNoise => "rnnoise",
    }
}

fn dred_name(value: audio::DredConfig) -> &'static str {
    match value {
        audio::DredConfig::Off => "off",
        audio::DredConfig::Auto => "auto",
        audio::DredConfig::On => "on",
    }
}

fn notification_name(value: config::NotificationSoundMode) -> &'static str {
    match value {
        config::NotificationSoundMode::Never => "never",
        config::NotificationSoundMode::InCalls => "in-calls",
        config::NotificationSoundMode::Always => "always",
    }
}

fn download_name(value: config::DownloadMode) -> &'static str {
    match value {
        config::DownloadMode::Off => "off",
        config::DownloadMode::Memory => "memory",
        config::DownloadMode::Persistent => "persistent",
    }
}

fn privacy_name(value: config::CandidatePrivacy) -> &'static str {
    match value {
        config::CandidatePrivacy::Mdns => "mdns",
        config::CandidatePrivacy::Disabled => "ip-address",
        config::CandidatePrivacy::NoHost => "no-host",
    }
}

fn autoplay_name(value: config::WebAutoplay) -> &'static str {
    match value {
        config::WebAutoplay::Disabled => "disabled",
        config::WebAutoplay::Muted => "muted",
        config::WebAutoplay::WithAudio => "with-audio",
    }
}

fn viewer_name(value: config::WebViewer) -> &'static str {
    match value {
        config::WebViewer::Panel => "panel",
        config::WebViewer::Tab => "tab",
    }
}

fn diagnostic(field: &str, message: &str) -> wire::SettingsDiagnostic {
    wire::SettingsDiagnostic {
        field: field.to_string(),
        severity: wire::DiagnosticSeverity::Error,
        message: message.to_string(),
    }
}

fn rejected_settings(
    request_id: RequestId,
    operation: Operation,
    code: u16,
    message: &str,
    diagnostics: Vec<wire::SettingsDiagnostic>,
    payload: wire::SettingsResultPayload,
) -> wire::SettingsResult {
    wire::SettingsResult {
        result: RequestResult {
            request_id,
            operation,
            outcome: RequestOutcome::Rejected {
                code,
                message: message.to_string(),
            },
        },
        payload,
        diagnostics,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "chatt-rpc-settings-{label}-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn app_with_missing_config(label: &str) -> App {
        let mut config = config::Config::default();
        let path = temp_config_path(label);
        let _ = std::fs::remove_file(&path);
        config.config_path = Some(path);
        App::new(config, None).unwrap()
    }

    fn opened_session(result: &wire::SettingsResult) -> wire::SettingsSessionId {
        let wire::SettingsResultPayload::Document(document) = &result.payload else {
            panic!("expected settings document")
        };
        document.session_id
    }

    fn field_value(
        result: &wire::SettingsResult,
        field: wire::SettingsFieldId,
    ) -> Option<&wire::SettingsValue> {
        let wire::SettingsResultPayload::Document(document) = &result.payload else {
            return None;
        };
        document
            .sections
            .iter()
            .flat_map(|section| &section.fields)
            .find(|candidate| candidate.id == field)
            .map(|field| &field.value)
    }

    #[test]
    fn rpc_settings_lease_is_idempotent_for_its_owner_and_close_reverts_preview() {
        let mut app = app_with_missing_config("lease");
        let owner = ClientId(41);
        let baseline = app.config.audio.max_amplification;
        let opened = app.handle_rpc_settings(owner, RequestId(1), wire::SettingsCommand::Open);
        let session_id = opened_session(&opened);
        let reopened = app.handle_rpc_settings(owner, RequestId(2), wire::SettingsCommand::Open);
        assert_eq!(opened_session(&reopened), session_id);

        app.handle_rpc_settings(
            owner,
            RequestId(3),
            wire::SettingsCommand::PreviewAudio {
                session_id,
                preview_seq: 1,
                changes: vec![wire::SettingsChange {
                    field: wire::SettingsFieldId(8),
                    value: wire::SettingsValue::Float(baseline + 5.0),
                }],
                loopback: false,
            },
        );
        assert_eq!(app.config.audio.max_amplification, baseline + 5.0);
        app.handle_rpc_settings(
            owner,
            RequestId(4),
            wire::SettingsCommand::Close { session_id },
        );
        assert_eq!(app.config.audio.max_amplification, baseline);
    }

    #[test]
    fn settings_document_is_declarative_and_contains_defaults() {
        let mut app = app_with_missing_config("document");
        let opened =
            app.handle_rpc_settings(ClientId(51), RequestId(1), wire::SettingsCommand::Open);
        opened.validate().unwrap();
        assert_eq!(
            field_value(&opened, wire::SettingsFieldId(46)),
            Some(&wire::SettingsValue::Text("127.0.0.1:8080".into()))
        );
        let wire::SettingsResultPayload::Document(document) = &opened.payload else {
            unreachable!()
        };
        let microphone = document
            .sections
            .iter()
            .flat_map(|section| &section.fields)
            .find(|field| field.id == wire::SettingsFieldId(1))
            .unwrap();
        assert_eq!(
            microphone.control.kind,
            wire::CONTROL_SEARCHABLE_CHOICE
        );
        assert_eq!(microphone.control.choices[0].label, "System default");
        assert!(data_field(microphone.id).unwrap().refresh_choices().is_some());
    }

    #[test]
    fn device_choices_preserve_selection_aliases_and_search_metadata() {
        let choices = device_choices(
            vec![settings::AudioDeviceItem {
                selection: Some("alsa:usb-studio".into()),
                aliases: vec!["legacy-usb".into()],
                backend_id: Some("alsa:usb-studio".into()),
                device_index: Some(3),
                name: "Studio microphone".into(),
                search_text: "Studio microphone USB Audio".into(),
                rank: 10,
                supported: false,
                preview: None,
                issue: Some("unsupported sample format".into()),
                variants: Vec::new(),
                default_source: "OS default input",
            }],
            &Some("legacy-usb".into()),
        );

        assert_eq!(choices[0].value, "legacy-usb");
        assert!(choices[0].detail.contains("unsupported sample format"));
        assert!(choices[0].detail.contains("alsa:usb-studio"));
        assert!(choices[0].search.contains("legacy-usb"));
        assert!(!choices[0].enabled);
    }

    #[test]
    fn force_save_rebases_changes_without_dropping_unexposed_config() {
        let path = temp_config_path("rebase");
        std::fs::write(&path, config::DEFAULT_CONFIG).unwrap();
        let mut config = config::Config::load(Some(path.to_str().unwrap())).unwrap();
        config.config_path = Some(path.clone());
        let mut app = App::new(config, None).unwrap();
        let owner = ClientId(61);
        let opened = app.handle_rpc_settings(owner, RequestId(1), wire::SettingsCommand::Open);
        let session_id = opened_session(&opened);
        let wire::SettingsResultPayload::Document(document) = opened.payload else {
            unreachable!()
        };

        std::fs::write(
            &path,
            format!(
                "{}\n[soundboard]\nenabled = false\nloss = \"congested_wifi\"\nseed = 7\n",
                config::DEFAULT_CONFIG
            ),
        )
        .unwrap();
        let conflict = app.handle_rpc_settings(
            owner,
            RequestId(2),
            wire::SettingsCommand::Save {
                session_id,
                expected_revision: document.revision,
                changes: vec![wire::SettingsChange {
                    field: wire::SettingsFieldId(3),
                    value: wire::SettingsValue::Float(75.0),
                }],
                force: false,
            },
        );
        let wire::SettingsResultPayload::Conflict { latest } = conflict.payload else {
            panic!("external edit should produce a conflict")
        };
        app.handle_rpc_settings(
            owner,
            RequestId(3),
            wire::SettingsCommand::Save {
                session_id,
                expected_revision: latest.revision,
                changes: vec![wire::SettingsChange {
                    field: wire::SettingsFieldId(3),
                    value: wire::SettingsValue::Float(75.0),
                }],
                force: true,
            },
        );

        let saved = config::Config::load(Some(path.to_str().unwrap())).unwrap();
        assert!(!saved.soundboard.enabled);
        assert_eq!(saved.soundboard.seed, 7);
        assert_eq!(saved.audio.output_volume, 75.0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reload_of_a_missing_config_keeps_the_defaults_document_available() {
        let path = temp_config_path("missing");
        let _ = std::fs::remove_file(&path);
        let mut config = config::Config::default();
        config.config_path = Some(path.clone());
        let mut app = App::new(config, None).unwrap();
        let owner = ClientId(71);
        let opened = app.handle_rpc_settings(owner, RequestId(1), wire::SettingsCommand::Open);
        let session_id = opened_session(&opened);
        let reloaded = app.handle_rpc_settings(
            owner,
            RequestId(2),
            wire::SettingsCommand::Reload { session_id },
        );
        let wire::SettingsResultPayload::Document(document) = reloaded.payload else {
            panic!("missing config should reload embedded defaults")
        };
        assert_eq!(document.source, wire::SettingsSourceStatus::Defaults);
    }
}
