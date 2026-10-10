//! Small modal dialogs opened from context menus: a single text field (rename,
//! worktree branch) or a confirmation for destructive actions.

use gpui::{div, prelude::*, px, Context, FocusHandle, Keystroke, SharedString};
use serde_json::Value;

use super::app::GuiApp;
use super::menu::Command;
use super::theme::Theme;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DialogSubmit {
    /// Send `method` with `params`, putting the field text under `field`.
    /// An empty optional field is omitted (for example, clearing a pane name).
    Request {
        method: &'static str,
        params: Value,
        field: &'static str,
        required: bool,
    },
    RenameMachine,
    /// Confirmation only.
    Command(Box<Command>),
}

impl DialogSubmit {
    pub(crate) fn request(
        method: &'static str,
        params: Value,
        field: &'static str,
        required: bool,
    ) -> Self {
        Self::Request {
            method,
            params,
            field,
            required,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Dialog {
    title: String,
    message: Option<String>,
    /// Field label and current text, for input dialogs.
    field: Option<(String, String)>,
    confirm_label: String,
    danger: bool,
    submit: DialogSubmit,
    pub(crate) error: Option<String>,
}

pub(crate) enum DialogOutcome {
    Pending,
    Cancel,
    Run(Command),
    RenameMachine(String),
}

impl Dialog {
    pub(crate) fn input(
        title: &str,
        label: &str,
        value: &str,
        confirm_label: &str,
        submit: DialogSubmit,
    ) -> Self {
        Self {
            title: title.to_owned(),
            message: None,
            field: Some((label.to_owned(), value.to_owned())),
            confirm_label: confirm_label.to_owned(),
            danger: false,
            submit,
            error: None,
        }
    }

    pub(crate) fn confirm(
        title: impl Into<String>,
        message: &str,
        confirm_label: &str,
        danger: bool,
        command: Command,
    ) -> Self {
        Self {
            title: title.into(),
            message: Some(message.to_owned()),
            field: None,
            confirm_label: confirm_label.to_owned(),
            danger,
            submit: DialogSubmit::Command(Box::new(command)),
            error: None,
        }
    }

    pub(crate) fn insert_text(&mut self, text: &str) {
        if let Some((_, value)) = self.field.as_mut() {
            value.extend(text.chars().filter(|ch| !ch.is_control()));
        }
    }

    pub(crate) fn key(&mut self, keystroke: &Keystroke) -> DialogOutcome {
        let modifiers = &keystroke.modifiers;
        match keystroke.key.as_str() {
            "escape" => return DialogOutcome::Cancel,
            "enter" => return self.submit(),
            "backspace" => {
                if let Some((_, value)) = self.field.as_mut() {
                    value.pop();
                }
            }
            _ if !(modifiers.control || modifiers.platform) => {
                if let Some(text) = keystroke.key_char.clone() {
                    self.insert_text(&text);
                }
            }
            _ => {}
        }
        DialogOutcome::Pending
    }

    pub(crate) fn submit(&mut self) -> DialogOutcome {
        let text = self
            .field
            .as_ref()
            .map(|(_, value)| value.trim().to_owned())
            .unwrap_or_default();
        match &self.submit {
            DialogSubmit::Command(command) => DialogOutcome::Run((**command).clone()),
            DialogSubmit::RenameMachine if text.is_empty() => {
                self.error = Some("Enter a name".into());
                DialogOutcome::Pending
            }
            DialogSubmit::RenameMachine => DialogOutcome::RenameMachine(text),
            DialogSubmit::Request {
                method,
                params,
                field,
                required,
            } => {
                if text.is_empty() && *required {
                    self.error = Some("This can't be empty".into());
                    return DialogOutcome::Pending;
                }
                let mut params = params.clone();
                if !text.is_empty() {
                    if let Value::Object(map) = &mut params {
                        map.insert((*field).to_owned(), Value::String(text));
                    }
                }
                DialogOutcome::Run(Command::Requests(vec![(*method, params)]))
            }
        }
    }

    pub(crate) fn render(
        &self,
        theme: &Theme,
        focus: &FocusHandle,
        cx: &mut Context<GuiApp>,
    ) -> impl IntoElement {
        let confirm_bg = if self.danger {
            theme.danger
        } else {
            theme.accent
        };
        div()
            .id("dialog-backdrop")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.5))
            .child(
                div()
                    .track_focus(focus)
                    .on_key_down(cx.listener(GuiApp::on_dialog_key))
                    .w(px(420.0))
                    .p(px(20.0))
                    .flex()
                    .flex_col()
                    .gap(px(14.0))
                    .rounded(px(10.0))
                    .bg(theme.panel_bg)
                    .border_1()
                    .border_color(theme.border)
                    .child(div().text_lg().child(self.title.clone()))
                    .when_some(self.message.clone(), |dialog, message| {
                        dialog.child(div().text_sm().text_color(theme.text_muted).child(message))
                    })
                    .when_some(self.field.clone(), |dialog, (label, value)| {
                        dialog.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(4.0))
                                .child(div().text_xs().text_color(theme.text_muted).child(label))
                                .child(
                                    div()
                                        .flex()
                                        .px(px(10.0))
                                        .py(px(6.0))
                                        .rounded(px(6.0))
                                        .border_1()
                                        .border_color(theme.accent)
                                        .bg(theme.sidebar_bg)
                                        .child(SharedString::from(value))
                                        .child(div().w(px(1.0)).h(px(16.0)).bg(theme.accent)),
                                ),
                        )
                    })
                    .when_some(self.error.clone(), |dialog, error| {
                        dialog.child(div().text_sm().text_color(theme.danger).child(error))
                    })
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap(px(8.0))
                            .text_sm()
                            .child(
                                div()
                                    .id("dialog-cancel")
                                    .px(px(12.0))
                                    .py(px(6.0))
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .hover(|style| style.bg(theme.hover))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_dialog(window);
                                        cx.notify();
                                    }))
                                    .child("Cancel"),
                            )
                            .child(
                                div()
                                    .id("dialog-confirm")
                                    .px(px(12.0))
                                    .py(px(6.0))
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .bg(confirm_bg)
                                    .text_color(theme.panel_bg)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_dialog(window, cx);
                                        cx.notify();
                                    }))
                                    .child(self.confirm_label.clone()),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn typed(dialog: &mut Dialog, text: &str) {
        dialog.insert_text(text);
    }

    #[test]
    fn input_dialog_puts_text_into_the_request_field() {
        let mut dialog = Dialog::input(
            "Rename tab",
            "Name",
            "",
            "Rename",
            DialogSubmit::request("tab.rename", json!({ "tab_id": "t1" }), "label", true),
        );
        assert!(matches!(dialog.submit(), DialogOutcome::Pending));
        assert!(dialog.error.is_some());
        typed(&mut dialog, " build ");
        match dialog.submit() {
            DialogOutcome::Run(Command::Requests(requests)) => assert_eq!(
                requests,
                vec![("tab.rename", json!({ "tab_id": "t1", "label": "build" }))]
            ),
            _ => panic!("expected a request"),
        }
    }

    #[test]
    fn empty_optional_field_is_omitted() {
        let mut dialog = Dialog::input(
            "Rename pane",
            "Name",
            "",
            "Rename",
            DialogSubmit::request("pane.rename", json!({ "pane_id": "p1" }), "label", false),
        );
        match dialog.submit() {
            DialogOutcome::Run(Command::Requests(requests)) => {
                assert_eq!(requests, vec![("pane.rename", json!({ "pane_id": "p1" }))])
            }
            _ => panic!("expected a request"),
        }
    }

    #[test]
    fn confirmation_runs_its_command() {
        let mut dialog = Dialog::confirm("Remove?", "msg", "Remove", true, Command::RemoveMachine);
        assert!(matches!(
            dialog.submit(),
            DialogOutcome::Run(Command::RemoveMachine)
        ));
    }
}
