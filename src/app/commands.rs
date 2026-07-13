use extui::Style;
use extui_editor::{Editor, InlineCompletion, Mode as EditorMode, Span};

use crate::chat_buffer::VirtualChatBuffer;

/// The argument a slash command accepts, driving web autocomplete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlashArg {
    None,
    User,
    Room,
    Sound,
    FreeText(&'static str),
}

impl SlashArg {
    /// The wire name the web view keys its argument completion on.
    pub(crate) fn wire_kind(self) -> &'static str {
        match self {
            SlashArg::None => "none",
            SlashArg::User => "user",
            SlashArg::Room => "room",
            SlashArg::Sound => "sound",
            SlashArg::FreeText(_) => "free",
        }
    }

    /// The free-text placeholder hint, if any.
    pub(crate) fn placeholder(self) -> Option<&'static str> {
        match self {
            SlashArg::FreeText(placeholder) => Some(placeholder),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SlashCommand {
    pub(crate) name: &'static str,
    pub(crate) usage: &'static str,
    pub(crate) description: &'static str,
    pub(crate) arg: SlashArg,
    /// Whether the command is exposed to the web view; TUI-only commands
    /// (overlays, local view manipulation) stay false.
    pub(crate) web: bool,
}

pub(crate) const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/audio",
        usage: "/audio",
        description: "show receive and playback diagnostics",
        arg: SlashArg::None,
        web: true,
    },
    SlashCommand {
        name: "/audio-reset",
        usage: "/audio-reset",
        description: "rebuild audio streams and re-scan devices",
        arg: SlashArg::None,
        web: true,
    },
    SlashCommand {
        name: "/clear",
        usage: "/clear",
        description: "clear the local chat view",
        arg: SlashArg::None,
        web: false,
    },
    SlashCommand {
        name: "/config",
        usage: "/config",
        description: "open settings",
        arg: SlashArg::None,
        web: false,
    },
    SlashCommand {
        name: "/deafen",
        usage: "/deafen",
        description: "stop playback and mute microphone send",
        arg: SlashArg::None,
        web: true,
    },
    SlashCommand {
        name: "/deafened",
        usage: "/deafened",
        description: "show deafen status",
        arg: SlashArg::None,
        web: false,
    },
    SlashCommand {
        name: "/dm",
        usage: "/dm user",
        description: "open a direct message room with a user",
        arg: SlashArg::User,
        web: true,
    },
    SlashCommand {
        name: "/help",
        usage: "/help",
        description: "show this command list",
        arg: SlashArg::None,
        web: false,
    },
    SlashCommand {
        name: "/mute",
        usage: "/mute",
        description: "mute microphone send",
        arg: SlashArg::None,
        web: true,
    },
    SlashCommand {
        name: "/muted",
        usage: "/muted",
        description: "show microphone mute status",
        arg: SlashArg::None,
        web: false,
    },
    SlashCommand {
        name: "/quit",
        usage: "/quit",
        description: "show the quit key hint",
        arg: SlashArg::None,
        web: false,
    },
    SlashCommand {
        name: "/report-bug",
        usage: "/report-bug what went wrong",
        description: "send recent logs and diagnostics to the server",
        arg: SlashArg::FreeText("what went wrong"),
        web: true,
    },
    SlashCommand {
        name: "/room",
        usage: "/room name",
        description: "switch the viewed room by name",
        arg: SlashArg::Room,
        web: true,
    },
    SlashCommand {
        name: "/room-settings",
        usage: "/room-settings",
        description: "open per-room download and persistence overrides",
        arg: SlashArg::None,
        web: false,
    },
    SlashCommand {
        name: "/rooms",
        usage: "/rooms",
        description: "open the room switcher",
        arg: SlashArg::None,
        web: false,
    },
    SlashCommand {
        name: "/servers",
        usage: "/servers",
        description: "open the server list",
        arg: SlashArg::None,
        web: false,
    },
    SlashCommand {
        name: "/settings",
        usage: "/settings",
        description: "open settings",
        arg: SlashArg::None,
        web: false,
    },
    SlashCommand {
        name: "/sound",
        usage: "/sound N|name",
        description: "play a soundboard clip",
        arg: SlashArg::Sound,
        web: true,
    },
    SlashCommand {
        name: "/soundboard",
        usage: "/soundboard",
        description: "list soundboard clips",
        arg: SlashArg::None,
        web: true,
    },
    SlashCommand {
        name: "/stats",
        usage: "/stats",
        description: "toggle detailed lobby voice stats",
        arg: SlashArg::None,
        web: false,
    },
    SlashCommand {
        name: "/undeafen",
        usage: "/undeafen",
        description: "resume playback and microphone send",
        arg: SlashArg::None,
        web: true,
    },
    SlashCommand {
        name: "/unmute",
        usage: "/unmute",
        description: "unmute microphone send",
        arg: SlashArg::None,
        web: true,
    },
    SlashCommand {
        name: "/upload",
        usage: "/upload path/to/file.ext",
        description: "relay a file to room members",
        arg: SlashArg::FreeText("path/to/file.ext"),
        web: false,
    },
    SlashCommand {
        name: "/upload-rate",
        usage: "/upload-rate 200K|off",
        description: "throttle upload speed (bytes/s, K/M suffix, off)",
        arg: SlashArg::FreeText("200K|off"),
        web: true,
    },
    SlashCommand {
        name: "/users",
        usage: "/users",
        description: "show known room users",
        arg: SlashArg::None,
        web: false,
    },
    SlashCommand {
        name: "/video",
        usage: "/video",
        description: "show screen-share diagnostics",
        arg: SlashArg::None,
        web: true,
    },
    SlashCommand {
        name: "/voice",
        usage: "/voice [room]",
        description: "join a room's voice call (default: the viewed room)",
        arg: SlashArg::Room,
        web: true,
    },
    SlashCommand {
        name: "/voice-leave",
        usage: "/voice-leave",
        description: "leave the voice call",
        arg: SlashArg::None,
        web: true,
    },
    SlashCommand {
        name: "/whoami",
        usage: "/whoami",
        description: "show the current authenticated user",
        arg: SlashArg::None,
        web: true,
    },
];

