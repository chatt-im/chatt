use jsony::Jsony;

use crate::{
    frame::RequestResult,
    ids::{DeviceId, ServerId},
    model::RequestId,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Jsony)]
#[jsony(Binary)]
pub struct ServerDraftId(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum DownloadOverride {
    Inherit,
    Off,
    Memory,
    Persistent,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct ServerDraft {
    pub draft_id: ServerDraftId,
    pub server_id: ServerId,
    pub is_new: bool,
    pub title: String,
    pub label: String,
    pub username: String,
    pub tcp_addr: String,
    pub udp_addr: String,
    pub udp_probe_addr: String,
    pub require_transport_encryption: bool,
    pub can_allow_plaintext: bool,
    pub credential_present: bool,
    pub server_identity: String,
    pub download: DownloadOverride,
    pub download_path: String,
    pub receive_limit_mb: String,
    pub history_enabled: Option<bool>,
    pub history_location: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct ServerCatalogEntry {
    pub server_id: ServerId,
    pub label: String,
    pub username: String,
    pub tcp_addr: String,
    pub require_transport_encryption: bool,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct FieldDiagnostic {
    pub field: String,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum ServerEditOutcome {
    Saved,
    SavedAndJoining,
    ReloadedConflict,
    Missing,
    Rejected,
    SavedButJoinFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum JoinPhase {
    Connecting,
    Authenticating,
    RepairingCredentials,
    AwaitingPlaintextConsent,
    Failed { message: String, retryable: bool },
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct JoinState {
    pub attempt_id: u64,
    pub server_label: String,
    pub phase: JoinPhase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub enum PlaintextPromptTarget {
    Pairing,
    Join { attempt_id: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Jsony)]
#[jsony(Binary, version)]
pub struct DeviceSummary {
    pub device_id: DeviceId,
    pub name: String,
    pub local: bool,
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub enum ManagementCommand {
    OpenServerCatalog,
    EditServer { server_id: ServerId },
    SaveServer { draft: ServerDraft, join: bool },
    DeleteServer { server_id: ServerId },
    Pair {
        input: String,
        device_name: String,
        overwrite_existing_device: bool,
    },
    SubmitPairingPassword { password: String },
    ResolvePlaintext { target: PlaintextPromptTarget, accept: bool },
    CancelPairing,
    RetryJoin { attempt_id: u64 },
    CancelJoin { attempt_id: u64 },
    ListDevices,
    CreateDeviceLink,
    CancelDeviceLink,
    RevokeDevice { device_id: DeviceId },
}

#[derive(Clone, Debug, PartialEq, Jsony)]
#[jsony(Binary, version)]
pub enum ManagementEvent {
    ServerCatalog {
        request_id: RequestId,
        servers: Vec<ServerCatalogEntry>,
    },
    ServerDraft {
        request_id: Option<RequestId>,
        draft: ServerDraft,
        diagnostics: Vec<FieldDiagnostic>,
    },
    ServerEditResult {
        result: RequestResult,
        outcome: ServerEditOutcome,
        draft: Option<ServerDraft>,
        diagnostics: Vec<FieldDiagnostic>,
    },
    Join(JoinState),
    PairingPasswordRequired { retry: bool },
    PlaintextConsentRequired {
        label: String,
        target: PlaintextPromptTarget,
    },
    PairingFailed { message: String },
    PairingIdentityExists { message: String },
    Devices {
        request_id: Option<RequestId>,
        devices: Vec<DeviceSummary>,
    },
    DeviceLink {
        pairing_string: String,
        expires_at_ms: u64,
    },
    DeviceLinkCanceled,
    DeviceLinked { device: DeviceSummary },
    Status { message: String, error: bool },
}

impl ManagementCommand {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::SaveServer { draft, .. } => draft.validate(),
            Self::Pair {
                input,
                device_name,
                ..
            } => {
                check_text(input, "pairing input")?;
                if !device_name.is_empty() {
                    check_text(device_name, "device name")?;
                }
                Ok(())
            }
            Self::SubmitPairingPassword { password } => {
                check_text(password, "pairing password")
            }
            Self::ResolvePlaintext {
                target: PlaintextPromptTarget::Join { attempt_id: 0 },
                ..
            }
            | Self::RetryJoin { attempt_id: 0 }
            | Self::CancelJoin { attempt_id: 0 } => {
                Err("join attempt id must be nonzero".into())
            }
            Self::EditServer { server_id }
            | Self::DeleteServer { server_id }
                if server_id.0 == [0; 16] =>
            {
                Err("server id must be nonzero".into())
            }
            Self::RevokeDevice { device_id } if device_id.0 == [0; 16] => {
                Err("device id must be nonzero".into())
            }
            _ => Ok(()),
        }
    }
}

impl ServerDraft {
    pub fn validate(&self) -> Result<(), String> {
        if self.draft_id.0 == 0 || self.server_id.0 == [0; 16] {
            return Err("server draft identity must be nonzero".into());
        }
        for (value, label) in [
            (&self.title, "server draft title"),
            (&self.label, "server label"),
            (&self.username, "server username"),
            (&self.tcp_addr, "server TCP address"),
            (&self.udp_addr, "server UDP address"),
            (&self.server_identity, "server identity"),
        ] {
            check_text(value, label)?;
        }
        for value in [
            &self.udp_probe_addr,
            &self.download_path,
            &self.receive_limit_mb,
            &self.history_location,
        ] {
            if !value.is_empty() {
                check_text(value, "server field")?;
            }
        }
        Ok(())
    }
}

impl ManagementEvent {
    pub fn request_id(&self) -> Option<RequestId> {
        match self {
            Self::ServerCatalog { request_id, .. } => Some(*request_id),
            Self::ServerDraft { request_id, .. } | Self::Devices { request_id, .. } => *request_id,
            Self::ServerEditResult { result, .. } => Some(result.request_id),
            _ => None,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::ServerCatalog {
                request_id,
                servers,
            } => {
                check_request(*request_id)?;
                if servers.len() > crate::MAX_SERVERS {
                    return Err("server catalog exceeds limit".into());
                }
                for server in servers {
                    if server.server_id.0 == [0; 16] {
                        return Err("server id must be nonzero".into());
                    }
                    check_text(&server.label, "server label")?;
                    check_text(&server.username, "server username")?;
                    check_text(&server.tcp_addr, "server TCP address")?;
                }
                Ok(())
            }
            Self::ServerDraft {
                request_id,
                draft,
                diagnostics,
            } => {
                if let Some(request_id) = request_id {
                    check_request(*request_id)?;
                }
                draft.validate()?;
                validate_diagnostics(diagnostics)
            }
            Self::ServerEditResult {
                result,
                draft,
                diagnostics,
                ..
            } => {
                check_request(result.request_id)?;
                if result.operation != crate::frame::Operation::Management {
                    return Err("server result carries the wrong operation".into());
                }
                if let crate::frame::RequestOutcome::Rejected { message, .. } = &result.outcome {
                    check_text(message, "server edit rejection")?;
                }
                if let Some(draft) = draft {
                    draft.validate()?;
                }
                validate_diagnostics(diagnostics)
            }
            Self::Join(join) => {
                if join.attempt_id == 0 {
                    return Err("join attempt id must be nonzero".into());
                }
                check_text(&join.server_label, "join server label")?;
                if let JoinPhase::Failed { message, .. } = &join.phase {
                    check_text(message, "join failure")?;
                }
                Ok(())
            }
            Self::PairingFailed { message }
            | Self::PairingIdentityExists { message }
            | Self::Status { message, .. } => check_text(message, "management message"),
            Self::PlaintextConsentRequired { label, .. } => {
                check_text(label, "plaintext server label")
            }
            Self::Devices {
                request_id,
                devices,
            } => {
                if let Some(request_id) = request_id {
                    check_request(*request_id)?;
                }
                if devices.len() > crate::MAX_DEVICES {
                    return Err("device collection exceeds limit".into());
                }
                for device in devices {
                    if device.device_id.0 == [0; 16] {
                        return Err("device id must be nonzero".into());
                    }
                    check_text(&device.name, "device name")?;
                }
                Ok(())
            }
            Self::DeviceLink {
                pairing_string,
                expires_at_ms,
            } => {
                check_text(pairing_string, "device link")?;
                if *expires_at_ms == 0 {
                    return Err("device link expiry must be nonzero".into());
                }
                Ok(())
            }
            Self::DeviceLinked { device } => {
                if device.device_id.0 == [0; 16] {
                    return Err("device id must be nonzero".into());
                }
                check_text(&device.name, "device name")
            }
            Self::PairingPasswordRequired { .. } | Self::DeviceLinkCanceled => Ok(()),
        }
    }
}

fn validate_diagnostics(diagnostics: &[FieldDiagnostic]) -> Result<(), String> {
    if diagnostics.len() > crate::MAX_MANAGEMENT_DIAGNOSTICS {
        return Err("management diagnostics exceed limit".into());
    }
    for diagnostic in diagnostics {
        check_text(&diagnostic.field, "diagnostic field")?;
        check_text(&diagnostic.message, "diagnostic message")?;
    }
    Ok(())
}

fn check_request(request_id: RequestId) -> Result<(), String> {
    if request_id.0 == 0 {
        Err("request id must be nonzero".into())
    } else {
        Ok(())
    }
}

fn check_text(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("{label} must not be empty"));
    }
    if value.len() > crate::MAX_STRING_BYTES {
        return Err(format!("{label} exceeds limit"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{
        ClientFrame, DaemonFrame, Operation, RequestOutcome, decode_client, decode_daemon,
        encode_client, encode_daemon,
    };

    fn draft() -> ServerDraft {
        ServerDraft {
            draft_id: ServerDraftId(3),
            server_id: ServerId([4; 16]),
            is_new: false,
            title: "Edit Server work".into(),
            label: "work".into(),
            username: "alice".into(),
            tcp_addr: "chat.example:443".into(),
            udp_addr: "chat.example:444".into(),
            udp_probe_addr: String::new(),
            require_transport_encryption: true,
            can_allow_plaintext: false,
            credential_present: true,
            server_identity: "public-identity".into(),
            download: DownloadOverride::Memory,
            download_path: String::new(),
            receive_limit_mb: "50".into(),
            history_enabled: Some(false),
            history_location: String::new(),
        }
    }

    #[test]
    fn management_frames_round_trip_as_typed_documents() {
        let command = ClientFrame::Management {
            request_id: RequestId(7),
            command: ManagementCommand::SaveServer {
                draft: draft(),
                join: true,
            },
        };
        assert_eq!(decode_client(&encode_client(&command).unwrap()).unwrap(), command);

        let event = DaemonFrame::Management(ManagementEvent::ServerEditResult {
            result: RequestResult {
                request_id: RequestId(7),
                operation: Operation::Management,
                outcome: RequestOutcome::Accepted,
            },
            outcome: ServerEditOutcome::SavedAndJoining,
            draft: None,
            diagnostics: Vec::new(),
        });
        assert_eq!(decode_daemon(&encode_daemon(&event).unwrap()).unwrap(), event);
    }

    #[test]
    fn rejects_invalid_management_identities_and_unbounded_diagnostics() {
        let command = ClientFrame::Management {
            request_id: RequestId(1),
            command: ManagementCommand::RetryJoin { attempt_id: 0 },
        };
        assert!(encode_client(&command).is_err());

        let event = DaemonFrame::Management(ManagementEvent::ServerDraft {
            request_id: Some(RequestId(1)),
            draft: draft(),
            diagnostics: vec![
                FieldDiagnostic {
                    field: "label".into(),
                    message: "invalid".into(),
                };
                crate::MAX_MANAGEMENT_DIAGNOSTICS + 1
            ],
        });
        assert!(encode_daemon(&event).is_err());

        let event = DaemonFrame::Management(ManagementEvent::ServerEditResult {
            result: RequestResult {
                request_id: RequestId(0),
                operation: Operation::Management,
                outcome: RequestOutcome::Accepted,
            },
            outcome: ServerEditOutcome::Saved,
            draft: None,
            diagnostics: Vec::new(),
        });
        assert!(encode_daemon(&event).is_err());
    }
}
