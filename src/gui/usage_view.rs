//! Sidebar usage section for the selected machine.

use std::collections::HashSet;

use gpui::{div, prelude::*, px, Context, Hsla, SharedString};

use super::app::GuiApp;
use super::theme::Theme;
use super::usage::{format_tokens, format_until, now, AgentKind, AgentUsage, UsageReport};

#[derive(Default)]
pub(crate) struct UsageState {
    pub(crate) report: Option<UsageReport>,
    pub(crate) error: Option<String>,
    pub(crate) loading: bool,
}

fn limit_color(theme: &Theme, used: f64) -> Hsla {
    use crate::api::schema::AgentStatus;
    if used >= 90.0 {
        theme.danger
    } else if used >= 70.0 {
        theme.status_color(AgentStatus::Working)
    } else {
        theme.status_color(AgentStatus::Idle)
    }
}

fn bar(theme: &Theme, used_percent: f64) -> impl IntoElement {
    let fraction = (used_percent / 100.0).clamp(0.0, 1.0) as f32;
    div()
        .h(px(4.0))
        .w_full()
        .rounded_full()
        .bg(theme.border)
        .child(
            div()
                .h_full()
                .w(gpui::relative(fraction))
                .rounded_full()
                .bg(limit_color(theme, used_percent)),
        )
}

fn agent_card(
    agent: AgentKind,
    usage: &AgentUsage,
    expanded: bool,
    theme: &Theme,
    cx: &mut Context<GuiApp>,
) -> impl IntoElement {
    let current = now();
    let plan = usage
        .limits
        .as_ref()
        .and_then(|limits| limits.plan.clone())
        .map(|plan| format!(" · {plan}"))
        .unwrap_or_default();
    // With plan limits, the headline is how much of the 5-hour window is used.
    let headline = usage
        .limits
        .as_ref()
        .and_then(|limits| limits.windows.iter().find(|w| w.window_minutes == 300))
        .map(|window| format!("5h {:.0}%", window.used_percent))
        .unwrap_or_else(|| format!("5h {}", format_tokens(usage.last_5h.tokens())));
    let mut card = div()
        .id(SharedString::from(format!("usage-{agent:?}")))
        .flex()
        .flex_col()
        .gap(px(4.0))
        .px(px(8.0))
        .py(px(6.0))
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(|style| style.bg(theme.hover))
        .on_click(cx.listener(move |this, _, _, cx| {
            this.toggle_usage_details(agent);
            cx.notify();
        }))
        .child(
            div()
                .flex()
                .justify_between()
                .text_sm()
                .gap(px(8.0))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(format!("{}{plan}", agent.label())),
                )
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(theme.text_muted)
                        .child(headline),
                ),
        );

    if let Some(limits) = &usage.limits {
        let age = current - limits.at;
        if age > 15 * 60 {
            card = card.child(div().text_xs().text_color(theme.text_muted).child(format!(
                "limits as of {} ago",
                format_until(current, current - age)
            )));
        }
        for window in &limits.windows {
            let resets = window
                .resets_at
                .filter(|at| *at > current)
                .map(|at| format!(" · resets {}", format_until(at, current)))
                .unwrap_or_default();
            card = card.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(div().text_xs().text_color(theme.text_muted).child(format!(
                        "{} {:.0}% used{resets}",
                        window.label(),
                        window.used_percent
                    )))
                    .child(bar(theme, window.used_percent)),
            );
        }
    }

    if agent == AgentKind::ClaudeCode && usage.limits.is_none() {
        card = card.child(
            div()
                .id("claude-limits-hint")
                .text_xs()
                .text_color(theme.accent)
                .cursor_pointer()
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_limits_settings(window, cx);
                    cx.stop_propagation();
                    cx.notify();
                }))
                .child("Show 5-hour / weekly % \u{2192} turn on in Settings"),
        );
    }
    let cost = usage
        .last_7d
        .cost_usd
        .map(|cost| format!(" · ${cost:.2}"))
        .unwrap_or_default();
    card = card.child(div().text_xs().text_color(theme.text_muted).child(format!(
        "24h {} · 7d {}{cost}",
        format_tokens(usage.last_24h.tokens()),
        format_tokens(usage.last_7d.tokens()),
    )));

    if expanded {
        let week = &usage.last_7d;
        card = card.child(div().text_xs().text_color(theme.text_muted).child(format!(
            "7d: in {} · out {} · cache read {} · cache write {} · {} requests",
            format_tokens(week.input),
            format_tokens(week.output),
            format_tokens(week.cache_read),
            format_tokens(week.cache_write),
            week.requests
        )));
        for (model, totals) in &usage.models {
            card = card.child(
                div()
                    .flex()
                    .justify_between()
                    .gap(px(8.0))
                    .text_xs()
                    .child(div().min_w_0().truncate().child(model.clone()))
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.text_muted)
                            .child(format_tokens(totals.tokens())),
                    ),
            );
        }
    }
    card
}

pub(crate) fn render(
    state: Option<&UsageState>,
    expanded: &HashSet<AgentKind>,
    theme: &Theme,
    cx: &mut Context<GuiApp>,
) -> impl IntoElement {
    let mut section = div().flex().flex_col().gap(px(2.0));
    let report = state.and_then(|state| state.report.as_ref());
    match (state, report) {
        (Some(state), None) if state.loading => {
            section = section.child(muted("Reading agent logs…", theme));
        }
        (_, Some(report)) if report.agents.is_empty() => {
            section = section.child(muted("No agent usage in the last 7 days", theme));
        }
        (_, Some(report)) => {
            for (agent, usage) in &report.agents {
                section = section.child(agent_card(
                    *agent,
                    usage,
                    expanded.contains(agent),
                    theme,
                    cx,
                ));
            }
        }
        _ => {}
    }
    if let Some(error) = state.and_then(|state| state.error.clone()) {
        section = section.child(muted(&error, theme));
    }
    section
}

fn muted(text: &str, theme: &Theme) -> impl IntoElement {
    div()
        .px(px(8.0))
        .text_xs()
        .text_color(theme.text_muted)
        .child(text.to_owned())
}
