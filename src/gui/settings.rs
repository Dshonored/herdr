//! Settings page: machines (shared with the TUI's catalog), usage, and appearance.

use gpui::{div, prelude::*, px, Context, FocusHandle, SharedString};

use super::app::GuiApp;
use super::claude_limits::LimitsHook;
use super::prefs::{GuiPrefs, MAX_FONT_SIZE, MIN_FONT_SIZE};
use super::theme::Theme;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SettingsSection {
    #[default]
    Machines,
    Usage,
    Appearance,
    Updates,
}

impl SettingsSection {
    const ALL: [Self; 4] = [Self::Machines, Self::Usage, Self::Appearance, Self::Updates];

    fn label(self) -> &'static str {
        match self {
            Self::Machines => "Machines",
            Self::Usage => "Usage",
            Self::Appearance => "Appearance",
            Self::Updates => "Updates",
        }
    }
}

/// The main update action available right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpdateAction {
    Download,
    Restart,
}

pub(crate) struct UpdateView {
    pub(crate) current: String,
    pub(crate) status: String,
    pub(crate) action: Option<UpdateAction>,
    pub(crate) busy: bool,
}

/// One saved machine as the settings page shows it.
pub(crate) struct MachineRow {
    pub(crate) profile_id: String,
    pub(crate) label: String,
    pub(crate) target: String,
    pub(crate) session: String,
    pub(crate) enabled: bool,
    /// Connection status while the machine is enabled.
    pub(crate) status: Option<String>,
}

/// Whether Claude Code plan limits are captured on one connected machine.
pub(crate) struct LimitsRow {
    pub(crate) host: usize,
    pub(crate) label: String,
    pub(crate) state: Option<LimitsHook>,
}

pub(crate) struct SettingsView<'a> {
    pub(crate) section: SettingsSection,
    pub(crate) machines: Vec<MachineRow>,
    pub(crate) limits: Vec<LimitsRow>,
    pub(crate) prefs: &'a GuiPrefs,
    pub(crate) update: UpdateView,
    /// This computer's server `[ui] max_fps`, when known.
    pub(crate) max_fps: Option<u16>,
}

fn button(id: impl Into<gpui::ElementId>, label: &str, theme: &Theme) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .whitespace_nowrap()
        .px(px(10.0))
        .py(px(4.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(theme.border)
        .text_sm()
        .cursor_pointer()
        .hover(|style| style.bg(theme.hover))
        .child(label.to_owned())
}

fn toggle(
    id: &'static str,
    label: &str,
    detail: &str,
    on: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(12.0))
        .py(px(6.0))
        .cursor_pointer()
        .child(
            div()
                .flex_none()
                .w(px(34.0))
                .h(px(18.0))
                .rounded_full()
                .p(px(2.0))
                .flex()
                .when(on, |track| track.justify_end().bg(theme.accent))
                .when(!on, |track| track.bg(theme.border))
                .child(div().size(px(14.0)).rounded_full().bg(theme.text)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(div().text_sm().child(label.to_owned()))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text_muted)
                        .child(detail.to_owned()),
                ),
        )
}

fn machines(view: &SettingsView<'_>, theme: &Theme, cx: &mut Context<GuiApp>) -> gpui::Div {
    let mut list = div().flex().flex_col().gap(px(8.0)).child(
        div()
            .text_sm()
            .text_color(theme.text_muted)
            .child("Saved SSH machines are shared with the herdr terminal app. Herdr is installed or updated on a machine automatically when it connects."),
    );
    list = list.child(
        div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .p(px(10.0))
            .rounded(px(8.0))
            .bg(theme.sidebar_bg)
            .child(div().flex_1().text_sm().child("Local"))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.text_muted)
                    .child("this computer"),
            ),
    );
    for (index, machine) in view.machines.iter().enumerate() {
        let toggle_id = machine.profile_id.clone();
        let rename_id = machine.profile_id.clone();
        let remove_id = machine.profile_id.clone();
        let enabled = machine.enabled;
        let detail: SharedString = format!(
            "{} · session {}{}",
            machine.target,
            machine.session,
            machine
                .status
                .as_ref()
                .map(|status| format!(" · {status}"))
                .unwrap_or_else(|| if enabled {
                    String::new()
                } else {
                    " · disabled".into()
                })
        )
        .into();
        list = list.child(
            div()
                .flex()
                .items_center()
                .gap(px(10.0))
                .p(px(10.0))
                .rounded(px(8.0))
                .bg(theme.sidebar_bg)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().child(machine.label.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.text_muted)
                                .truncate()
                                .child(detail),
                        ),
                )
                .child(
                    button(
                        ("machine-toggle", index),
                        if enabled { "Disable" } else { "Enable" },
                        theme,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_machine_enabled(&toggle_id, !enabled);
                        cx.notify();
                    })),
                )
                .when(enabled, |row| {
                    row.child(button(("machine-rename", index), "Rename", theme).on_click(
                        cx.listener(move |this, _, window, cx| {
                            this.rename_machine_dialog(&rename_id, window);
                            cx.notify();
                        }),
                    ))
                })
                .child(
                    button(("machine-remove", index), "Remove", theme)
                        .text_color(theme.danger)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.remove_machine_dialog(&remove_id, window);
                            cx.notify();
                        })),
                ),
        );
    }
    list.child(
        div().child(
            button("machine-add", "+ Add SSH machine", theme)
                .text_color(theme.accent)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_form(window);
                    cx.notify();
                })),
        ),
    )
}