/// Looks up a command by its exact `/name` token.
pub(crate) fn find_command(name: &str) -> Option<&'static SlashCommand> {
    SLASH_COMMANDS.iter().find(|command| command.name == name)
}

/// The commands exposed to the web view.
pub(crate) fn web_commands() -> impl Iterator<Item = &'static SlashCommand> {
    SLASH_COMMANDS.iter().filter(|command| command.web)
}

/// Checks that `first_token` names a web-capable command.
pub(crate) fn web_command_gate(first_token: &str) -> Result<(), String> {
    let Some(command) = find_command(first_token) else {
        return Err(format!("unknown command: {first_token}"));
    };
    if !command.web {
        return Err(format!("{first_token} is not available from the web view"));
    }
    Ok(())
}

#[derive(Default)]
pub(crate) struct CommandCompletionState {
    cycle: Option<CompletionCycle>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CompletionCycle {
    offset: u32,
    original_prefix: String,
    candidates: Vec<&'static str>,
    index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CommandContext {
    span: Span,
    token: String,
}

impl CommandCompletionState {
    pub(crate) fn inline_completion(
        &mut self,
        editor: &Editor,
        style: Style,
    ) -> Option<InlineCompletion> {
        if editor.mode() != EditorMode::Insert {
            self.cycle = None;
            return None;
        }
        let context = command_context(editor)?;
        self.clear_stale_cycle(&context);
        let candidates = matching_command_names(&context.token);
        let candidate = completion_candidate(&context.token, &candidates)?;
        Some(InlineCompletion::new(context.span, candidate, style))
    }

    pub(crate) fn complete(&mut self, editor: &mut Editor) -> bool {
        let Some(context) = command_context(editor) else {
            self.cycle = None;
            return false;
        };
        if let Some(cycle) = self.valid_cycle(&context).cloned() {
            let next = (cycle.index + 1) % cycle.candidates.len();
            let replacement = cycle.candidates[next];
            editor.replace_range(context.span, replacement);
            self.cycle = Some(CompletionCycle {
                index: next,
                ..cycle
            });
            return true;
        }

        let candidates = matching_command_names(&context.token);
        let Some((index, replacement)) = completion_candidate_index(&context.token, &candidates)
        else {
            self.cycle = None;
            return false;
        };

        editor.replace_range(context.span, replacement);
        self.cycle = Some(CompletionCycle {
            offset: context.span.offset,
            original_prefix: context.token,
            candidates,
            index,
        });
        true
    }

    pub(crate) fn clear(&mut self) {
        self.cycle = None;
    }

    fn valid_cycle(&self, context: &CommandContext) -> Option<&CompletionCycle> {
        let cycle = self.cycle.as_ref()?;
        let current = cycle.candidates.get(cycle.index)?;
        (cycle.offset == context.span.offset && context.token == *current).then_some(cycle)
    }

    fn clear_stale_cycle(&mut self, context: &CommandContext) {
        let valid = self.valid_cycle(context).is_some()
            || self.cycle.as_ref().is_some_and(|cycle| {
                cycle.offset == context.span.offset && context.token == cycle.original_prefix
            });
        if !valid {
            self.cycle = None;
        }
    }
}

pub(crate) fn slash_command_help() -> String {
    let mut out = String::from("Slash commands:\n");
    for command in SLASH_COMMANDS {
        out.push_str(command.usage);
        out.push_str(" - ");
        out.push_str(command.description);
        out.push('\n');
    }
    out.push_str("Type a prefix and press Tab to complete. Press Tab again to cycle matches.");
    out
}

fn matching_command_names(prefix: &str) -> Vec<&'static str> {
    if !prefix.starts_with('/') {
        return Vec::new();
    }
    SLASH_COMMANDS
        .iter()
        .filter(|command| command.name.starts_with(prefix))
        .map(|command| command.name)
        .collect()
}

fn completion_candidate<'a>(token: &str, candidates: &'a [&'static str]) -> Option<&'a str> {
    completion_candidate_index(token, candidates).map(|(_, candidate)| candidate)
}

fn completion_candidate_index<'a>(
    token: &str,
    candidates: &'a [&'static str],
) -> Option<(usize, &'a str)> {
    candidates
        .iter()
        .copied()
        .enumerate()
        .find(|(_, candidate)| *candidate != token)
        .or_else(|| candidates.first().copied().map(|candidate| (0, candidate)))
}

