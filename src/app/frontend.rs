use local_rpc::{
    bulk::BeginAttachmentRead,
    bulk::BeginUpload,
    frame::{ClientFrame, Operation, RequestOutcome, RequestResult},
    ids::RoomId,
    model::{
        AttachmentDescriptor, AttachmentId, CommandCandidate, CommandCandidateKind,
        CommandOutputLine, ConnectionState, MediaKind, Message, RequestId, RoomKind, RoomSnapshot,
        RoomSummary, ServerAvailability, ServerSelectionState, ServerSummary, StateSnapshot,
        TrustState, VoiceSessionState, VoiceState,
    },
};

use crate::{client_channel::ClientId, client_net::NetworkCommand};

use super::{App, room::ClientRoomKind};

pub(crate) struct RpcLiveShareOpen {
    pub(crate) handle: crate::video::NativeViewerHandle,
    pub(crate) status: local_rpc::model::LiveShareViewStatus,
}

pub(crate) enum RpcCommandEffect {
    Reply(RequestResult),
    Snapshot(RequestId),
    Pong(RequestId, u64),
    Disconnect(RequestResult),
    BeginRead {
        result: RequestResult,
        read: BeginAttachmentRead,
        descriptor: AttachmentDescriptor,
        source: crate::receive_store::Source,
    },
    OpenAttachmentSource {
        request_id: RequestId,
        room_id: RoomId,
        attachment_id: AttachmentId,
        descriptor: AttachmentDescriptor,
        source: crate::receive_store::Source,
    },
    BeginUpload {
        request_id: RequestId,
        upload: BeginUpload,
    },
    CommandResult {
        result: RequestResult,
        lines: Vec<CommandOutputLine>,
    },
    CommandCandidates {
        request_id: RequestId,
        kind: CommandCandidateKind,
        items: Vec<CommandCandidate>,
    },
    MessageReferenceResolved {
        request_id: RequestId,
        room_id: RoomId,
        room_generation: u64,
        message_id: rpc::ids::MessageId,
        message: Option<Message>,
    },
    None,
}

pub(crate) struct RpcHistoryPage {
    pub messages: Vec<Message>,
    pub room_generation: u64,
    pub older_cursor: Option<rpc::ids::MessageId>,
    pub at_start: bool,
}

impl App {
    pub(crate) fn register_rpc_client(&mut self, client_id: ClientId) {
        self.rpc_clients.insert(client_id);
        if let Some(room_id) = self.room.viewed_room {
            self.room.prepare_client_view(client_id, room_id);
        }
    }

    pub(crate) fn reconcile_rpc_client_views(&mut self) {
        let Some(fallback_room) = self.room.viewed_room else {
            return;
        };
        for &client_id in &self.rpc_clients {
            let selection_is_accessible = self
                .room
                .selected_room_for(client_id)
                .is_some_and(|room_id| self.room.room_meta(room_id).is_some());
            if !selection_is_accessible {
                self.room.prepare_client_view(client_id, fallback_room);
            }
        }
    }

    pub(crate) fn rpc_snapshot(&self, client_id: ClientId) -> StateSnapshot {
        self.rpc_snapshot_inner(client_id, true)
    }

    pub(crate) fn rpc_projection_state(&self, client_id: ClientId) -> StateSnapshot {
        self.rpc_snapshot_inner(client_id, false)
    }

    fn rpc_snapshot_inner(&self, client_id: ClientId, include_history: bool) -> StateSnapshot {
        let issue = self
            .rpc_server_selection_issue
            .as_ref()
            .filter(|issue| issue.owner == client_id)
            .map(|issue| issue.issue.clone());
        let server_selection = ServerSelectionState {
            servers: self
                .config
                .servers
                .iter()
                .map(|server| ServerSummary {
                    label: server.label.clone(),
                    username: server.username.clone(),
                    tcp_addr: server.tcp_addr.clone(),
                    require_transport_encryption: server.require_transport_encryption,
                    availability: if server
                        .token
                        .starts_with(rpc::crypto::OPEN_PAIR_RECOVERY_PREFIX)
                    {
                        ServerAvailability::PairingIncomplete
                    } else {
                        ServerAvailability::Ready
                    },
                })
                .collect(),
            error: issue.as_ref().and_then(|issue| match issue {
                super::RpcServerSelectionIssue::Error(error) => Some(error.clone()),
                super::RpcServerSelectionIssue::Prompt(_) => None,
            }),
            prompt: issue.and_then(|issue| match issue {
                super::RpcServerSelectionIssue::Error(_) => None,
                super::RpcServerSelectionIssue::Prompt(prompt) => Some(prompt),
            }),
        };
        let selected_room = self.room.selected_room_for(client_id);
        let rooms = self
            .room
            .room_metas()
            .map(|(id, meta)| RoomSummary {
                id,
                name: meta.name.clone(),
                kind: match meta.kind {
                    ClientRoomKind::Public => RoomKind::Public,
                    ClientRoomKind::Private { .. } => RoomKind::Private,
                    ClientRoomKind::Dm { .. } => RoomKind::Direct,
                },
                unread: meta.unread,
                behind_head: meta.unread == 0 && meta.head > meta.last_read,
                voice_active: !meta.voice_users.is_empty(),
                trust: match (&meta.kind, self.room.e2e_trust_state(id)) {
                    (
                        ClientRoomKind::Dm { .. },
                        Some(super::room::DmTrustState::Verified { .. }),
                    ) => TrustState::Verified,
                    (
                        ClientRoomKind::Dm { .. },
                        Some(super::room::DmTrustState::Accepted {
                            change_from: Some(_),
                            ..
                        }),
                    ) => TrustState::Changed,
                    (ClientRoomKind::Dm { .. }, _) => TrustState::Unverified,
                    _ => TrustState::NotApplicable,
                },
            })
            .collect();
        let room = selected_room.map(|room_id| {
            if include_history {
                self.rpc_room_snapshot(room_id)
            } else {
                self.rpc_room_projection(room_id)
            }
        });
        let mut live_shares = self
            .room
            .available_shares
            .iter()
            .map(|(stream_id, share)| local_rpc::model::LiveShare {
                room_id: share.room_id,
                stream_id: *stream_id,
                generation: share.generation,
                sender_name: share.sender_name.clone(),
                codec: share.codec.clone(),
                coded_width: share.coded_width,
                coded_height: share.coded_height,
                extradata: share.extradata.clone(),
            })
            .collect::<Vec<_>>();
        live_shares.sort_by_key(|share| share.stream_id);
        StateSnapshot {
            connection: if self.user_id.is_some() {
                ConnectionState::Online
            } else if self.network.is_some() {
                ConnectionState::Connecting
            } else {
                ConnectionState::Offline
            },
            active_server: self.room.active_server_label.clone().or_else(|| {
                (!self.room.server_alias.is_empty()).then(|| self.room.server_alias.clone())
            }),
            server_selection,
            local_identity: (!self.room.local_username.is_empty())
                .then(|| self.room.local_username.clone()),
            rooms,
            selected_room,
            room,
            voice: VoiceSessionState {
                state: rpc_voice_state(self.local_voice_state()),
                output_volume: self.config.audio.output_volume,
                joined_room: self.room.voice_room,
            },
            transfers: selected_room.map_or_else(Vec::new, |room_id| {
                self.room.rpc_transfer_summaries(room_id)
            }),
            live_shares,
        }
    }

