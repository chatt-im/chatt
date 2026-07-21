use std::sync::atomic::Ordering;

use rpc::{
    daemon::{
        bulk::BeginAttachmentRead,
        bulk::BeginUpload,
        frame::{ClientFrame, Operation, RequestOutcome, RequestResult},
        model::{
            AttachmentDescriptor, AttachmentId, ConnectionState, MediaKind, Message, RequestId,
            RoomKind, RoomSnapshot, RoomSummary, StateSnapshot, TrustState, VoiceState,
        },
    },
    ids::RoomId,
};

use crate::{client_channel::ClientId, client_net::NetworkCommand};

use super::{App, room::ClientRoomKind};

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
    BeginUpload {
        request_id: RequestId,
        upload: BeginUpload,
    },
    None,
}

pub(crate) struct RpcHistoryPage {
    pub messages: Vec<Message>,
    pub older_cursor: Option<rpc::ids::MessageId>,
    pub at_start: bool,
}

impl App {
    pub(crate) fn register_rpc_client(&mut self, client_id: ClientId) {
        if let Some(room_id) = self.room.viewed_room {
            self.room.prepare_client_view(client_id, room_id);
        }
    }

    pub(crate) fn rpc_snapshot(&self, client_id: ClientId) -> StateSnapshot {
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
        let room = selected_room.map(|room_id| self.rpc_room_snapshot(room_id));
        let mut live_shares = self
            .room
            .available_shares
            .iter()
            .map(|(stream_id, share)| rpc::daemon::model::LiveShare {
                room_id: share.room_id,
                stream_id: *stream_id,
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
            local_identity: (!self.room.local_username.is_empty())
                .then(|| self.room.local_username.clone()),
            rooms,
            selected_room,
            room,
            voice: VoiceState {
                muted: self.mic_muted.load(Ordering::Relaxed),
                deafened: self.deafened.load(Ordering::Relaxed),
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
        stream: std::os::unix::net::UnixStream,
    ) -> Result<crate::video::NativeViewerHandle, String> {
        let share = self
            .room
            .available_shares
            .get(&stream_id)
            .ok_or_else(|| "that screen share is no longer available".to_string())?;
        if self.room.voice_room != Some(share.room_id) {
            return Err("join the share's voice room before viewing".into());
        }
        let view_secret = share.view_secret.clone();
        let own_share = self.screencast_stream_id == Some(stream_id);
        let tcp_addr = self.active_tcp_addr.clone();
        let session_id = self.session_id;
        let video_transport = self.video_transport;
        let upstream_is_active = self.subscribers.contains_key(&stream_id);
        let wait_for_upstream_bootstrap = !own_share && !upstream_is_active;
        let handle = self.video_fanout.add_native(
            client_id.0 as u64,
            stream_id,
            stream,
            wait_for_upstream_bootstrap,
        )?;
        if own_share || upstream_is_active {
            return Ok(handle);
        }
        let Some(tcp_addr) = tcp_addr else {
            return Err("not connected to a server".into());
        };
        let Some(session_id) = session_id else {
            return Err("the voice session is no longer active".into());
        };
        let Some(video_transport) = video_transport else {
            return Err("video transport is not ready".into());
        };
        let subscriber = crate::video::start_subscriber(
            session_id,
            stream_id,
            view_secret,
            tcp_addr,
            video_transport,
            self.video_fanout.clone(),
        );
        self.subscribers.insert(stream_id, subscriber);
        Ok(handle)
    }

    pub(crate) fn stop_rpc_live_share(&mut self, stream_id: rpc::ids::StreamId) {
        if self.video_fanout.has_native(stream_id) || self.web_viewing_shares.contains(&stream_id) {
            return;
        }
        if let Some(mut subscriber) = self.subscribers.remove(&stream_id) {
            subscriber.stop();
        }
    }

    fn rpc_room_snapshot(&self, room_id: RoomId) -> RoomSnapshot {
        let page = self
            .room
            .resident_message_page(
                room_id,
                None,
                rpc::daemon::MAX_MESSAGES,
                rpc::daemon::MAX_ROOM_SNAPSHOT_BYTES,
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
        let (room_cursor, room_at_start) = self.room.history_cursor(room_id);
        let (older_cursor, at_start) = if has_older {
            (messages.first().map(|message| message.message_id), false)
        } else {
            (room_cursor, room_at_start)
        };
        RoomSnapshot {
            room_id,
            messages,
            older_cursor,
            at_start,
            participants: self
                .room
                .participant_summaries(room_id)
                .into_iter()
                .map(|user| rpc::daemon::model::Participant {
                    user_id: user.user_id,
                    name: user.username,
                    online: user.online,
                    speaking: false,
                    muted: user.voice_status.muted,
                    deafened: user.voice_status.deafened,
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
            rpc::daemon::MAX_ROOM_SNAPSHOT_BYTES,
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
            older_cursor: messages.first().map(|message| message.message_id),
            at_start: !has_older && room_at_start,
            messages,
        })
    }

    fn rpc_message(&self, message: rpc::control::ChatMessage) -> Message {
        let local_user = self.user_id;
        let attachment = message.file_transfer_id.and_then(|transfer_id| {
            let key = crate::room_history::FileHistoryKey {
                timestamp_ms: message.timestamp_ms,
                transfer_id,
            };
            let detail = self.room.resident_file_detail(message.room_id, &key)?;
            self.rpc_attachment_descriptor(message.room_id, message.message_id, detail)
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
            ClientFrame::Ping { request_id, nonce } => RpcCommandEffect::Pong(request_id, nonce),
            ClientFrame::RequestSnapshot { request_id } => RpcCommandEffect::Snapshot(request_id),
            ClientFrame::Disconnect { request_id } => {
                RpcCommandEffect::Disconnect(accepted(request_id, Operation::Disconnect))
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
                before,
                limit,
            } => {
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
            ClientFrame::SetMuted { request_id, muted } => {
                self.set_mute(muted);
                RpcCommandEffect::Reply(accepted(request_id, Operation::SetMuted))
            }
            ClientFrame::SetDeafened {
                request_id,
                deafened,
            } => {
                self.set_deafen(deafened);
                RpcCommandEffect::Reply(accepted(request_id, Operation::SetDeafened))
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

    fn rpc_owns_message(&self, room_id: RoomId, target: rpc::ids::MessageId) -> bool {
        self.room
            .resident_message(room_id, target)
            .is_some_and(|message| Some(message.sender) == self.user_id)
    }

    fn rpc_attachment_descriptor(
        &self,
        room_id: RoomId,
        message_id: rpc::ids::MessageId,
        detail: &crate::room_history::FileDetail,
    ) -> Option<AttachmentDescriptor> {
        let attachment_id = AttachmentId {
            room_id,
            message_id,
        };
        if !self
            .download_store
            .bind_attachment(attachment_id, &detail.file_name)
        {
            return None;
        }
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
        attachment_id: rpc::daemon::model::AttachmentId,
    ) -> Option<(AttachmentDescriptor, crate::receive_store::Source)> {
        if attachment_id.room_id != room_id {
            return None;
        }
        let history = self.room.history_for(room_id);
        for message in history.messages {
            if message.message_id != attachment_id.message_id {
                continue;
            }
            let Some(transfer_id) = message.file_transfer_id else {
                continue;
            };
            let key = crate::room_history::FileHistoryKey {
                timestamp_ms: message.timestamp_ms,
                transfer_id,
            };
            let Some(detail) = history.files.get(&key) else {
                continue;
            };
            let Some(descriptor) =
                self.rpc_attachment_descriptor(room_id, message.message_id, detail)
            else {
                continue;
            };
            if descriptor.id == attachment_id {
                let source = self.download_store.resolve_attachment(attachment_id)?;
                return Some((descriptor, source));
            }
        }
        None
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

fn rpc_message_size_estimate(message: &rpc::control::ChatMessage) -> usize {
    const STRUCTURAL_OVERHEAD: usize = 256;
    const ATTACHMENT_OVERHEAD: usize = 512;
    STRUCTURAL_OVERHEAD
        .saturating_add(message.sender_name.len())
        .saturating_add(message.body.len())
        .saturating_add(
            message
                .file_transfer_id
                .is_some()
                .then_some(ATTACHMENT_OVERHEAD)
                .unwrap_or(0),
        )
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
        control::{ParticipantVoiceStatus, RoomInfo, RoomKind as WireRoomKind, UserSummary},
        ids::UserId,
    };

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
                voice_status: ParticipantVoiceStatus::default(),
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
    fn rpc_snapshot_exposes_live_share_decoder_metadata() {
        let mut app = App::new(crate::config::Config::default(), None).unwrap();
        let stream_id = rpc::ids::StreamId(11);
        app.room.available_shares.insert(
            stream_id,
            crate::app::AvailableShare {
                room_id: RoomId(7),
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
        assert_eq!(share.sender_name, "alice");
        assert_eq!((share.coded_width, share.coded_height), (320, 240));
        assert_eq!(share.extradata, vec![1, 2, 3]);
    }

    #[test]
    fn attachment_identity_uses_room_and_message() {
        let first = AttachmentId {
            room_id: RoomId(1),
            message_id: rpc::ids::MessageId(7),
        };
        let next_message = AttachmentId {
            room_id: RoomId(1),
            message_id: rpc::ids::MessageId(8),
        };
        let other_room = AttachmentId {
            room_id: RoomId(2),
            message_id: rpc::ids::MessageId(7),
        };

        assert_ne!(first, next_message);
        assert_ne!(first, other_room);
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
        let first = app
            .rpc_attachment_descriptor(RoomId(1), rpc::ids::MessageId(7), &detail)
            .expect("first descriptor");
        let second = app
            .rpc_attachment_descriptor(RoomId(1), rpc::ids::MessageId(8), &detail)
            .expect("second descriptor");

        assert_ne!(first.id, second.id);
        assert!(app.download_store.resolve_attachment(first.id).is_some());
        assert!(app.download_store.resolve_attachment(second.id).is_some());
    }
}
