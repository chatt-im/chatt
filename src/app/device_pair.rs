use extui::event::{KeyEvent, MouseEvent};
use extui::{Buffer, Rect};
use unicode_width::UnicodeWidthStr;
use zeroize::Zeroize;

use crate::{
    config::FormBindings,
    theme::Theme,
    tui::form::{FormAction, FormFieldKind, FormMouseIntent},
    ui::form::{self, ActionButton, Commit, FieldIntent, Form, FormSurface},
};

const PAIR_SECTION: &str = "Device pairing";
const LINK_SECTION: &str = "Device link";
const LABEL_WIDTH: u16 = 19;
const PAIR_DESCRIPTION: &str =
    "Paste the one-time link from an existing device, then name this installation.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DevicePairButton {
    Cancel,
    Close,
    Pair,
}

pub(crate) enum DevicePairEvent {
    Consumed,
    Cancel,
    Close,
    Submit {
        pairing_string: String,
        transfer_password: String,
        device_name: String,
        overwrite_existing: bool,
    },
}

pub(crate) struct DevicePairDialog {
    pairing_string: String,
    transfer_password: String,
    device_name: String,
    show_secrets: bool,
    feedback: String,
    feedback_error: bool,
    submitting: bool,
    confirm_overwrite: bool,
    form: form::State,
}

impl DevicePairDialog {
    pub(crate) fn new(pairing_string: String, bindings: FormBindings) -> Self {
        let initial_field = if pairing_string.trim().is_empty() {
            "Pairing string"
        } else {
            "Transfer password"
        };
        Self {
            pairing_string,
            transfer_password: String::new(),
            device_name: String::new(),
            show_secrets: false,
            feedback: "Enter the one-time link details".to_string(),
            feedback_error: false,
            submitting: false,
            confirm_overwrite: false,
            form: form::state_with_focus(bindings, PAIR_SECTION, initial_field),
        }
    }

    pub(crate) fn form_height(&self, terminal_width: u16) -> u16 {
        let width = dialog_body_width(terminal_width);
        form::wrapped_line_count(PAIR_DESCRIPTION, width)
            .saturating_add(form::wrapped_line_count(&self.feedback, width))
            .saturating_add(7)
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        self.form.begin_frame(area);
        {
            let mut form = Form::new(
                &mut self.form,
                Some(buf),
                theme,
                false,
                FieldIntent::None,
                None,
                None,
            )
            .with_label_width(LABEL_WIDTH)
            .with_surface(FormSurface::Dialog);
            device_pair_form(
                &mut form,
                &mut self.pairing_string,
                &mut self.transfer_password,
                &mut self.device_name,
                &mut self.show_secrets,
                &self.feedback,
                self.feedback_error,
                self.confirm_overwrite,
            );
        }
        self.form.finish_frame();
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent, theme: &Theme) -> DevicePairEvent {
        let kind = self.form.focused_kind();
        let text_focused = kind == FormFieldKind::Text;
        let event = self.form.handle_key(key, kind);
        match event.action {
            FormAction::None | FormAction::Scrolled => {
                self.drive(theme, FieldIntent::None, event.commit, None);
            }
            FormAction::TextChanged => {
                self.confirm_overwrite = false;
                self.feedback = "Details changed; retry pairing to check local identity state"
                    .to_string();
                self.feedback_error = false;
                self.drive(theme, FieldIntent::None, event.commit, None);
            }
            FormAction::Cancel => return DevicePairEvent::Cancel,
            FormAction::FocusMoved => {
                self.drive(theme, FieldIntent::None, event.commit, None);
            }
            FormAction::Adjust(delta) => {
                self.drive(theme, FieldIntent::Adjust(delta), event.commit, None);
            }
            FormAction::MoveFocus(delta) => self.move_focus(theme, delta, false),
            FormAction::ActivateNextInsert => {
                self.drive(theme, FieldIntent::None, event.commit, None);
                self.move_focus(theme, 1, true);
            }
            FormAction::Activate if text_focused => {
                self.drive(theme, FieldIntent::None, event.commit, None);
                self.move_focus(theme, 1, false);
            }
            FormAction::Activate => {
                if let Some(button) = self.drive(theme, FieldIntent::Activate, event.commit, None) {
                    return self.activate(button);
                }
            }
        }
        DevicePairEvent::Consumed
    }