    pub(crate) fn start_rpc_live_share(
        &mut self,
        client_id: ClientId,
        stream_id: rpc::ids::StreamId,
        generation: u64,
        stream: std::os::unix::net::UnixStream,
    ) -> Result<RpcLiveShareOpen, String> {
        let share = self
            .room
            .available_shares
            .get(&stream_id)
            .ok_or_else(|| "that screen share is no longer available".to_string())?;
        if share.generation != generation {
            return Err("that screen share has been replaced".into());
        }
        if self.room.voice_room != Some(share.room_id) {
            return Err("join the share's voice room before viewing".into());
        }
        let view_secret = share.view_secret.clone();
        let own_share = self.screencast_stream_id == Some(stream_id);
        let session_id = self.session_id;
        let video_transport = self.video_transport;
        let upstream_is_active = self.subscribers.contains_key(&stream_id);
        let wait_for_upstream_bootstrap = !own_share && !upstream_is_active;
        if !own_share && !upstream_is_active {
            if session_id.is_none() {
                return Err("the voice session is no longer active".into());
            }
            if video_transport.is_none() {
                return Err("video transport is not ready".into());
            }
        }
        let handle = self.video_fanout.add_native(
            client_id.0 as u64,
            stream_id,
            stream,
            wait_for_upstream_bootstrap,
        )?;
        if own_share {
            return Ok(RpcLiveShareOpen {
                handle,
                status: local_rpc::model::LiveShareViewStatus::WaitingForKeyframe,
            });
        }
        if let Some(subscriber) = self.subscribers.get(&stream_id) {
            return Ok(RpcLiveShareOpen {
                handle,
                status: rpc_live_share_status(subscriber.view_state()),
            });
        }
        let session_id = session_id.expect("validated for a new remote live share subscriber");
        let video_transport =
            video_transport.expect("validated for a new remote live share subscriber");
        let subscriber = crate::video::start_subscriber(
            session_id,
            stream_id,
            generation,
            view_secret,
            video_transport,
            self.video_fanout.clone(),
            self.events.sender(),
        )?;
        let status = rpc_live_share_status(subscriber.view_state());
        self.subscribers.insert(stream_id, subscriber);
        Ok(RpcLiveShareOpen { handle, status })
    }

    pub(crate) fn stop_rpc_live_share(&mut self, stream_id: rpc::ids::StreamId) {
        if self.video_fanout.has_native(stream_id) || self.web_viewing_shares.contains(&stream_id) {
            return;
        }
        if let Some(mut subscriber) = self.subscribers.remove(&stream_id) {
            subscriber.stop();
        }
    }

    pub(crate) fn is_current_live_share(
        &self,
        stream_id: rpc::ids::StreamId,
        generation: u64,
    ) -> bool {
        self.room
            .available_shares
            .get(&stream_id)
            .is_some_and(|share| share.generation == generation)
    }

    pub(crate) fn rpc_room_snapshot(&self, room_id: RoomId) -> RoomSnapshot {
        let page = self
            .room
            .resident_message_page(
                room_id,
                None,
                local_rpc::MAX_MESSAGES,
                local_rpc::MAX_ROOM_SNAPSHOT_BYTES,
                rpc_message_size_estimate,
            )
            .unwrap_or_else(|| super::room::ResidentMessagePage {
                messages: Vec::new(),
                has_older: false,
            });
        let has_older = page.has_older;
        let messages: Vec<Message> = page
            .messages
            .into_iter()
            .map(|message| self.rpc_message(message))
            .collect();
        let mut snapshot = self.rpc_room_projection(room_id);
        if has_older {
            snapshot.older_cursor = messages.first().map(|message| message.message_id);
            snapshot.at_start = false;
        }
        snapshot.messages = messages;
        snapshot
    }

    fn rpc_room_projection(&self, room_id: RoomId) -> RoomSnapshot {
        let (older_cursor, at_start) = self.room.history_cursor(room_id);
        RoomSnapshot {
            room_id,
            room_generation: self.room.room_generation(room_id).unwrap_or_default(),
            history_revision: self.room.room_history_revision(room_id).unwrap_or_default(),
            messages: Vec::new(),
            older_cursor,
            at_start,
            participants: self
                .room
                .participant_summaries(room_id)
                .into_iter()
                .map(|user| local_rpc::model::Participant {
                    user_id: user.user_id,
                    name: user.username,
                    online: user.online,
                    speaking: false,
                    voice_state: rpc_voice_state(user.voice_state),
                })
                .collect(),
        }
    }

    pub(crate) fn rpc_resident_history_page(
        &self,
        room_id: RoomId,
        before: rpc::ids::MessageId,
        limit: u16,
    ) -> Option<RpcHistoryPage> {
        let page = self.room.resident_message_page(
            room_id,
            Some(before),
            usize::from(limit),
            local_rpc::MAX_ROOM_SNAPSHOT_BYTES,
            rpc_message_size_estimate,
        )?;
        if page.messages.is_empty() {
            return None;
        }
        let has_older = page.has_older;
        let messages = page
            .messages
            .into_iter()
            .map(|message| self.rpc_message(message))
            .collect::<Vec<_>>();
        let (_, room_at_start) = self.room.history_cursor(room_id);
        Some(RpcHistoryPage {
            room_generation: self.room.room_generation(room_id)?,
            older_cursor: messages.first().map(|message| message.message_id),
            at_start: !has_older && room_at_start,
            messages,
        })
    }