fn usage(view: &SettingsView<'_>, theme: &Theme, cx: &mut Context<GuiApp>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .text_sm()
                .text_color(theme.text_muted)
                .child("Usage is read from the agents' own logs from the last 7 days: Claude Code (tokens), Codex (tokens and 5-hour / weekly limits) and pi (tokens and cost). Nothing is sent anywhere."),
        )
        .child(limits_section(view, theme, cx))
        .child(
            toggle(
                "usage-enabled",
                "Show usage in the sidebar",
                "For the selected machine, refreshed every minute.",
                view.prefs.usage_enabled,
                theme,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.update_prefs(|prefs| prefs.usage_enabled = !prefs.usage_enabled);
                cx.notify();
            })),
        )
        .child(
            toggle(
                "usage-remote",
                "Read usage on SSH machines",
                "Runs a read-only find/grep over SSH that returns token counts and limit percentages only.",
                view.prefs.remote_usage,
                theme,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.update_prefs(|prefs| prefs.remote_usage = !prefs.remote_usage);
                cx.notify();
            })),
        )
}

fn limits_section(view: &SettingsView<'_>, theme: &Theme, cx: &mut Context<GuiApp>) -> gpui::Div {
    let mut section = div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .mt(px(6.0))
        .mb(px(10.0))
        .p(px(12.0))
        .rounded(px(8.0))
        .bg(theme.sidebar_bg)
        .child(div().text_sm().child("Claude Code 5-hour and weekly limits (%)"))
        .child(
            div()
                .text_xs()
                .text_color(theme.text_muted)
                .child("Claude Code reports these to its status line (Pro and Max plans). Enabling wraps your statusLine setting so each update is also saved to ~/.claude/herdr-usage.json; your own status line keeps working unchanged. Disabling restores your original setting. Values appear after Claude Code's next response."),
        );
    for row in &view.limits {
        let host = row.host;
        let (status, action): (&str, Option<bool>) = match &row.state {
            None | Some(LimitsHook::Checking) => ("checking…", None),
            Some(LimitsHook::Enabled) => ("on", Some(false)),
            Some(LimitsHook::Disabled) => ("off", Some(true)),
            Some(LimitsHook::Failed(_)) => ("error", Some(true)),
        };
        let error = match &row.state {
            Some(LimitsHook::Failed(error)) => Some(error.clone()),
            _ => None,
        };
        section = section.child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .child(row.label.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_xs()
                                .text_color(theme.text_muted)
                                .child(status),
                        )
                        .when_some(action, |line, enable| {
                            line.child(
                                button(
                                    ("limits-toggle", host),
                                    if enable { "Enable" } else { "Disable" },
                                    theme,
                                )
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.set_claude_limits(host, enable, cx);
                                        cx.notify();
                                    },
                                )),
                            )
                        }),
                )
                .when_some(error, |line, error| {
                    line.child(div().text_xs().text_color(theme.danger).child(error))
                }),
        );
    }
    section
}

fn updates(view: &SettingsView<'_>, theme: &Theme, cx: &mut Context<GuiApp>) -> gpui::Div {
    let update = &view.update;
    let mut actions = div().flex().gap(px(8.0)).child(
        button(
            "update-check",
            if update.busy {
                "Checking…"
            } else {
                "Check for updates"
            },
            theme,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.check_for_updates(true, cx);
            cx.notify();
        })),
    );
    match update.action {
        Some(UpdateAction::Download) => {
            actions = actions.child(
                button("update-download", "Download update", theme)
                    .text_color(theme.accent)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.download_update(cx);
                        cx.notify();
                    })),
            );
        }
        Some(UpdateAction::Restart) => {
            actions = actions.child(
                button("update-restart", "Restart to update", theme)
                    .text_color(theme.accent)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.restart_to_update(cx);
                        cx.notify();
                    })),
            );
        }
        None => {}
    }
    actions = actions.child(
        button("update-notes", "Release notes", theme).on_click(cx.listener(|this, _, _, cx| {
            this.open_release_notes(cx);
        })),
    );
    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .text_sm()
                        .child(format!("herdr desktop {}", update.current)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text_muted)
                        .child(update.status.clone()),
                ),
        )
        .child(actions)
        .child(
            toggle(
                "update-auto",
                "Download updates automatically",
                "New releases download in the background; you choose when to restart.",
                view.prefs.auto_download_updates,
                theme,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.update_prefs(|prefs| {
                    prefs.auto_download_updates = !prefs.auto_download_updates;
                });
                cx.notify();
            })),
        )
}