    pub(crate) fn handle_mouse(&mut self, mouse: MouseEvent, theme: &Theme) -> DevicePairEvent {
        let event = self.form.handle_mouse(mouse);
        match event.intent {
            FormMouseIntent::None => {
                self.drive(theme, FieldIntent::None, event.commit, None);
            }
            FormMouseIntent::Activate(_) => {
                if let Some(button) = self.drive(theme, FieldIntent::Activate, event.commit, None) {
                    return self.activate(button);
                }
            }
            FormMouseIntent::Adjust(_, delta) => {
                self.drive(theme, FieldIntent::Adjust(delta), event.commit, None);
            }
            FormMouseIntent::Text(_, _, column) => {
                self.drive(theme, FieldIntent::None, event.commit, Some(column));
            }
            FormMouseIntent::PickerItem(_, _) => {}
        }
        DevicePairEvent::Consumed
    }

    pub(crate) fn paste(&mut self, text: &str, theme: &Theme) {
        if let Some(commit) = self.form.replace_active_text(text.trim()) {
            self.drive(theme, FieldIntent::None, Some(commit), None);
        }
    }

    pub(crate) fn pairing_failed(&mut self, error: String) {
        self.submitting = false;
        self.confirm_overwrite = false;
        self.feedback = error;
        self.feedback_error = true;
    }

    pub(crate) fn device_pairing_failed(&mut self, error: String, transfer_password: String) {
        self.transfer_password = transfer_password;
        self.pairing_failed(error);
    }

    pub(crate) fn identity_exists(&mut self, message: String, transfer_password: String) {
        self.transfer_password = transfer_password;
        self.submitting = false;
        self.confirm_overwrite = true;
        self.feedback = message;
        self.feedback_error = true;
    }

    fn activate(&mut self, button: DevicePairButton) -> DevicePairEvent {
        match button {
            DevicePairButton::Cancel => DevicePairEvent::Cancel,
            DevicePairButton::Close => DevicePairEvent::Close,
            DevicePairButton::Pair => {
                if self.submitting {
                    return DevicePairEvent::Consumed;
                }
                if let Some(error) = pair_validation(
                    &self.pairing_string,
                    &self.transfer_password,
                    &self.device_name,
                ) {
                    self.feedback = error;
                    self.feedback_error = true;
                    return DevicePairEvent::Consumed;
                }
                self.submitting = true;
                let overwrite_existing = self.confirm_overwrite;
                self.feedback = if overwrite_existing {
                    "Overwriting local identity and linking device...".to_string()
                } else {
                    "Linking device...".to_string()
                };
                self.feedback_error = false;
                DevicePairEvent::Submit {
                    pairing_string: self.pairing_string.trim().to_string(),
                    transfer_password: std::mem::take(&mut self.transfer_password),
                    device_name: self.device_name.trim().to_string(),
                    overwrite_existing,
                }
            }
        }
    }

    fn move_focus(&mut self, theme: &Theme, delta: isize, insert: bool) {
        let commit = self.form.move_focus(delta);
        self.drive(theme, FieldIntent::None, commit, None);
        if insert {
            self.form.enter_insert_mode();
        }
    }

    fn drive(
        &mut self,
        theme: &Theme,
        intent: FieldIntent,
        commit: Option<Commit>,
        focus_column: Option<u16>,
    ) -> Option<DevicePairButton> {
        let viewport = self.form.viewport();
        self.form.begin_frame(viewport);
        let activated = {
            let mut form = Form::new(
                &mut self.form,
                None,
                theme,
                false,
                intent,
                commit,
                focus_column,
            )
            .with_label_width(LABEL_WIDTH)
            .with_surface(FormSurface::Dialog);
            device_pair_form(
                &mut form,
                &mut self.pairing_string,
                &mut self.transfer_password,
                &mut self.device_name,
                &mut self.show_secrets,
                &self.feedback,
                self.feedback_error,
                self.confirm_overwrite,
            )
        };
        self.form.finish_frame();
        activated
    }
}