    pub(crate) fn rpc_canonical_message(
        &self,
        room_id: RoomId,
        message_id: rpc::ids::MessageId,
    ) -> Option<Message> {
        self.room
            .resident_message(room_id, message_id)
            .cloned()
            .map(|message| self.rpc_message(message))
    }

    fn rpc_message(&self, message: rpc::control::ChatMessage) -> Message {
        let detail = message.file_transfer_id.and_then(|transfer_id| {
            self.room.resident_file_detail(
                message.room_id,
                &crate::room_history::FileHistoryKey {
                    timestamp_ms: message.timestamp_ms,
                    transfer_id,
                },
            )
        });
        self.rpc_message_with_file_detail(message, detail)
    }

    fn rpc_message_with_file_detail(
        &self,
        message: rpc::control::ChatMessage,
        file_detail: Option<&crate::room_history::FileDetail>,
    ) -> Message {
        let local_user = self.user_id;
        let attachment = message.file_transfer_id.and_then(|transfer_id| {
            let key = crate::room_history::FileHistoryKey {
                timestamp_ms: message.timestamp_ms,
                transfer_id,
            };
            let detail = file_detail?;
            let descriptor = self.rpc_attachment_descriptor(key, detail)?;
            kvlog::info!(
                "daemon attachment descriptor projected",
                room_id = message.room_id.0,
                message_id = message.message_id.0,
                attachment_timestamp_ms = descriptor.id.timestamp_ms,
                attachment_transfer_id = descriptor.id.transfer_id.0,
                file_name = descriptor.file_name.as_str(),
                byte_len = descriptor.byte_len
            );
            Some(descriptor)
        });
        Message {
            room_id: message.room_id,
            message_id: message.message_id,
            sender_id: message.sender,
            sender_name: message.sender_name,
            body: message.body,
            timestamp_ms: message.timestamp_ms,
            local: Some(message.sender) == local_user,
            edited: message.flags.edited(),
            unverified: self.room.message_unverified(
                message.room_id,
                message.message_id,
                local_user,
            ),
            notice: false,
            reference: None,
            attachment,
        }
    }

