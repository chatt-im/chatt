//! Renderer-neutral daemon settings.
//!
//! The wire shape is deliberately independent of Chatt's Rust configuration
//! structs. New settings are additional rows using the existing value/control
//! vocabulary, so renderers do not need to be rebuilt when the daemon exposes
//! another field.

use jsony::Jsony;

use crate::{
    MAX_SETTINGS_CHANGES, MAX_SETTINGS_CHOICES, MAX_SETTINGS_DIAGNOSTICS,
    MAX_SETTINGS_FIELDS, MAX_SETTINGS_LIST_ITEMS, MAX_SETTINGS_SECTIONS,
    frame::{Operation, RequestOutcome, RequestResult},
    model::{RequestId, check_nonempty_string, check_string},
};

pub const CONTROL_TOGGLE: u8 = 1;
pub const CONTROL_NUMBER: u8 = 2;
pub const CONTROL_TEXT: u8 = 3;
pub const CONTROL_TEXT_LIST: u8 = 4;
pub const CONTROL_CHOICE: u8 = 5;
pub const CONTROL_SEARCHABLE_CHOICE: u8 = 6;

pub const FIELD_ADVANCED: u8 = 1 << 0;
pub const FIELD_AUDIO: u8 = 1 << 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Jsony)]
#[jsony(Binary, version)]
pub struct SettingsSessionId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Jsony)]
#[jsony(Binary, version)]
pub struct SettingsFieldId(pub u16);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum SettingsSourceStatus {
    Defaults,
    File,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum DiagnosticSeverity {
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct SettingsDiagnostic {
    pub field: String,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

impl SettingsDiagnostic {
    fn validate(&self) -> Result<(), String> {
        check_nonempty_string(&self.field)?;
        check_nonempty_string(&self.message)
    }
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub enum SettingsValue {
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    Float(f32),
    Text(String),
    TextList(Vec<String>),
}

impl SettingsValue {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Float(value) if !value.is_finite() => {
                Err("setting contains a non-finite number".into())
            }
            Self::Text(value) => check_string(value),
            Self::TextList(values) => {
                if values.len() > MAX_SETTINGS_LIST_ITEMS {
                    return Err("setting list exceeds limit".into());
                }
                for value in values {
                    check_nonempty_string(value)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct SettingsChoice {
    pub value: String,
    pub label: String,
    pub detail: String,
    pub search: String,
    pub enabled: bool,
}

impl SettingsChoice {
    fn validate(&self) -> Result<(), String> {
        check_string(&self.value)?;
        check_nonempty_string(&self.label)?;
        check_string(&self.detail)?;
        check_string(&self.search)
    }
}

/// Generic editor metadata. `kind` is numeric so an older renderer can skip a
/// row carrying a future control without failing to decode the document.
#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub struct SettingsControl {
    pub kind: u8,
    pub choices: Vec<SettingsChoice>,
    pub min: Option<f32>,
    pub max: Option<f32>,
    pub step: Option<f32>,
    pub unit: String,
    pub placeholder: String,
}

impl SettingsControl {
    fn validate(&self) -> Result<(), String> {
        if self.kind == 0 {
            return Err("setting control kind must be nonzero".into());
        }
        if self.choices.len() > MAX_SETTINGS_CHOICES {
            return Err("setting choice collection exceeds limit".into());
        }
        for choice in &self.choices {
            choice.validate()?;
        }
        for value in [self.min, self.max, self.step].into_iter().flatten() {
            if !value.is_finite() {
                return Err("setting control contains a non-finite number".into());
            }
        }
        if self.min.zip(self.max).is_some_and(|(min, max)| min > max) {
            return Err("setting control range is inverted".into());
        }
        if self.step.is_some_and(|step| step <= 0.0) {
            return Err("setting control step must be positive".into());
        }
        check_string(&self.unit)?;
        check_string(&self.placeholder)
    }
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub struct SettingsField {
    pub id: SettingsFieldId,
    pub key: String,
    pub label: String,
    pub help: String,
    pub flags: u8,
    pub value: SettingsValue,
    pub default: SettingsValue,
    pub control: SettingsControl,
}

impl SettingsField {
    fn validate(&self) -> Result<(), String> {
        if self.id.0 == 0 {
            return Err("setting field id must be nonzero".into());
        }
        check_nonempty_string(&self.key)?;
        check_nonempty_string(&self.label)?;
        check_string(&self.help)?;
        self.value.validate()?;
        self.default.validate()?;
        self.control.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub struct SettingsSection {
    pub id: String,
    pub title: String,
    pub help: String,
    pub fields: Vec<SettingsField>,
}

impl SettingsSection {
    fn validate(&self) -> Result<(), String> {
        check_nonempty_string(&self.id)?;
        check_nonempty_string(&self.title)?;
        check_string(&self.help)?;
        if self.fields.len() > MAX_SETTINGS_FIELDS {
            return Err("settings field collection exceeds limit".into());
        }
        for field in &self.fields {
            field.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct SettingsActions {
    pub audio_preview: bool,
    pub audio_loopback: bool,
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub struct AudioRuntimeState {
    pub preview_active: bool,
    pub loopback: bool,
    pub applying: bool,
    pub preview_seq: u64,
    pub diagnostics: Vec<SettingsDiagnostic>,
}

impl AudioRuntimeState {
    fn validate(&self) -> Result<(), String> {
        validate_diagnostics(&self.diagnostics)
    }
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub struct SettingsDocument {
    pub session_id: SettingsSessionId,
    pub revision: u64,
    pub source: SettingsSourceStatus,
    pub sections: Vec<SettingsSection>,
    pub actions: SettingsActions,
    pub audio_runtime: AudioRuntimeState,
    pub diagnostics: Vec<SettingsDiagnostic>,
}

impl SettingsDocument {
    fn validate(&self) -> Result<(), String> {
        validate_session_id(self.session_id)?;
        if self.revision == 0 {
            return Err("settings revision must be nonzero".into());
        }
        if self.sections.len() > MAX_SETTINGS_SECTIONS {
            return Err("settings section collection exceeds limit".into());
        }
        let mut ids = std::collections::HashSet::new();
        for section in &self.sections {
            section.validate()?;
            for field in &section.fields {
                if !ids.insert(field.id) {
                    return Err("settings document contains duplicate field ids".into());
                }
            }
        }
        self.audio_runtime.validate()?;
        validate_diagnostics(&self.diagnostics)
    }
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub struct SettingsChange {
    pub field: SettingsFieldId,
    pub value: SettingsValue,
}

impl SettingsChange {
    fn validate(&self) -> Result<(), String> {
        if self.field.0 == 0 {
            return Err("setting change field id must be nonzero".into());
        }
        self.value.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub enum SettingsCommand {
    Open,
    SetAudioPreviewActive {
        session_id: SettingsSessionId,
        active: bool,
    },
    PreviewAudio {
        session_id: SettingsSessionId,
        preview_seq: u64,
        changes: Vec<SettingsChange>,
        loopback: bool,
    },
    RefreshChoices {
        session_id: SettingsSessionId,
        field: SettingsFieldId,
        changes: Vec<SettingsChange>,
    },
    Reload {
        session_id: SettingsSessionId,
    },
    Save {
        session_id: SettingsSessionId,
        expected_revision: u64,
        changes: Vec<SettingsChange>,
        force: bool,
    },
    Close {
        session_id: SettingsSessionId,
    },
}

impl SettingsCommand {
    pub fn operation(&self) -> Operation {
        match self {
            Self::Open => Operation::OpenSettings,
            Self::SetAudioPreviewActive { .. } => Operation::SetAudioPreviewActive,
            Self::PreviewAudio { .. } => Operation::PreviewAudioSettings,
            Self::RefreshChoices { .. } => Operation::RefreshSettingsChoices,
            Self::Reload { .. } => Operation::ReloadSettings,
            Self::Save { .. } => Operation::SaveSettings,
            Self::Close { .. } => Operation::CloseSettings,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        let (session_id, changes) = match self {
            Self::Open => return Ok(()),
            Self::SetAudioPreviewActive { session_id, .. }
            | Self::Reload { session_id }
            | Self::Close { session_id } => (*session_id, &[][..]),
            Self::PreviewAudio {
                session_id,
                preview_seq,
                changes,
                ..
            } => {
                if *preview_seq == 0 {
                    return Err("audio preview sequence must be nonzero".into());
                }
                (*session_id, changes.as_slice())
            }
            Self::RefreshChoices {
                session_id,
                field,
                changes,
            } => {
                if field.0 == 0 {
                    return Err("choice refresh field id must be nonzero".into());
                }
                (*session_id, changes.as_slice())
            }
            Self::Save {
                session_id,
                changes,
                ..
            } => (*session_id, changes.as_slice()),
        };
        validate_session_id(session_id)?;
        if changes.len() > MAX_SETTINGS_CHANGES {
            return Err("setting change collection exceeds limit".into());
        }
        for change in changes {
            change.validate()?;
        }
        if let Self::Save {
            expected_revision, ..
        } = self
            && *expected_revision == 0
        {
            return Err("settings revision must be nonzero".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub enum SettingsResultPayload {
    None,
    Document(SettingsDocument),
    PreviewApplied {
        session_id: SettingsSessionId,
        runtime: AudioRuntimeState,
    },
    Conflict {
        latest: SettingsDocument,
    },
    Closed {
        session_id: SettingsSessionId,
    },
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub struct SettingsResult {
    pub result: RequestResult,
    pub payload: SettingsResultPayload,
    pub diagnostics: Vec<SettingsDiagnostic>,
}

impl SettingsResult {
    pub fn accepted(
        request_id: RequestId,
        operation: Operation,
        payload: SettingsResultPayload,
    ) -> Self {
        Self {
            result: RequestResult {
                request_id,
                operation,
                outcome: RequestOutcome::Accepted,
            },
            payload,
            diagnostics: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.result.request_id.0 == 0 {
            return Err("request id must be nonzero".into());
        }
        if !matches!(
            self.result.operation,
            Operation::OpenSettings
                | Operation::SetAudioPreviewActive
                | Operation::PreviewAudioSettings
                | Operation::RefreshSettingsChoices
                | Operation::ReloadSettings
                | Operation::SaveSettings
                | Operation::CloseSettings
        ) {
            return Err("settings result carries the wrong operation".into());
        }
        if let RequestOutcome::Rejected { message, .. } = &self.result.outcome {
            check_nonempty_string(message)?;
        }
        match &self.payload {
            SettingsResultPayload::Document(document) => document.validate()?,
            SettingsResultPayload::PreviewApplied {
                session_id,
                runtime,
            } => {
                validate_session_id(*session_id)?;
                runtime.validate()?;
            }
            SettingsResultPayload::Conflict { latest } => latest.validate()?,
            SettingsResultPayload::Closed { session_id } => validate_session_id(*session_id)?,
            SettingsResultPayload::None => {}
        }
        validate_diagnostics(&self.diagnostics)
    }
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub enum SettingsEvent {
    Document(SettingsDocument),
    AudioMeter {
        session_id: SettingsSessionId,
        rms: f32,
        peak: f32,
        voice_active: bool,
    },
    AudioRuntime {
        session_id: SettingsSessionId,
        runtime: AudioRuntimeState,
    },
}

impl SettingsEvent {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Document(document) => document.validate(),
            Self::AudioMeter {
                session_id,
                rms,
                peak,
                ..
            } => {
                validate_session_id(*session_id)?;
                if !rms.is_finite() || !peak.is_finite() || *rms < 0.0 || *peak < 0.0 {
                    return Err("audio meter contains an invalid level".into());
                }
                Ok(())
            }
            Self::AudioRuntime {
                session_id,
                runtime,
            } => {
                validate_session_id(*session_id)?;
                runtime.validate()
            }
        }
    }
}

fn validate_session_id(session_id: SettingsSessionId) -> Result<(), String> {
    if session_id.0 == 0 {
        Err("settings session id must be nonzero".into())
    } else {
        Ok(())
    }
}

fn validate_diagnostics(diagnostics: &[SettingsDiagnostic]) -> Result<(), String> {
    if diagnostics.len() > MAX_SETTINGS_DIAGNOSTICS {
        return Err("settings diagnostic collection exceeds limit".into());
    }
    for diagnostic in diagnostics {
        diagnostic.validate()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{ClientFrame, decode_client, encode_client};

    #[test]
    fn declarative_changes_round_trip_without_a_config_schema() {
        let frame = ClientFrame::Settings {
            request_id: RequestId(7),
            command: SettingsCommand::Save {
                session_id: SettingsSessionId(3),
                expected_revision: 9,
                changes: vec![SettingsChange {
                    field: SettingsFieldId(41),
                    value: SettingsValue::Bool(true),
                }],
                force: false,
            },
        };
        assert_eq!(
            decode_client(&encode_client(&frame).unwrap()).unwrap(),
            frame
        );

        let frame = ClientFrame::Settings {
            request_id: RequestId(8),
            command: SettingsCommand::RefreshChoices {
                session_id: SettingsSessionId(3),
                field: SettingsFieldId(2),
                changes: Vec::new(),
            },
        };
        assert_eq!(
            decode_client(&encode_client(&frame).unwrap()).unwrap(),
            frame
        );
    }
}