impl Drop for DevicePairDialog {
    fn drop(&mut self) {
        self.pairing_string.zeroize();
        self.transfer_password.zeroize();
    }
}

fn device_pair_form(
    form: &mut Form<'_>,
    pairing_string: &mut String,
    transfer_password: &mut String,
    device_name: &mut String,
    show_secrets: &mut bool,
    feedback: &str,
    feedback_error: bool,
    confirm_overwrite: bool,
) -> Option<DevicePairButton> {
    form.section_with_id("Enrollment", PAIR_SECTION);
    form.description(PAIR_DESCRIPTION);
    if *show_secrets {
        form.text("Pairing string", pairing_string, required);
        form.text("Transfer password", transfer_password, required);
    } else {
        form.secret_text("Pairing string", pairing_string, required);
        form.secret_text("Transfer password", transfer_password, required);
    }
    form.text("Device name", device_name, device_name_error);
    form.checkbox("Show secrets", show_secrets);
    form.description_with_error(feedback, feedback_error);
    form.spacer(1);
    let activated = form
        .actions(&[
            ActionButton {
                key: "cancel",
                label: "Cancel",
                value: DevicePairButton::Cancel,
                help: "Cancel device pairing.",
            },
            ActionButton {
                key: "pair",
                label: if confirm_overwrite {
                    "Overwrite & pair"
                } else {
                    "Pair"
                },
                value: DevicePairButton::Pair,
                help: if confirm_overwrite {
                    "Replace the existing local identity, then redeem this one-time link."
                } else {
                    "Redeem this one-time link and create the device identity."
                },
            },
            ActionButton {
                key: "close",
                label: "Close",
                value: DevicePairButton::Close,
                help: "Hide this dialog without canceling a submitted pairing attempt.",
            },
        ])
        .activated;
    activated
}

fn required(value: &str) -> Option<String> {
    value.trim().is_empty().then(|| "required".to_string())
}

fn device_name_error(value: &str) -> Option<String> {
    let value = value.trim();
    (value.is_empty()
        || value.len() > rpc::e2e::MAX_DEVICE_NAME_BYTES
        || value.chars().any(char::is_control))
    .then(|| "must be 1-64 bytes with no control characters".to_string())
}