    pub(crate) fn handle_rpc_frame(
        &mut self,
        client_id: ClientId,
        frame: ClientFrame,
    ) -> RpcCommandEffect {
        match frame {
            ClientFrame::SelectServer { request_id, label } => {
                RpcCommandEffect::Reply(self.rpc_select_server(client_id, request_id, label))
            }
            ClientFrame::ResolveServerPrompt {
                request_id,
                attempt_id,
                accept,
            } => RpcCommandEffect::Reply(
                self.rpc_resolve_server_prompt(client_id, request_id, attempt_id, accept),
            ),
            ClientFrame::Ping { request_id, nonce } => RpcCommandEffect::Pong(request_id, nonce),
            ClientFrame::RequestSnapshot { request_id } => RpcCommandEffect::Snapshot(request_id),
            ClientFrame::Disconnect { request_id } => {
                RpcCommandEffect::Disconnect(accepted(request_id, Operation::Disconnect))
            }
            ClientFrame::Settings {
                request_id,
                command,
            } => RpcCommandEffect::Reply(rejected(
                request_id,
                command.operation(),
                500,
                "settings command bypassed runtime dispatch",
            )),
            ClientFrame::Appearance {
                request_id,
                command,
            } => RpcCommandEffect::Reply(rejected(
                request_id,
                command.operation(),
                500,
                "appearance command bypassed runtime dispatch",
            )),
            ClientFrame::Identity {
                request_id,
                command,
            } => RpcCommandEffect::Reply(rejected(
                request_id,
                command.operation(),
                500,
                "identity command bypassed runtime dispatch",
            )),
            ClientFrame::RunCommand { request_id, body } => {
                let room_id = self.room.selected_room_for(client_id);
                match self.run_frontend_command_captured(client_id, room_id, body) {
                    Ok(lines) => RpcCommandEffect::CommandResult {
                        result: accepted(request_id, Operation::RunCommand),
                        lines,
                    },
                    Err(message) => RpcCommandEffect::CommandResult {
                        result: rejected(request_id, Operation::RunCommand, 422, &message),
                        lines: Vec::new(),
                    },
                }
            }
            ClientFrame::RequestCommandCandidates { request_id, kind } => {
                RpcCommandEffect::CommandCandidates {
                    request_id,
                    kind,
                    items: self.frontend_command_candidates(kind),
                }
            }
            ClientFrame::ResolveMessageReference {
                request_id,
                room_id,
                room_generation,
                message_id,
            } => {
                let message = (self.room.room_generation(room_id) == Some(room_generation))
                    .then(|| {
                        self.room
                            .reference_message(room_id, message_id)
                            .map(|(message, detail)| {
                                self.rpc_message_with_file_detail(message, detail.as_ref())
                            })
                    })
                    .flatten();
                RpcCommandEffect::MessageReferenceResolved {
                    request_id,
                    room_id,
                    room_generation,
                    message_id,
                    message,
                }
            }
            ClientFrame::StartLiveShare { .. } | ClientFrame::StopLiveShare { .. } => {
                RpcCommandEffect::None
            }
            ClientFrame::SelectRoom {
                request_id,
                room_id,
            } => {
                let previous = std::mem::replace(&mut self.command_client, client_id);
                let selected = self.set_viewed_room(room_id);
                self.command_client = previous;
                if selected {
                    RpcCommandEffect::Reply(accepted(request_id, Operation::SelectRoom))
                } else {
                    RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::SelectRoom,
                        404,
                        "room is not available",
                    ))
                }
            }
            ClientFrame::LoadOlder {
                request_id,
                room_id,
                room_generation,
                before,
                limit,
            } => {
                if self.room.room_generation(room_id) != Some(room_generation) {
                    return RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::LoadOlder,
                        409,
                        "room generation is stale",
                    ));
                }
                if self.room.selected_room_for(client_id) != Some(room_id) {
                    return RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::LoadOlder,
                        409,
                        "room is not selected by this client",
                    ));
                }
                let (expected_before, at_start) = self.room.history_cursor(room_id);
                if at_start {
                    return RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::LoadOlder,
                        409,
                        "no older history is currently available",
                    ));
                }
                if before != expected_before {
                    return RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::LoadOlder,
                        409,
                        "history cursor is stale",
                    ));
                }
                let Some((_, canonical_before, canonical_limit)) =
                    self.room.older_history_request(room_id)
                else {
                    return RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::LoadOlder,
                        409,
                        "an older-history fetch is already active",
                    ));
                };
                let limit = limit.max(1).min(canonical_limit);
                if self.send_network_command(
                    NetworkCommand::FetchHistory {
                        room_id,
                        before: canonical_before,
                        limit,
                    },
                    false,
                ) {
                    RpcCommandEffect::Reply(accepted(request_id, Operation::LoadOlder))
                } else {
                    self.room.abort_history_fetch(room_id, canonical_before);
                    RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::LoadOlder,
                        503,
                        "not connected",
                    ))
                }
            }
            ClientFrame::SendMessage {
                request_id,
                room_id,
                body,
            } => {
                if body.trim().is_empty() {
                    return RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::SendMessage,
                        422,
                        "chat message is empty",
                    ));
                }
                if self.room.room_meta(room_id).is_none() {
                    return RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::SendMessage,
                        404,
                        "room is not available",
                    ));
                }
                if self.send_network_command(NetworkCommand::SendChat { room_id, body }, true) {
                    RpcCommandEffect::Reply(accepted(request_id, Operation::SendMessage))
                } else {
                    RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::SendMessage,
                        503,
                        "not connected",
                    ))
                }
            }
            ClientFrame::EditMessage {
                request_id,
                room_id,
                target,
                body,
            } => {
                if body.trim().is_empty() {
                    return RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::EditMessage,
                        422,
                        "chat message is empty",
                    ));
                }
                if !self.rpc_owns_message(room_id, target) {
                    return RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::EditMessage,
                        403,
                        "message cannot be edited",
                    ));
                }
                if self.send_network_command(
                    NetworkCommand::EditChat {
                        room_id,
                        target,
                        body,
                    },
                    true,
                ) {
                    RpcCommandEffect::Reply(accepted(request_id, Operation::EditMessage))
                } else {
                    RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::EditMessage,
                        503,
                        "not connected",
                    ))
                }
            }
            ClientFrame::DeleteMessage {
                request_id,
                room_id,
                target,
            } => {
                if !self.rpc_owns_message(room_id, target) {
                    return RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::DeleteMessage,
                        403,
                        "message cannot be deleted",
                    ));
                }
                if self.network.is_none() {
                    RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::DeleteMessage,
                        503,
                        "not connected",
                    ))
                } else {
                    self.delete_chat_messages(room_id, vec![target]);
                    RpcCommandEffect::Reply(accepted(request_id, Operation::DeleteMessage))
                }
            }
            ClientFrame::SetVoiceState { request_id, state } => {
                self.set_voice_state(core_voice_state(state));
                RpcCommandEffect::Reply(accepted(request_id, Operation::SetVoiceState))
            }
            ClientFrame::SetOutputVolume { request_id, volume } => {
                self.set_output_volume(volume);
                RpcCommandEffect::Reply(accepted(request_id, Operation::SetOutputVolume))
            }
            ClientFrame::JoinVoice {
                request_id,
                room_id,
            } => {
                if self.network.is_none() || self.room.room_meta(room_id).is_none() {
                    RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::JoinVoice,
                        503,
                        "voice room is unavailable",
                    ))
                } else {
                    self.join_voice_room(room_id);
                    RpcCommandEffect::Reply(accepted(request_id, Operation::JoinVoice))
                }
            }
            ClientFrame::LeaveVoice { request_id, .. } => {
                self.leave_voice_command();
                RpcCommandEffect::Reply(accepted(request_id, Operation::LeaveVoice))
            }
            ClientFrame::BeginUpload { request_id, upload } => {
                if self.room.room_meta(upload.room_id).is_none() {
                    return RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::BeginUpload,
                        404,
                        "upload room is unavailable",
                    ));
                }
                if self.network.is_none() {
                    return RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::BeginUpload,
                        503,
                        "not connected",
                    ));
                }
                if upload.byte_len > self.config.files.max_upload_bytes() {
                    return RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::BeginUpload,
                        413,
                        "upload exceeds configured size limit",
                    ));
                }
                RpcCommandEffect::BeginUpload { request_id, upload }
            }
            ClientFrame::FinishUpload { request_id, .. } => RpcCommandEffect::Reply(rejected(
                request_id,
                Operation::FinishUpload,
                409,
                "upload has no runtime staging state",
            )),
            ClientFrame::CancelUpload { request_id, .. } => {
                RpcCommandEffect::Reply(accepted(request_id, Operation::CancelUpload))
            }
            ClientFrame::BeginAttachmentRead { request_id, read } => {
                if self.room.selected_room_for(client_id) != Some(read.room_id) {
                    return RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::BeginAttachmentRead,
                        403,
                        "attachment room is not selected by this client",
                    ));
                }
                let Some((descriptor, source)) =
                    self.rpc_attachment_source(read.room_id, read.attachment_id)
                else {
                    return RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::BeginAttachmentRead,
                        404,
                        "attachment is unavailable",
                    ));
                };
                RpcCommandEffect::BeginRead {
                    result: accepted(request_id, Operation::BeginAttachmentRead),
                    read,
                    descriptor,
                    source,
                }
            }
            ClientFrame::OpenAttachmentSource {
                request_id,
                room_id,
                attachment_id,
            } => {
                if self.room.selected_room_for(client_id) != Some(room_id) {
                    return RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::OpenAttachmentSource,
                        403,
                        "attachment room is not selected by this client",
                    ));
                }
                let Some((descriptor, source)) = self.rpc_attachment_source(room_id, attachment_id)
                else {
                    return RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::OpenAttachmentSource,
                        404,
                        "attachment source is unavailable",
                    ));
                };
                if descriptor.byte_len == 0 {
                    return RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::OpenAttachmentSource,
                        422,
                        "attachment source is empty",
                    ));
                }
                RpcCommandEffect::OpenAttachmentSource {
                    request_id,
                    room_id,
                    attachment_id,
                    descriptor,
                    source,
                }
            }
            ClientFrame::CancelBulkTransfer {
                request_id,
                transfer_id,
            } => {
                let _ = transfer_id;
                RpcCommandEffect::Reply(rejected(
                    request_id,
                    Operation::CancelBulkTransfer,
                    404,
                    "bulk transfer is not active",
                ))
            }
            ClientFrame::CancelFileTransfer {
                request_id,
                transfer_id,
            } => {
                if self.network.is_none() {
                    RpcCommandEffect::Reply(rejected(
                        request_id,
                        Operation::CancelFileTransfer,
                        503,
                        "not connected",
                    ))
                } else {
                    self.cancel_transfer(transfer_id);
                    RpcCommandEffect::Reply(accepted(request_id, Operation::CancelFileTransfer))
                }
            }
            ClientFrame::UploadChunk(_) => RpcCommandEffect::None,
        }
    }

    fn rpc_select_server(
        &mut self,
        client_id: ClientId,
        request_id: RequestId,
        label: String,
    ) -> RequestResult {
        if self
            .rpc_server_selection_issue
            .as_ref()
            .is_some_and(|issue| issue.owner == client_id)
        {
            self.rpc_server_selection_issue = None;
        }
        let Some(server) = self
            .config
            .servers
            .iter()
            .find(|server| server.label == label)
        else {
            return rejected(
                request_id,
                Operation::SelectServer,
                404,
                "server is not configured",
            );
        };
        if server
            .token
            .starts_with(rpc::crypto::OPEN_PAIR_RECOVERY_PREFIX)
        {
            return rejected(
                request_id,
                Operation::SelectServer,
                409,
                "server pairing is incomplete; finish pairing in the terminal client",
            );
        }
        if self.room.active_server_label.as_deref() == Some(label.as_str())
            && self.network.is_some()
        {
            return accepted(request_id, Operation::SelectServer);
        }
        if self.room.has_active_transfers() {
            return rejected(
                request_id,
                Operation::SelectServer,
                409,
                super::SERVER_SWITCH_TRANSFER_BLOCKED,
            );
        }
        if self.start_connection(&label, client_id) {
            accepted(request_id, Operation::SelectServer)
        } else {
            rejected(
                request_id,
                Operation::SelectServer,
                503,
                "failed to start the server connection",
            )
        }
    }

    fn rpc_resolve_server_prompt(
        &mut self,
        client_id: ClientId,
        request_id: RequestId,
        attempt_id: u64,
        accept_prompt: bool,
    ) -> RequestResult {
        let current = self
            .rpc_server_selection_issue
            .as_ref()
            .filter(|issue| issue.owner == client_id)
            .and_then(|issue| match &issue.issue {
                super::RpcServerSelectionIssue::Prompt(
                    local_rpc::model::ServerSelectionPrompt::AllowUnencryptedTransport {
                        attempt_id,
                        ..
                    },
                ) => Some(*attempt_id),
                super::RpcServerSelectionIssue::Error(_) => None,
            });
        if current != Some(attempt_id) {
            return rejected(
                request_id,
                Operation::ResolveServerPrompt,
                409,
                "server selection prompt is stale",
            );
        }

        let previous = std::mem::replace(&mut self.command_client, client_id);
        let resolved = if accept_prompt {
            self.accept_transport_encryption_warning_for(attempt_id)
        } else {
            self.cancel_transport_encryption_warning_for(attempt_id);
            true
        };
        self.command_client = previous;
        if resolved {
            accepted(request_id, Operation::ResolveServerPrompt)
        } else {
            rejected(
                request_id,
                Operation::ResolveServerPrompt,
                500,
                "could not apply the server security preference",
            )
        }
    }

    fn rpc_owns_message(&self, room_id: RoomId, target: rpc::ids::MessageId) -> bool {
        self.room
            .resident_message(room_id, target)
            .is_some_and(|message| Some(message.sender) == self.user_id)
    }

    fn rpc_attachment_descriptor(
        &self,
        key: crate::room_history::FileHistoryKey,
        detail: &crate::room_history::FileDetail,
    ) -> Option<AttachmentDescriptor> {
        let attachment_id = AttachmentId {
            timestamp_ms: key.timestamp_ms,
            transfer_id: key.transfer_id,
        };
        let metadata = self
            .download_store
            .attachment_metadata_by_id(attachment_id)?;
        let (width, height) = detail
            .dimensions()
            .map_or((None, None), |(w, h)| (Some(w), Some(h)));
        let content_type = metadata.content_type.to_string();
        let media_kind = if content_type.starts_with("image/") {
            MediaKind::Image
        } else if content_type.starts_with("video/") {
            MediaKind::Video
        } else if content_type.starts_with("audio/") {
            MediaKind::Audio
        } else {
            MediaKind::File
        };
        Some(AttachmentDescriptor {
            id: attachment_id,
            file_name: detail.file_name.clone(),
            media_kind,
            content_type,
            byte_len: metadata.byte_len,
            width,
            height,
        })
    }

    fn rpc_attachment_source(
        &self,
        room_id: RoomId,
        attachment_id: local_rpc::model::AttachmentId,
    ) -> Option<(AttachmentDescriptor, crate::receive_store::Source)> {
        let key = crate::room_history::FileHistoryKey {
            timestamp_ms: attachment_id.timestamp_ms,
            transfer_id: attachment_id.transfer_id,
        };
        let (message, detail) = self.room.reference_attachment(room_id, &key)?;
        let descriptor = self.rpc_attachment_descriptor(key, &detail)?;
        let source = self.download_store.resolve_attachment(attachment_id)?;
        let (source_kind, source_bytes) = match &source {
            crate::receive_store::Source::Memory { bytes, .. } => ("memory", bytes.len() as u64),
            crate::receive_store::Source::Disk(path) => (
                "disk",
                std::fs::metadata(path).map_or(0, |metadata| metadata.len()),
            ),
        };
        if source_bytes != descriptor.byte_len {
            kvlog::warn!(
                "daemon attachment source length changed",
                room_id = room_id.0,
                message_id = message.message_id.0,
                attachment_timestamp_ms = attachment_id.timestamp_ms,
                attachment_transfer_id = attachment_id.transfer_id.0,
                descriptor_bytes = descriptor.byte_len,
                source_kind = source_kind,
                source_bytes = source_bytes
            );
            return None;
        }
        kvlog::info!(
            "daemon attachment source resolved",
            room_id = room_id.0,
            message_id = message.message_id.0,
            attachment_timestamp_ms = attachment_id.timestamp_ms,
            attachment_transfer_id = attachment_id.transfer_id.0,
            served_name = detail.file_name.as_str(),
            descriptor_bytes = descriptor.byte_len,
            source_kind = source_kind,
            source_bytes = source_bytes
        );
        Some((descriptor, source))
    }

    pub(crate) fn queue_rpc_upload(
        &mut self,
        room_id: RoomId,
        path: std::path::PathBuf,
        name: String,
    ) -> Result<(), String> {
        if self.network.is_none() || self.room.room_meta(room_id).is_none() {
            let _ = std::fs::remove_file(path);
            return Err("upload room is no longer available".into());
        }
        let request = crate::client_net::UploadFileRequest {
            path: path.clone(),
            name_override: Some(name),
            delete_after_open: true,
        };
        if self.send_network_command(
            NetworkCommand::UploadFile {
                room_id: Some(room_id),
                request,
            },
            true,
        ) {
            Ok(())
        } else {
            let _ = std::fs::remove_file(path);
            Err("not connected".into())
        }
    }
}

