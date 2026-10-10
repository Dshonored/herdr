//! The "add SSH machine" dialog. Saved machines go into the same endpoint
//! catalog the TUI uses, so a machine added here also appears in `herdr`.

use gpui::{div, prelude::*, px, Context, FocusHandle, Keystroke, SharedString};

use super::app::GuiApp;
use super::theme::Theme;

const FIELDS: [(&str, &str); 3] = [
    ("Name", "devbox"),
    ("SSH target", "user@host or an ~/.ssh/config alias"),
    ("Session", crate::session::DEFAULT_SESSION_NAME),
];

pub(crate) enum HostFormOutcome {
    Pending,
    Cancel,
    Submit {
        label: String,
        target: String,
        session: String,
    },
}

#[derive(Default)]
pub(crate) struct HostForm {
    values: [String; 3],
    focused: usize,
    pub(crate) error: Option<String>,
}

impl HostForm {
    pub(crate) fn key(&mut self, keystroke: &Keystroke) -> HostFormOutcome {
        let modifiers = &keystroke.modifiers;
        match keystroke.key.as_str() {
            "escape" => return HostFormOutcome::Cancel,
            "enter" => return self.submit(),
            "tab" if modifiers.shift => {
                self.focused = (self.focused + FIELDS.len() - 1) % FIELDS.len()
            }
            "tab" | "down" => self.focused = (self.focused + 1) % FIELDS.len(),
            "up" => self.focused = (self.focused + FIELDS.len() - 1) % FIELDS.len(),
            "backspace" => {
                self.values[self.focused].pop();
            }
            _ if !(modifiers.control || modifiers.platform) => {
                if let Some(text) = &keystroke.key_char {
                    self.values[self.focused].extend(text.chars().filter(|ch| !ch.is_control()));
                }
            }
            _ => {}
        }
        HostFormOutcome::Pending
    }

    fn submit(&mut self) -> HostFormOutcome {
        let target = self.values[1].trim().to_owned();
        if target.is_empty() {
            self.error = Some("Enter an SSH target".into());
            self.focused = 1;
            return HostFormOutcome::Pending;
        }
        let label = match self.values[0].trim() {
            "" => target.clone(),
            label => label.to_owned(),
        };
        let session = match self.values[2].trim() {
            "" => crate::session::DEFAULT_SESSION_NAME.to_owned(),
            session => session.to_owned(),
        };
        HostFormOutcome::Submit {
            label,
            target,
            session,
        }
    }

    pub(crate) fn render(
        &self,
        theme: &Theme,
        focus: &FocusHandle,
        cx: &mut Context<GuiApp>,
    ) -> impl IntoElement {
        let mut fields = div().flex().flex_col().gap(px(10.0));
        for (index, (label, placeholder)) in FIELDS.iter().enumerate() {
            let value = &self.values[index];
            let focused = index == self.focused;
            let text: SharedString = if value.is_empty() {
                (*placeholder).into()
            } else {
                value.clone().into()
            };
            fields = fields.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(div().text_xs().text_color(theme.text_muted).child(*label))
                    .child(
                        div()
                            .id(("host-field", index))
                            .flex()
                            .px(px(10.0))
                            .py(px(6.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(if focused { theme.accent } else { theme.border })
                            .bg(theme.sidebar_bg)
                            .cursor_text()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.focus_form_field(index);
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .when(value.is_empty(), |text| {
                                        text.text_color(theme.text_muted)
                                    })
                                    .child(text),
                            )
                            .when(focused, |field| {
                                field.child(div().w(px(1.0)).h(px(16.0)).bg(theme.accent))
                            }),
                    ),
            );
        }

        div()
            .id("host-form-backdrop")
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
                    .on_key_down(cx.listener(GuiApp::on_form_key_event))
                    .w(px(420.0))
                    .p(px(20.0))
                    .flex()
                    .flex_col()
                    .gap(px(14.0))
                    .rounded(px(10.0))
                    .bg(theme.panel_bg)
                    .border_1()
                    .border_color(theme.border)
                    .child(div().text_lg().child("Add SSH machine"))
                    .child(div().text_xs().text_color(theme.text_muted).child(
                        "Herdr must be installed on the machine. Uses your ssh config and keys.",
                    ))
                    .child(fields)
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
                                    .id("host-cancel")
                                    .px(px(12.0))
                                    .py(px(6.0))
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .hover(|style| style.bg(theme.hover))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.cancel_form(window);
                                        cx.notify();
                                    }))
                                    .child("Cancel"),
                            )
                            .child(
                                div()
                                    .id("host-add")
                                    .px(px(12.0))
                                    .py(px(6.0))
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .bg(theme.accent)
                                    .text_color(theme.panel_bg)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_form(window);
                                        cx.notify();
                                    }))
                                    .child("Add machine"),
                            ),
                    ),
            )
    }

    pub(crate) fn focus_field(&mut self, index: usize) {
        self.focused = index.min(FIELDS.len() - 1);
    }

    pub(crate) fn force_submit(&mut self) -> HostFormOutcome {
        self.submit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(form: &mut HostForm, key: &str, key_char: Option<&str>) -> HostFormOutcome {
        form.key(&Keystroke {
            modifiers: gpui::Modifiers::default(),
            key: key.into(),
            key_char: key_char.map(str::to_owned),
        })
    }

    #[test]
    fn submit_requires_a_target_and_defaults_label_and_session() {
        let mut form = HostForm::default();
        assert!(matches!(
            press(&mut form, "enter", None),
            HostFormOutcome::Pending
        ));
        assert!(form.error.is_some());
        for ch in "me@box".chars() {
            let text = ch.to_string();
            press(&mut form, &text, Some(&text));
        }
        match press(&mut form, "enter", None) {
            HostFormOutcome::Submit {
                label,
                target,
                session,
            } => {
                assert_eq!(label, "me@box");
                assert_eq!(target, "me@box");
                assert_eq!(session, crate::session::DEFAULT_SESSION_NAME);
            }
            _ => panic!("expected submit"),
        }
    }

    #[test]
    fn tab_cycles_fields_and_backspace_edits_the_focused_one() {
        let mut form = HostForm::default();
        press(&mut form, "a", Some("a"));
        press(&mut form, "tab", None);
        press(&mut form, "b", Some("b"));
        press(&mut form, "backspace", None);
        assert_eq!(form.values, ["a".to_owned(), String::new(), String::new()]);
        assert!(matches!(
            press(&mut form, "escape", None),
            HostFormOutcome::Cancel
        ));
    }
}
