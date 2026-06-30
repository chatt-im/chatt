use extui::Style;
use extui_editor::{Editor, InlineCompletion, Mode as EditorMode, Span};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SlashCommand {
    pub(crate) name: &'static str,
    pub(crate) usage: &'static str,
    pub(crate) description: &'static str,
}

pub(crate) const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/audio",
        usage: "/audio",
        description: "show receive and playback diagnostics",
    },
    SlashCommand {
        name: "/clear",
        usage: "/clear",
        description: "clear the local chat view",
    },
    SlashCommand {
        name: "/config",
        usage: "/config",
        description: "open settings",
    },
    SlashCommand {
        name: "/deafen",
        usage: "/deafen",
        description: "stop playback and mute microphone send",
    },
    SlashCommand {
        name: "/deafened",
        usage: "/deafened",
        description: "show deafen status",
    },
    SlashCommand {
        name: "/help",
        usage: "/help",
        description: "show this command list",
    },
    SlashCommand {
        name: "/mute",
        usage: "/mute",
        description: "mute microphone send",
    },
    SlashCommand {
        name: "/muted",
        usage: "/muted",
        description: "show microphone mute status",
    },
    SlashCommand {
        name: "/quit",
        usage: "/quit",
        description: "show the quit key hint",
    },
    SlashCommand {
        name: "/report-bug",
        usage: "/report-bug what went wrong",
        description: "send recent logs and diagnostics to the server",
    },
    SlashCommand {
        name: "/servers",
        usage: "/servers",
        description: "open the server list",
    },
    SlashCommand {
        name: "/settings",
        usage: "/settings",
        description: "open settings",
    },
    SlashCommand {
        name: "/sound",
        usage: "/sound N|name",
        description: "play a soundboard clip",
    },
    SlashCommand {
        name: "/soundboard",
        usage: "/soundboard",
        description: "list soundboard clips",
    },
    SlashCommand {
        name: "/undeafen",
        usage: "/undeafen",
        description: "resume playback and microphone send",
    },
    SlashCommand {
        name: "/unmute",
        usage: "/unmute",
        description: "unmute microphone send",
    },
    SlashCommand {
        name: "/upload",
        usage: "/upload path/to/file.ext",
        description: "relay a file to room members",
    },
    SlashCommand {
        name: "/users",
        usage: "/users",
        description: "show known room users",
    },
    SlashCommand {
        name: "/whoami",
        usage: "/whoami",
        description: "show the current authenticated user",
    },
];

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