pub(crate) fn rpc_live_share_status(
    state: super::ShareViewState,
) -> local_rpc::model::LiveShareViewStatus {
    match state {
        super::ShareViewState::WaitingForKeyframe => {
            local_rpc::model::LiveShareViewStatus::WaitingForKeyframe
        }
        super::ShareViewState::Reconnecting => local_rpc::model::LiveShareViewStatus::Reconnecting,
    }
}

fn rpc_voice_state(state: rpc::control::VoiceState) -> VoiceState {
    match state {
        rpc::control::VoiceState::Live => VoiceState::Live,
        rpc::control::VoiceState::Muted => VoiceState::Muted,
        rpc::control::VoiceState::Deafened => VoiceState::Deafened,
    }
}

fn core_voice_state(state: VoiceState) -> rpc::control::VoiceState {
    match state {
        VoiceState::Live => rpc::control::VoiceState::Live,
        VoiceState::Muted => rpc::control::VoiceState::Muted,
        VoiceState::Deafened => rpc::control::VoiceState::Deafened,
    }
}

fn rpc_message_size_estimate(message: &rpc::control::ChatMessage) -> usize {
    const STRUCTURAL_OVERHEAD: usize = 256;
    const ATTACHMENT_OVERHEAD: usize = 512;
    STRUCTURAL_OVERHEAD
        .saturating_add(message.sender_name.len())
        .saturating_add(message.body.len())
        .saturating_add(if message.file_transfer_id.is_some() {
            ATTACHMENT_OVERHEAD
        } else {
            0
        })
}