fn appearance(view: &SettingsView<'_>, theme: &Theme, cx: &mut Context<GuiApp>) -> gpui::Div {
    let size = view.prefs.font_size;
    let mut rates = div()
        .flex()
        .items_center()
        .gap(px(10.0))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w_0()
                .child(div().text_sm().child("Terminal frame rate"))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.text_muted)
                        .child("How often this computer's herdr server sends screen updates. 120 is smoother on high refresh rate displays and uses more CPU. SSH machines keep their own setting."),
                ),
        );
    for fps in [60u16, 120] {
        let active = view.max_fps == Some(fps);
        rates = rates.child(
            button(("fps", usize::from(fps)), &format!("{fps} fps"), theme)
                .when(active, |button| {
                    button.bg(theme.selected).border_color(theme.accent)
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_frame_rate(fps, cx);
                    cx.notify();
                })),
        );
    }
    div().flex().flex_col().gap(px(14.0)).child(rates).child(
        div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(div().flex_1().text_sm().child("Terminal font size"))
            .child(
                button("font-smaller", "−", theme).on_click(cx.listener(|this, _, _, cx| {
                    this.update_prefs(|prefs| {
                        prefs.font_size = (prefs.font_size - 1.0).max(MIN_FONT_SIZE);
                    });
                    cx.notify();
                })),
            )
            .child(div().w(px(36.0)).text_center().child(format!("{size:.0}")))
            .child(
                button("font-larger", "+", theme).on_click(cx.listener(|this, _, _, cx| {
                    this.update_prefs(|prefs| {
                        prefs.font_size = (prefs.font_size + 1.0).min(MAX_FONT_SIZE);
                    });
                    cx.notify();
                })),
            ),
    )
}

pub(crate) fn render(
    view: SettingsView<'_>,
    focus: &FocusHandle,
    theme: &Theme,
    cx: &mut Context<GuiApp>,
) -> impl IntoElement {
    let mut nav = div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .w(px(150.0))
        .p(px(10.0));
    for section in SettingsSection::ALL {
        let active = section == view.section;
        nav = nav.child(
            div()
                .id(SharedString::from(format!("settings-{}", section.label())))
                .px(px(10.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .text_sm()
                .cursor_pointer()
                .hover(|style| style.bg(theme.hover))
                .when(active, |item| item.bg(theme.selected))
                .when(!active, |item| item.text_color(theme.text_muted))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_settings_section(section);
                    cx.notify();
                }))
                .child(section.label()),
        );
    }
    let content = match view.section {
        SettingsSection::Machines => machines(&view, theme, cx),
        SettingsSection::Usage => usage(&view, theme, cx),
        SettingsSection::Appearance => appearance(&view, theme, cx),
        SettingsSection::Updates => updates(&view, theme, cx),
    };

    div()
        .id("settings")
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
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.key == "escape" {
                        this.close_settings(window);
                        cx.notify();
                    }
                }))
                .w(px(760.0))
                .h(px(520.0))
                .max_w(gpui::relative(0.94))
                .max_h(gpui::relative(0.9))
                .flex()
                .flex_col()
                .overflow_hidden()
                .rounded(px(10.0))
                .bg(theme.panel_bg)
                .border_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .px(px(16.0))
                        .py(px(12.0))
                        .border_b_1()
                        .border_color(theme.border)
                        .child(div().flex_1().text_lg().child("Settings"))
                        .child(
                            div()
                                .id("settings-close")
                                .px(px(8.0))
                                .rounded(px(4.0))
                                .cursor_pointer()
                                .text_color(theme.text_muted)
                                .hover(|style| style.bg(theme.hover))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.close_settings(window);
                                    cx.notify();
                                }))
                                .child("×"),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_h_0()
                        .child(nav.flex_none().border_r_1().border_color(theme.border))
                        .child(
                            div()
                                .id("settings-content")
                                .flex_1()
                                .min_w_0()
                                .p(px(16.0))
                                .overflow_y_scroll()
                                .child(content),
                        ),
                ),
        )
}