fn pair_validation(ticket: &str, password: &str, name: &str) -> Option<String> {
    if ticket.trim().is_empty() || password.trim().is_empty() {
        Some("Pairing string and transfer password are required".to_string())
    } else {
        device_name_error(name)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeviceLinkButton {
    CopyTicket,
    CopyPassword,
    GenerateNew,
    CancelLink,
    Close,
}

pub(crate) struct DeviceLinkDialog {
    redemption_secret_hash: Vec<u8>,
    pairing_string: String,
    transfer_password: String,
    expires_at_ms: u64,
    now_ms: u64,
    generate_armed: bool,
    form: form::State,
}

impl DeviceLinkDialog {
    pub(crate) fn new(
        redemption_secret_hash: Vec<u8>,
        pairing_string: String,
        transfer_password: String,
        expires_at_ms: u64,
        bindings: FormBindings,
    ) -> Self {
        Self {
            redemption_secret_hash,
            pairing_string,
            transfer_password,
            expires_at_ms,
            now_ms: 0,
            generate_armed: false,
            form: form::state_with_focus(bindings, LINK_SECTION, "copy-ticket"),
        }
    }

    pub(crate) fn form_height(&self, terminal_width: u16) -> u16 {
        let value_width = dialog_body_width(terminal_width)
            .saturating_sub(LABEL_WIDTH)
            .max(1) as usize;
        let ticket_rows = self.pairing_string.width().div_ceil(value_width);
        u16::try_from(ticket_rows)
            .unwrap_or(u16::MAX)
            .saturating_add(5)
    }

    pub(crate) fn render(
        &mut self,
        area: Rect,
        buf: &mut Buffer,
        theme: &Theme,
        now_ms: u64,
    ) {
        self.now_ms = now_ms;
        self.form.begin_frame(area);
        let mut form = Form::new(
            &mut self.form,
            Some(buf),
            theme,
            false,
            FieldIntent::None,
            None,
            None,
        )
        .with_label_width(LABEL_WIDTH)
        .with_surface(FormSurface::Dialog);
        device_link_form(
            &mut form,
            &self.pairing_string,
            &self.transfer_password,
            self.expires_at_ms,
            self.now_ms,
            self.generate_armed,
        );
        self.form.finish_frame();
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent, theme: &Theme) -> Option<DeviceLinkButton> {
        let kind = self.form.focused_kind();
        let event = self.form.handle_key(key, kind);
        match event.action {
            FormAction::Cancel => Some(DeviceLinkButton::Close),
            FormAction::MoveFocus(delta) => {
                let commit = self.form.move_focus(delta);
                self.drive(theme, FieldIntent::None, commit, None);
                None
            }
            FormAction::Activate => {
                let button = self.drive(theme, FieldIntent::Activate, event.commit, None);
                self.activate(button)
            }
            FormAction::Adjust(delta) => {
                self.drive(theme, FieldIntent::Adjust(delta), event.commit, None);
                None
            }
            _ => {
                self.drive(theme, FieldIntent::None, event.commit, None);
                None
            }
        }
    }
    pub(crate) fn handle_mouse(
        &mut self,
        mouse: MouseEvent,
        theme: &Theme,
    ) -> Option<DeviceLinkButton> {
        let event = self.form.handle_mouse(mouse);
        match event.intent {
            FormMouseIntent::Activate(_) => {
                let button = self.drive(theme, FieldIntent::Activate, event.commit, None);
                self.activate(button)
            }
            FormMouseIntent::Text(_, _, column) => {
                self.drive(theme, FieldIntent::None, event.commit, Some(column));
                None
            }
            FormMouseIntent::Adjust(_, delta) => {
                self.drive(theme, FieldIntent::Adjust(delta), event.commit, None);
                None
            }
            _ => {
                self.drive(theme, FieldIntent::None, event.commit, None);
                None
            }
        }
    }
    pub(crate) fn value(&self, button: DeviceLinkButton) -> Option<&str> {
        if self.now_ms >= self.expires_at_ms {
            return None;
        }
        match button {
            DeviceLinkButton::CopyTicket => Some(&self.pairing_string),
            DeviceLinkButton::CopyPassword => Some(&self.transfer_password),
            DeviceLinkButton::GenerateNew
            | DeviceLinkButton::CancelLink
            | DeviceLinkButton::Close => None,
        }
    }

    pub(crate) fn redemption_secret_hash(&self) -> &[u8] {
        &self.redemption_secret_hash
    }

    fn activate(&mut self, button: Option<DeviceLinkButton>) -> Option<DeviceLinkButton> {
        match button {
            Some(DeviceLinkButton::GenerateNew) if !self.generate_armed => {
                self.generate_armed = true;
                None
            }
            Some(button) => {
                self.generate_armed = false;
                Some(button)
            }
            None => None,
        }
    }
    fn drive(
        &mut self,
        theme: &Theme,
        intent: FieldIntent,
        commit: Option<Commit>,
        focus_column: Option<u16>,
    ) -> Option<DeviceLinkButton> {
        let viewport = self.form.viewport();
        self.form.begin_frame(viewport);
        let activated = {
            let mut form = Form::new(
                &mut self.form,
                None,
                theme,
                false,
                intent,
                commit,
                focus_column,
            )
            .with_label_width(LABEL_WIDTH)
            .with_surface(FormSurface::Dialog);
            device_link_form(
                &mut form,
                &self.pairing_string,
                &self.transfer_password,
                self.expires_at_ms,
                self.now_ms,
                self.generate_armed,
            )
        };
        self.form.finish_frame();
        activated
    }
}

fn dialog_body_width(terminal_width: u16) -> u16 {
    terminal_width.saturating_sub(4).min(112).saturating_sub(4)
}

impl Drop for DeviceLinkDialog {
    fn drop(&mut self) {
        self.pairing_string.zeroize();
        self.transfer_password.zeroize();
    }
}

fn device_link_form(
    form: &mut Form<'_>,
    ticket: &str,
    password: &str,
    expires_at_ms: u64,
    now_ms: u64,
    generate_armed: bool,
) -> Option<DeviceLinkButton> {
    form.section_with_id("One-time link", LINK_SECTION);
    form.wrapped_static_row("Pairing string", ticket);
    form.static_row("Password", password);
    let expiry = device_link_expiry(expires_at_ms, now_ms);
    form.static_row("Expires", &expiry);
    form.spacer(1);
    let generate_label = if generate_armed {
        "Confirm new"
    } else {
        "Generate new"
    };
    form.actions(&[
        ActionButton {
            key: "cancel-link",
            label: "Cancel link",
            value: DeviceLinkButton::CancelLink,
            help: "Immediately invalidate these exposed link credentials.",
        },
        ActionButton {
            key: "copy-ticket",
            label: "Copy ticket",
            value: DeviceLinkButton::CopyTicket,
            help: "Copy the one-time pairing string.",
        },
        ActionButton {
            key: "copy-password",
            label: "Copy password",
            value: DeviceLinkButton::CopyPassword,
            help: "Copy the six-word transfer password.",
        },
        ActionButton {
            key: "generate-new",
            label: generate_label,
            value: DeviceLinkButton::GenerateNew,
            help: "Activate twice to replace this link with a newly generated link.",
        },
        ActionButton {
            key: "close",
            label: "Close",
            value: DeviceLinkButton::Close,
            help: "Close this link dialog.",
        },
    ])
    .activated
}

fn device_link_expiry(expires_at_ms: u64, now_ms: u64) -> String {
    let remaining_seconds = expires_at_ms.saturating_sub(now_ms).div_ceil(1_000);
    if remaining_seconds == 0 {
        "Expired — generate a new link".to_string()
    } else {
        format!(
            "{:02}:{:02} remaining; one use",
            remaining_seconds / 60,
            remaining_seconds % 60
        )
    }
}

#[cfg(test)]
mod tests {
    use extui::event::{KeyCode, KeyModifiers};

    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn render_pair(dialog: &mut DevicePairDialog, theme: &Theme) {
        let mut buf = Buffer::new(80, dialog.form_height(80));
        dialog.render(buf.rect(), &mut buf, theme);
    }

    #[test]
    fn device_link_countdown_rounds_up_and_expires() {
        assert_eq!(device_link_expiry(70_001, 10_000), "01:01 remaining; one use");
        assert_eq!(device_link_expiry(70_000, 10_000), "01:00 remaining; one use");
        assert_eq!(
            device_link_expiry(70_000, 70_000),
            "Expired — generate a new link"
        );
    }

    #[test]
    fn generating_a_replacement_link_requires_two_activations() {
        let mut dialog = DeviceLinkDialog::new(
            vec![0; 32],
            "ticket".to_string(),
            "password".to_string(),
            60_000,
            FormBindings::Standard,
        );
        assert_eq!(dialog.activate(Some(DeviceLinkButton::GenerateNew)), None);
        assert!(dialog.generate_armed);
        assert_eq!(
            dialog.activate(Some(DeviceLinkButton::GenerateNew)),
            Some(DeviceLinkButton::GenerateNew)
        );
        assert!(!dialog.generate_armed);
    }

    #[test]
    fn explicit_link_cancel_is_distinct_from_closing_the_dialog() {
        let hash = vec![7; 32];
        let mut dialog = DeviceLinkDialog::new(
            hash.clone(),
            "ticket".to_string(),
            "password".to_string(),
            60_000,
            FormBindings::Standard,
        );

        assert_eq!(dialog.redemption_secret_hash(), hash);
        assert_eq!(
            dialog.activate(Some(DeviceLinkButton::CancelLink)),
            Some(DeviceLinkButton::CancelLink)
        );
        assert_eq!(
            dialog.activate(Some(DeviceLinkButton::Close)),
            Some(DeviceLinkButton::Close)
        );
    }

    #[test]
    fn submitted_device_pairing_can_be_closed_without_canceling() {
        let mut dialog = DevicePairDialog::new("ticket".to_string(), FormBindings::Standard);
        dialog.submitting = true;

        assert!(matches!(
            dialog.activate(DevicePairButton::Close),
            DevicePairEvent::Close
        ));
        assert!(matches!(
            dialog.activate(DevicePairButton::Cancel),
            DevicePairEvent::Cancel
        ));
    }

    #[test]
    fn standard_bindings_move_through_shared_form_before_submitting() {
        let theme = Theme::tomorrow_night();
        let mut dialog =
            DevicePairDialog::new("tcd1_ticket".to_string(), FormBindings::Standard);
        render_pair(&mut dialog, &theme);

        dialog.paste("coral-lantern", &theme);
        dialog.handle_key(key(KeyCode::Enter), &theme);
        dialog.paste("Alice's laptop", &theme);
        dialog.handle_key(key(KeyCode::Enter), &theme);
        dialog.handle_key(key(KeyCode::Tab), &theme);
        dialog.handle_key(key(KeyCode::Right), &theme);

        match dialog.handle_key(key(KeyCode::Enter), &theme) {
            DevicePairEvent::Submit {
                pairing_string,
                transfer_password,
                device_name,
                overwrite_existing,
            } => {
                assert_eq!(pairing_string, "tcd1_ticket");
                assert_eq!(transfer_password, "coral-lantern");
                assert_eq!(device_name, "Alice's laptop");
                assert!(!overwrite_existing);
            }
            _ => panic!("Pair action did not submit the completed form"),
        }
    }

    #[test]
    fn vim_bindings_drive_the_same_fields_and_actions() {
        let theme = Theme::tomorrow_night();
        let mut dialog =
            DevicePairDialog::new("tcd1_ticket".to_string(), FormBindings::Vim);
        render_pair(&mut dialog, &theme);

        dialog.paste("coral-lantern", &theme);
        dialog.handle_key(key(KeyCode::Char('j')), &theme);
        dialog.paste("Alice's laptop", &theme);
        dialog.handle_key(key(KeyCode::Char('j')), &theme);
        dialog.handle_key(key(KeyCode::Char('j')), &theme);
        dialog.handle_key(key(KeyCode::Char('l')), &theme);

        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter), &theme),
            DevicePairEvent::Submit { .. }
        ));
    }

    #[test]
    fn empty_pairing_prompt_starts_on_ticket_and_can_reveal_secrets() {
        let theme = Theme::tomorrow_night();
        let mut dialog = DevicePairDialog::new(String::new(), FormBindings::Standard);
        render_pair(&mut dialog, &theme);

        dialog.paste("tcd1_ticket", &theme);
        dialog.handle_key(key(KeyCode::Enter), &theme);
        dialog.paste("coral-lantern", &theme);
        dialog.handle_key(key(KeyCode::Enter), &theme);
        dialog.paste("Alice's laptop", &theme);
        dialog.handle_key(key(KeyCode::Enter), &theme);
        assert!(!dialog.show_secrets);

        dialog.handle_key(key(KeyCode::Enter), &theme);

        assert!(dialog.show_secrets);
        assert_eq!(dialog.pairing_string, "tcd1_ticket");
        assert_eq!(dialog.transfer_password, "coral-lantern");
    }

    #[test]
    fn existing_identity_requires_explicit_overwrite_submission() {
        let mut dialog =
            DevicePairDialog::new("tcd1_ticket".to_string(), FormBindings::Standard);
        dialog.transfer_password = "coral-lantern".to_string();
        dialog.device_name = "Alice's laptop".to_string();
        dialog.identity_exists(
            "Existing identity found. Overwrite it?".to_string(),
            "coral-lantern".to_string(),
        );

        assert!(dialog.confirm_overwrite);
        assert_eq!(dialog.transfer_password, "coral-lantern");
        match dialog.activate(DevicePairButton::Pair) {
            DevicePairEvent::Submit {
                overwrite_existing,
                transfer_password,
                ..
            } => {
                assert!(overwrite_existing);
                assert_eq!(transfer_password, "coral-lantern");
            }
            _ => panic!("overwrite confirmation did not resubmit pairing"),
        }
    }

    #[test]
    fn link_height_reserves_every_pairing_string_row() {
        let dialog = DeviceLinkDialog::new(
            vec![0; 32],
            format!("tcd1_{}", "a".repeat(180)),
            "coral-lantern".to_string(),
            0,
            FormBindings::Standard,
        );

        assert_eq!(dialog.form_height(80), 9);
    }
}