fn accepted(request_id: RequestId, operation: Operation) -> RequestResult {
    RequestResult {
        request_id,
        operation,
        outcome: RequestOutcome::Accepted,
    }
}

fn rejected(
    request_id: RequestId,
    operation: Operation,
    code: u16,
    message: &str,
) -> RequestResult {
    RequestResult {
        request_id,
        operation,
        outcome: RequestOutcome::Rejected {
            code,
            message: message.into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::{
        control::{RoomInfo, RoomKind as WireRoomKind, UserSummary, VoiceState},
        ids::UserId,
    };

    fn app_with_server(label: &str, token: &str, require_transport_encryption: bool) -> App {
        let mut config = crate::config::Config::default();
        config.servers.push(crate::config::ServerEntry {
            label: label.into(),
            username: "alice".into(),
            tcp_addr: "127.0.0.1:4000".into(),
            udp_addr: "127.0.0.1:4000".into(),
            token: token.into(),
            require_transport_encryption,
            ..Default::default()
        });
        App::new(config, None).unwrap()
    }

    #[test]
    fn rpc_snapshot_projects_safe_server_catalog_and_pairing_state() {
        let app = app_with_server(
            "work",
            &format!("{}pending", rpc::crypto::OPEN_PAIR_RECOVERY_PREFIX),
            false,
        );

        let snapshot = app.rpc_snapshot(ClientId(7));

        assert_eq!(snapshot.server_selection.servers.len(), 1);
        let server = &snapshot.server_selection.servers[0];
        assert_eq!(server.label, "work");
        assert_eq!(server.username, "alice");
        assert_eq!(server.tcp_addr, "127.0.0.1:4000");
        assert!(!server.require_transport_encryption);
        assert_eq!(server.availability, ServerAvailability::PairingIncomplete);
    }

    #[test]
    fn rpc_rejects_unknown_and_incomplete_server_selection() {
        let mut app = app_with_server(
            "work",
            &format!("{}pending", rpc::crypto::OPEN_PAIR_RECOVERY_PREFIX),
            true,
        );

        for (request_id, label, expected_code) in [(1, "missing", 404), (2, "work", 409)] {
            let RpcCommandEffect::Reply(result) = app.handle_rpc_frame(
                ClientId(7),
                ClientFrame::SelectServer {
                    request_id: RequestId(request_id),
                    label: label.into(),
                },
            ) else {
                panic!("expected request result");
            };
            assert_eq!(result.operation, Operation::SelectServer);
            assert!(matches!(
                result.outcome,
                RequestOutcome::Rejected { code, .. } if code == expected_code
            ));
        }
        assert!(app.network.is_none());
    }

    #[test]
    fn rpc_selects_ready_server_and_projects_connecting_state() {
        let mut app = app_with_server("work", "token", true);
        let client_id = ClientId(7);
        app.register_rpc_client(client_id);

        let RpcCommandEffect::Reply(result) = app.handle_rpc_frame(
            client_id,
            ClientFrame::SelectServer {
                request_id: RequestId(1),
                label: "work".into(),
            },
        ) else {
            panic!("expected request result");
        };

        assert_eq!(result.outcome, RequestOutcome::Accepted);
        let snapshot = app.rpc_snapshot(client_id);
        assert_eq!(snapshot.active_server.as_deref(), Some("work"));
        assert_eq!(snapshot.connection, ConnectionState::Connecting);
        assert!(app.network.is_some());
    }

    #[test]
    fn rpc_server_prompt_is_owner_scoped_and_cancel_is_stale_safe() {
        let mut app = app_with_server("legacy", "token", true);
        let owner = ClientId(7);
        let observer = ClientId(8);
        app.register_rpc_client(owner);
        app.register_rpc_client(observer);
        app.connection_attempt = Some(super::super::ConnectionAttempt {
            generation: 11,
            owner,
            server_label: "legacy".into(),
        });
        app.rpc_server_selection_issue = Some(super::super::OwnedRpcServerSelectionIssue {
            owner,
            issue: super::super::RpcServerSelectionIssue::Prompt(
                local_rpc::model::ServerSelectionPrompt::AllowUnencryptedTransport {
                    label: "legacy".into(),
                    attempt_id: 11,
                },
            ),
        });

        assert!(app.rpc_snapshot(owner).server_selection.prompt.is_some());
        assert!(app.rpc_snapshot(observer).server_selection.prompt.is_none());

        let stale = app.rpc_resolve_server_prompt(owner, RequestId(1), 10, false);
        assert!(matches!(
            stale.outcome,
            RequestOutcome::Rejected { code: 409, .. }
        ));
        assert!(app.connection_attempt.is_some());

        let canceled = app.rpc_resolve_server_prompt(owner, RequestId(2), 11, false);
        assert_eq!(canceled.outcome, RequestOutcome::Accepted);
        assert!(app.connection_attempt.is_none());
        assert!(app.rpc_snapshot(owner).server_selection.prompt.is_none());
    }

    #[test]
    fn rpc_frontends_keep_independent_selected_rooms() {
        let mut app = App::new(crate::config::Config::default(), None).unwrap();
        let rooms = vec![
            RoomInfo {
                room_id: RoomId(1),
                name: "one".into(),
                kind: WireRoomKind::Public,
                head: None,
                voice_users: Vec::new(),
            },
            RoomInfo {
                room_id: RoomId(2),
                name: "two".into(),
                kind: WireRoomKind::Public,
                head: None,
                voice_users: Vec::new(),
            },
        ];
        app.room.authenticated(
            &rooms,
            vec![UserSummary {
                user_id: UserId(1),
                username: "alice".into(),
                online: true,
                connected_at_ms: 1,
                voice_state: VoiceState::default(),
            }],
            RoomId(1),
            Some(RoomId(1)),
            Some(UserId(1)),
        );
        let first = ClientId(1);
        let second = ClientId(2);
        app.register_rpc_client(first);
        app.register_rpc_client(second);
        assert!(matches!(
            app.handle_rpc_frame(
                first,
                ClientFrame::SelectRoom {
                    request_id: RequestId(1),
                    room_id: RoomId(2)
                }
            ),
            RpcCommandEffect::Reply(_)
        ));
        assert_eq!(app.rpc_snapshot(first).selected_room, Some(RoomId(2)));
        assert_eq!(app.rpc_snapshot(second).selected_room, Some(RoomId(1)));
        assert_eq!(app.room.viewed_room, Some(RoomId(1)));
    }

    #[test]
    fn rpc_history_requests_reject_stale_room_generation() {
        let mut app = App::new(crate::config::Config::default(), None).unwrap();
        app.room.authenticated(
            &[RoomInfo {
                room_id: RoomId(1),
                name: "one".into(),
                kind: WireRoomKind::Public,
                head: None,
                voice_users: Vec::new(),
            }],
            Vec::new(),
            RoomId(1),
            Some(RoomId(1)),
            Some(UserId(1)),
        );
        let client = ClientId(7);
        app.register_rpc_client(client);
        let stale = app.room.room_generation(RoomId(1)).unwrap().wrapping_add(1);

        let RpcCommandEffect::Reply(result) = app.handle_rpc_frame(
            client,
            ClientFrame::LoadOlder {
                request_id: RequestId(1),
                room_id: RoomId(1),
                room_generation: stale,
                before: None,
                limit: 10,
            },
        ) else {
            panic!("expected request result");
        };
        assert!(matches!(
            result.outcome,
            RequestOutcome::Rejected { code: 409, .. }
        ));

        let RpcCommandEffect::MessageReferenceResolved {
            room_generation,
            message,
            ..
        } = app.handle_rpc_frame(
            client,
            ClientFrame::ResolveMessageReference {
                request_id: RequestId(2),
                room_id: RoomId(1),
                room_generation: stale,
                message_id: rpc::ids::MessageId(1),
            },
        )
        else {
            panic!("expected reference response");
        };
        assert_eq!(room_generation, stale);
        assert!(message.is_none());
    }

    #[test]
    fn rpc_frontends_inherit_authenticated_room_and_preserve_valid_independent_views() {
        let mut app = App::new(crate::config::Config::default(), None).unwrap();
        let first = ClientId(1);
        let second = ClientId(2);
        app.register_rpc_client(first);
        app.register_rpc_client(second);
        let rooms = vec![
            RoomInfo {
                room_id: RoomId(1),
                name: "one".into(),
                kind: WireRoomKind::Public,
                head: None,
                voice_users: Vec::new(),
            },
            RoomInfo {
                room_id: RoomId(2),
                name: "two".into(),
                kind: WireRoomKind::Public,
                head: None,
                voice_users: Vec::new(),
            },
        ];
        let users = || {
            vec![UserSummary {
                user_id: UserId(1),
                username: "alice".into(),
                online: true,
                connected_at_ms: 1,
                voice_state: VoiceState::default(),
            }]
        };

        app.room
            .authenticated(&rooms, users(), RoomId(1), Some(RoomId(2)), Some(UserId(1)));
        app.reconcile_rpc_client_views();

        assert_eq!(app.rpc_snapshot(first).selected_room, Some(RoomId(2)));
        assert_eq!(app.rpc_snapshot(second).selected_room, Some(RoomId(2)));

        assert!(app.room.prepare_client_view(first, RoomId(1)));
        app.room
            .authenticated(&rooms, users(), RoomId(1), Some(RoomId(2)), Some(UserId(1)));
        app.reconcile_rpc_client_views();

        assert_eq!(app.rpc_snapshot(first).selected_room, Some(RoomId(1)));
        assert_eq!(app.rpc_snapshot(second).selected_room, Some(RoomId(2)));

        app.room.authenticated(
            &rooms[1..],
            users(),
            RoomId(2),
            Some(RoomId(2)),
            Some(UserId(1)),
        );
        app.reconcile_rpc_client_views();

        assert_eq!(app.rpc_snapshot(first).selected_room, Some(RoomId(2)));
        assert_eq!(app.rpc_snapshot(second).selected_room, Some(RoomId(2)));
    }

    #[test]
    fn rpc_server_switch_rejects_active_transfer_without_disconnecting() {
        let mut app = app_with_server("work", "token", true);
        app.room.authenticated(
            &[RoomInfo {
                room_id: RoomId(1),
                name: "one".into(),
                kind: WireRoomKind::Public,
                head: None,
                voice_users: Vec::new(),
            }],
            Vec::new(),
            RoomId(1),
            None,
            None,
        );
        app.room.transfer_progress(
            RoomId(1),
            rpc::ids::FileTransferId(9),
            10,
            100,
            crate::client_net::TransferDirection::Outgoing,
        );

        let RpcCommandEffect::Reply(result) = app.handle_rpc_frame(
            ClientId(7),
            ClientFrame::SelectServer {
                request_id: RequestId(1),
                label: "work".into(),
            },
        ) else {
            panic!("expected request result");
        };

        assert!(matches!(
            result.outcome,
            RequestOutcome::Rejected { code: 409, ref message }
                if message == super::super::SERVER_SWITCH_TRANSFER_BLOCKED
        ));
        assert!(app.network.is_none());
    }

    #[test]
    fn rpc_runs_frontend_safe_commands_and_captures_output() {
        let mut app = App::new(crate::config::Config::default(), None).unwrap();
        let effect = app.handle_rpc_frame(
            ClientId(7),
            ClientFrame::RunCommand {
                request_id: RequestId(3),
                body: "/whoami".into(),
            },
        );

        let RpcCommandEffect::CommandResult { result, lines } = effect else {
            panic!("expected command result");
        };
        assert_eq!(result.operation, Operation::RunCommand);
        assert_eq!(result.outcome, RequestOutcome::Accepted);
        assert!(!lines.is_empty());
    }

    #[test]
    fn rpc_rejects_terminal_only_commands_without_running_them() {
        let mut app = App::new(crate::config::Config::default(), None).unwrap();
        let effect = app.handle_rpc_frame(
            ClientId(7),
            ClientFrame::RunCommand {
                request_id: RequestId(4),
                body: "/clear".into(),
            },
        );

        let RpcCommandEffect::CommandResult { result, lines } = effect else {
            panic!("expected command result");
        };
        assert!(matches!(
            result.outcome,
            RequestOutcome::Rejected { code: 422, .. }
        ));
        assert!(lines.is_empty());
    }

    #[test]
    fn rpc_snapshot_exposes_live_share_decoder_metadata() {
        let mut app = App::new(crate::config::Config::default(), None).unwrap();
        let stream_id = rpc::ids::StreamId(11);
        app.room.available_shares.insert(
            stream_id,
            crate::app::AvailableShare {
                room_id: RoomId(7),
                generation: 4,
                view_secret: vec![9; 32],
                sender_name: "alice".into(),
                codec: "avc1.42C00D".into(),
                coded_width: 320,
                coded_height: 240,
                extradata: vec![1, 2, 3],
            },
        );
        let snapshot = app.rpc_snapshot(ClientId::PRIMARY);
        assert_eq!(snapshot.live_shares.len(), 1);
        let share = &snapshot.live_shares[0];
        assert_eq!(share.stream_id, stream_id);
        assert_eq!(share.generation, 4);
        assert_eq!(share.sender_name, "alice");
        assert_eq!((share.coded_width, share.coded_height), (320, 240));
        assert_eq!(share.extradata, vec![1, 2, 3]);
    }

    #[test]
    fn attachment_identity_uses_timestamp_and_server_transfer_id() {
        let first = AttachmentId {
            timestamp_ms: 1_000,
            transfer_id: rpc::ids::FileTransferId(7),
        };
        let next_transfer = AttachmentId {
            timestamp_ms: 1_000,
            transfer_id: rpc::ids::FileTransferId(8),
        };
        let reused_transfer = AttachmentId {
            timestamp_ms: 2_000,
            transfer_id: rpc::ids::FileTransferId(7),
        };

        assert_ne!(first, next_transfer);
        assert_ne!(first, reused_transfer);
    }

    #[test]
    fn rejects_attachment_source_outside_selected_room() {
        let mut app = App::new(crate::config::Config::default(), None).unwrap();
        let effect = app.handle_rpc_frame(
            ClientId(7),
            ClientFrame::OpenAttachmentSource {
                request_id: RequestId(8),
                room_id: RoomId(9),
                attachment_id: AttachmentId {
                    timestamp_ms: 10,
                    transfer_id: rpc::ids::FileTransferId(11),
                },
            },
        );
        let RpcCommandEffect::Reply(result) = effect else {
            panic!("expected attachment source rejection");
        };
        assert_eq!(result.operation, Operation::OpenAttachmentSource);
        assert!(matches!(
            result.outcome,
            RequestOutcome::Rejected { code: 403, .. }
        ));
    }

    #[test]
    fn repeated_same_name_and_bytes_get_independent_rpc_attachment_ids() {
        let app = App::new(crate::config::Config::default(), None).unwrap();
        let served_name = app
            .download_store
            .insert("clip.mp4", b"same video bytes".to_vec())
            .unwrap();
        let detail = crate::room_history::FileDetail {
            file_name: served_name,
            length: 16,
            packed_dims: 0,
        };
        let first_id = AttachmentId {
            timestamp_ms: 1_000,
            transfer_id: rpc::ids::FileTransferId(7),
        };
        let second_id = AttachmentId {
            timestamp_ms: 2_000,
            transfer_id: rpc::ids::FileTransferId(8),
        };
        assert!(
            app.download_store
                .bind_attachment(first_id, &detail.file_name)
        );
        assert!(
            app.download_store
                .bind_attachment(second_id, &detail.file_name)
        );
        let first = app
            .rpc_attachment_descriptor(
                crate::room_history::FileHistoryKey {
                    timestamp_ms: 1_000,
                    transfer_id: rpc::ids::FileTransferId(7),
                },
                &detail,
            )
            .expect("first descriptor");
        let second = app
            .rpc_attachment_descriptor(
                crate::room_history::FileHistoryKey {
                    timestamp_ms: 2_000,
                    transfer_id: rpc::ids::FileTransferId(8),
                },
                &detail,
            )
            .expect("second descriptor");

        assert_ne!(first.id, second.id);
        assert!(app.download_store.resolve_attachment(first.id).is_some());
        assert!(app.download_store.resolve_attachment(second.id).is_some());
    }

    #[test]
    fn historical_filename_does_not_rebind_to_current_memory_source() {
        let app = App::new(crate::config::Config::default(), None).unwrap();
        let served_name = app
            .download_store
            .insert("clipboard.png", b"current image".to_vec())
            .unwrap();
        let detail = crate::room_history::FileDetail {
            file_name: served_name.clone(),
            length: 13,
            packed_dims: 0,
        };
        let current = crate::room_history::FileHistoryKey {
            timestamp_ms: 2_000,
            transfer_id: rpc::ids::FileTransferId(8),
        };
        let historical = crate::room_history::FileHistoryKey {
            timestamp_ms: 1_000,
            transfer_id: rpc::ids::FileTransferId(7),
        };
        assert!(app.download_store.bind_attachment(
            AttachmentId {
                timestamp_ms: current.timestamp_ms,
                transfer_id: current.transfer_id,
            },
            &served_name,
        ));

        assert!(app.rpc_attachment_descriptor(current, &detail).is_some());
        assert!(app.rpc_attachment_descriptor(historical, &detail).is_none());
    }
}