fn command_context(editor: &Editor) -> Option<CommandContext> {
    let text = editor.text();
    let cursor = editor.cursor_offset() as usize;
    if cursor != text.len() || !text.starts_with('/') || text.contains(char::is_whitespace) {
        return None;
    }
    Some(CommandContext {
        span: Span::new(0, cursor as u32),
        token: text,
    })
}

/// Messages scanned (newest first) when fuzzy-matching a reference picker
/// pattern.
const REF_PICKER_SCAN: usize = 200;
/// Matches kept for Tab cycling.
const REF_PICKER_MATCHES: usize = 8;

/// Composer completion for `@@` message references.
///
/// Typing `@@` plus a fuzzy pattern shows the best match's pill label as ghost
/// text; Tab replaces the token with that message's real `@@code`, and further
/// Tabs cycle the other matches, each hinted by its label. The ghost hint is
/// display-only: nothing in chatt calls `accept_inline_completion`, so the
/// label text can never reach the buffer.
#[derive(Default)]
pub(crate) struct RefCompletionState {
    cycle: Option<RefCycle>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RefCycle {
    offset: u32,
    original_token: String,
    candidates: Vec<RefCandidate>,
    index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RefCandidate {
    /// The text Tab inserts: `@@<code>`.
    insert: String,
    /// The target's pill label, shown as the ghost hint.
    label: String,
}

impl RefCompletionState {
    pub(crate) fn inline_completion(
        &mut self,
        editor: &Editor,
        chat: &VirtualChatBuffer,
        style: Style,
    ) -> Option<InlineCompletion> {
        if editor.mode() != EditorMode::Insert {
            self.cycle = None;
            return None;
        }
        let context = ref_context(editor)?;
        self.clear_stale_cycle(&context);
        if let Some(cycle) = self.valid_cycle(&context) {
            let label = &cycle.candidates[cycle.index].label;
            let hint = format!("{} {label}", context.token);
            return Some(InlineCompletion::new(context.span, hint, style));
        }
        if let Some(target) = decode_ref_token(&context.token) {
            let label = chat.ref_label_for(target)?;
            let hint = format!("{} {label}", context.token);
            return Some(InlineCompletion::new(context.span, hint, style));
        }
        let candidates = ref_candidates(chat, &context.token);
        let first = candidates.first()?;
        let hint = format!("{} {}", context.token, first.label);
        Some(InlineCompletion::new(context.span, hint, style))
    }

    pub(crate) fn complete(&mut self, editor: &mut Editor, chat: &VirtualChatBuffer) -> bool {
        let Some(context) = ref_context(editor) else {
            self.cycle = None;
            return false;
        };
        if let Some(cycle) = self.valid_cycle(&context).cloned() {
            let next = (cycle.index + 1) % cycle.candidates.len();
            editor.replace_range(context.span, &cycle.candidates[next].insert);
            self.cycle = Some(RefCycle {
                index: next,
                ..cycle
            });
            return true;
        }
        let candidates = ref_candidates(chat, &context.token);
        let Some(first) = candidates.first() else {
            self.cycle = None;
            return false;
        };
        editor.replace_range(context.span, &first.insert);
        self.cycle = Some(RefCycle {
            offset: context.span.offset,
            original_token: context.token,
            candidates,
            index: 0,
        });
        true
    }

    pub(crate) fn clear(&mut self) {
        self.cycle = None;
    }

    fn valid_cycle(&self, context: &CommandContext) -> Option<&RefCycle> {
        let cycle = self.cycle.as_ref()?;
        let current = cycle.candidates.get(cycle.index)?;
        (cycle.offset == context.span.offset && context.token == current.insert).then_some(cycle)
    }

    fn clear_stale_cycle(&mut self, context: &CommandContext) {
        let valid = self.valid_cycle(context).is_some()
            || self.cycle.as_ref().is_some_and(|cycle| {
                cycle.offset == context.span.offset && context.token == cycle.original_token
            });
        if !valid {
            self.cycle = None;
        }
    }
}

/// Decodes a whole `@@code` token, for hinting a pasted reference's target.
fn decode_ref_token(token: &str) -> Option<rpc::msgref::MessageRef> {
    let code = token.strip_prefix(rpc::msgref::REF_PREFIX)?;
    rpc::msgref::MessageRef::decode(code)
}

/// The whitespace-delimited `@@` token ending at the cursor, if any.
fn ref_context(editor: &Editor) -> Option<CommandContext> {
    let text = editor.text();
    let cursor = editor.cursor_offset() as usize;
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let before = &text[..cursor];
    let start = before.rfind(char::is_whitespace).map_or(0, |i| {
        i + before[i..].chars().next().map_or(1, char::len_utf8)
    });
    let token = &before[start..];
    let pattern = token.strip_prefix(rpc::msgref::REF_PREFIX)?;
    if pattern.is_empty() || pattern.contains('@') {
        return None;
    }
    Some(CommandContext {
        span: Span::new(start as u32, cursor as u32),
        token: token.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::control::ChatMessage;
    use rpc::ids::{MessageId, RoomId, UserId};

    fn chat_with(messages: &[(&str, &str)]) -> VirtualChatBuffer {
        let mut chat = VirtualChatBuffer::new(100, crate::theme::SyntaxTheme::default());
        chat.set_room_id(RoomId(1));
        for (i, (sender, body)) in messages.iter().enumerate() {
            chat.push_chat(
                ChatMessage {
                    message_id: MessageId(i as u64 + 1),
                    room_id: RoomId(1),
                    sender: UserId(1),
                    sender_name: sender.to_string(),
                    timestamp_ms: 1_000_000 + i as u64,
                    body: body.to_string(),
                    file_transfer_id: None,
                    flags: rpc::control::MessageFlags::default(),
                    target: None,
                },
                false,
            );
        }
        chat
    }

    fn insert_editor(text: &str) -> Editor {
        let mut editor = Editor::default();
        editor.set_lines(text);
        editor.enter_insert_mode();
        editor.set_cursor_offset(text.len() as u32);
        editor
    }

    #[test]
    fn ref_picker_completes_a_fuzzy_pattern_to_a_code() {
        let chat = chat_with(&[
            ("alice", "the delay manager change is in"),
            ("bob", "unrelated chatter"),
        ]);
        let mut editor = insert_editor("see @@delay");
        let mut state = RefCompletionState::default();

        let hint = state
            .inline_completion(&editor, &chat, Style::DEFAULT)
            .expect("ghost hint for a fuzzy match");
        assert!(
            hint.replacement.starts_with("@@delay @@ alice:"),
            "unexpected hint {:?}",
            hint.replacement
        );

        assert!(state.complete(&mut editor, &chat));
        let text = editor.text();
        let code = text
            .strip_prefix("see @@")
            .expect("token replaced in place");
        let target = rpc::msgref::MessageRef::decode(code).expect("inserted code decodes");
        assert_eq!(target.message_id.0, 1);
        assert_eq!(target.room_id.0, 1);
    }

    #[test]
    fn ref_picker_cycles_matches_on_repeat_tab() {
        let chat = chat_with(&[("alice", "first note"), ("alice", "second note")]);
        let mut editor = insert_editor("@@note");
        let mut state = RefCompletionState::default();

        assert!(state.complete(&mut editor, &chat));
        let first = editor.text();
        assert!(state.complete(&mut editor, &chat));
        let second = editor.text();
        assert_ne!(first, second, "second Tab must cycle to the other match");
        for text in [first, second] {
            let code = text.strip_prefix("@@").unwrap();
            assert!(rpc::msgref::MessageRef::decode(code).is_some());
        }
    }

    #[test]
    fn web_gate_allows_curated_commands() {
        let curated = [
            "/mute",
            "/unmute",
            "/deafen",
            "/undeafen",
            "/voice",
            "/voice-leave",
            "/sound",
            "/soundboard",
            "/room",
            "/dm",
            "/upload-rate",
            "/report-bug",
            "/whoami",
            "/audio",
            "/audio-reset",
            "/video",
        ];
        for name in curated {
            assert_eq!(web_command_gate(name), Ok(()), "{name} must pass the gate");
        }
        assert_eq!(web_commands().count(), curated.len());
    }

    #[test]
    fn web_gate_rejects_tui_only_and_unknown_commands() {
        assert_eq!(
            web_command_gate("/clear"),
            Err("/clear is not available from the web view".to_string())
        );
        assert_eq!(
            web_command_gate("/settings"),
            Err("/settings is not available from the web view".to_string())
        );
        assert_eq!(
            web_command_gate("/nope"),
            Err("unknown command: /nope".to_string())
        );
    }

    #[test]
    fn web_commands_all_carry_usage_and_description() {
        for command in web_commands() {
            assert!(command.usage.starts_with(command.name), "{}", command.name);
            assert!(!command.description.is_empty(), "{}", command.name);
        }
    }

    #[test]
    fn ref_picker_ignores_plain_text_and_bare_prefix() {
        let chat = chat_with(&[("alice", "hello")]);
        let mut state = RefCompletionState::default();
        for text in ["hello there", "@@", "email@@"] {
            let mut editor = insert_editor(text);
            assert!(
                state
                    .inline_completion(&editor, &chat, Style::DEFAULT)
                    .is_none(),
                "no completion expected for {text:?}"
            );
            assert!(!state.complete(&mut editor, &chat));
            assert_eq!(editor.text(), text);
        }
    }
}

fn ref_candidates(chat: &VirtualChatBuffer, token: &str) -> Vec<RefCandidate> {
    let pattern = &token[rpc::msgref::REF_PREFIX.len()..];
    let mut scored: Vec<(i32, usize)> = Vec::new();
    let len = chat.len();
    for index in (len.saturating_sub(REF_PICKER_SCAN)..len).rev() {
        let entry = chat.message(index);
        if entry.timestamp_ms == 0 {
            continue;
        }
        let first_line = entry.body.lines().next().unwrap_or("");
        let haystack = format!("{} {first_line}", entry.sender);
        let Some(score) = crate::fuzzy::fuzzy_score(pattern, &haystack) else {
            continue;
        };
        scored.push((score, index));
    }
    // Stable sort keeps newest-first among equal scores, since the scan ran
    // newest to oldest.
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.truncate(REF_PICKER_MATCHES);
    let mut candidates = Vec::with_capacity(scored.len());
    for (_, index) in scored {
        let Some((target, label)) = chat.ref_for_index(index) else {
            continue;
        };
        candidates.push(RefCandidate {
            insert: format!("{}{}", rpc::msgref::REF_PREFIX, target.encode()),
            label,
        });
    }
    candidates
}
