//! Right-click context menus. Each target offers the same actions as the TUI's
//! context menus, expressed as endpoint API requests or local commands.

use gpui::{div, prelude::*, px, Context, Pixels, Point, SharedString, Size};
use serde_json::{json, Value};

use crate::protocol::{ClientShellAgent, ClientShellPane, ClientShellTab, ClientShellWorkspace};

use super::app::GuiApp;
use super::connection::HostTarget;
use super::dialog::{Dialog, DialogSubmit};
use super::host::{HostState, HostStatus};
use super::theme::Theme;

const MENU_WIDTH: f32 = 230.0;
const ITEM_HEIGHT: f32 = 28.0;
const SEPARATOR_HEIGHT: f32 = 9.0;

/// What a menu item or confirmed dialog does.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Command {
    /// Endpoint requests sent in order to the menu's machine.
    Requests(Vec<(&'static str, Value)>),
    Dialog(Box<Dialog>),
    NewWorkspace,
    Reconnect,
    UpdateAndRestart,
    RemoveMachine,
    /// Remove a saved machine by catalog profile id (it may be disabled).
    RemoveProfile(String),
    /// Record the scanned host key of the menu's machine and connect.
    TrustHostKey,
    Paste,
}

impl Command {
    fn request(method: &'static str, params: Value) -> Self {
        Self::Requests(vec![(method, params)])
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum MenuItem {
    Action {
        label: SharedString,
        command: Command,
        danger: bool,
    },
    Separator,
}

fn action(label: &str, command: Command) -> MenuItem {
    MenuItem::Action {
        label: label.to_owned().into(),
        command,
        danger: false,
    }
}

fn danger(label: &str, command: Command) -> MenuItem {
    MenuItem::Action {
        label: label.to_owned().into(),
        command,
        danger: true,
    }
}

pub(crate) struct ContextMenu {
    /// Index of the machine the actions apply to.
    pub(crate) host: usize,
    pub(crate) position: Point<Pixels>,
    pub(crate) items: Vec<MenuItem>,
}

pub(crate) fn machine_menu(host: &HostState) -> Vec<MenuItem> {
    let mut items = Vec::new();
    if host.is_online() {
        items.push(action("New workspace", Command::NewWorkspace));
    }
    items.push(action("Reconnect", Command::Reconnect));
    if matches!(host.status, HostStatus::NeedsRestart(_)) {
        items.push(action(
            "Update & restart herdr…",
            Command::Dialog(Box::new(Dialog::confirm(
                format!("Update herdr on {}?", host.label),
                "Restarting the herdr server on this machine stops its running shells and agents. \
                 The session layout is restored afterwards.",
                "Update & restart",
                true,
                Command::UpdateAndRestart,
            ))),
        ));
    }
    if matches!(host.target, HostTarget::Ssh { .. }) {
        items.push(MenuItem::Separator);
        items.push(action(
            "Rename machine…",
            Command::Dialog(Box::new(Dialog::input(
                "Rename machine",
                "Name",
                &host.label,
                "Rename",
                DialogSubmit::RenameMachine,
            ))),
        ));
        items.push(danger(
            "Remove machine…",
            Command::Dialog(Box::new(Dialog::confirm(
                format!("Remove {}?", host.label),
                "This only removes it from this app. Its herdr session keeps running on the machine.",
                "Remove",
                true,
                Command::RemoveMachine,
            ))),
        ));
    }
    items
}

pub(crate) fn workspace_menu(workspace: &ClientShellWorkspace) -> Vec<MenuItem> {
    let id = workspace.workspace_id.clone();
    let mut items = vec![
        action(
            "New tab",
            Command::request("tab.create", json!({ "workspace_id": id, "focus": true })),
        ),
        action(
            "Rename…",
            Command::Dialog(Box::new(Dialog::input(
                "Rename workspace",
                "Name",
                &workspace.label,
                "Rename",
                DialogSubmit::request(
                    "workspace.rename",
                    json!({ "workspace_id": id }),
                    "label",
                    true,
                ),
            ))),
        ),
    ];
    if workspace.branch.is_some() || workspace.worktree.is_some() {
        items.push(MenuItem::Separator);
        items.push(action(
            "New worktree…",
            Command::Dialog(Box::new(Dialog::input(
                "New worktree",
                "Branch name",
                "",
                "Create",
                DialogSubmit::request(
                    "worktree.create",
                    json!({ "workspace_id": id, "focus": true }),
                    "branch",
                    true,
                ),
            ))),
        ));
        items.push(action(
            "Open worktree…",
            Command::Dialog(Box::new(Dialog::input(
                "Open worktree",
                "Existing branch",
                "",
                "Open",
                DialogSubmit::request(
                    "worktree.open",
                    json!({ "workspace_id": id, "focus": true }),
                    "branch",
                    true,
                ),
            ))),
        ));
        if workspace
            .worktree
            .as_ref()
            .is_some_and(|worktree| worktree.is_linked_worktree)
        {
            items.push(danger(
                "Delete worktree checkout…",
                Command::Dialog(Box::new(Dialog::confirm(
                    format!("Delete the {} worktree checkout?", workspace.label),
                    "Closes this workspace and deletes its linked worktree folder. \
                     Uncommitted changes block the deletion.",
                    "Delete checkout",
                    true,
                    Command::request(
                        "worktree.remove",
                        json!({ "workspace_id": id, "force": false }),
                    ),
                ))),
            ));
        }
    }
    items.push(MenuItem::Separator);
    items.push(danger(
        "Close workspace…",
        Command::Dialog(Box::new(Dialog::confirm(
            format!("Close {}?", workspace.label),
            "Closes every tab and pane in this workspace and ends their processes.",
            "Close workspace",
            true,
            Command::request("workspace.close", json!({ "workspace_id": id })),
        ))),
    ));
    items
}

pub(crate) fn tab_menu(tab: &ClientShellTab) -> Vec<MenuItem> {
    let id = tab.tab_id.clone();
    vec![
        action(
            "New tab",
            Command::request(
                "tab.create",
                json!({ "workspace_id": tab.workspace_id, "focus": true }),
            ),
        ),
        action(
            "Rename…",
            Command::Dialog(Box::new(Dialog::input(
                "Rename tab",
                "Name",
                &tab.label,
                "Rename",
                DialogSubmit::request("tab.rename", json!({ "tab_id": id }), "label", true),
            ))),
        ),
        MenuItem::Separator,
        danger(
            "Close tab",
            Command::request("tab.close", json!({ "tab_id": id })),
        ),
    ]
}

pub(crate) fn pane_menu(pane: &ClientShellPane, focused_pane_id: Option<&str>) -> Vec<MenuItem> {
    let id = pane.pane_id.clone();
    let mut items = vec![
        action(
            "Split right",
            Command::request(
                "pane.split",
                json!({ "target_pane_id": id, "direction": "right", "focus": true }),
            ),
        ),
        action(
            "Split down",
            Command::request(
                "pane.split",
                json!({ "target_pane_id": id, "direction": "down", "focus": true }),
            ),
        ),
        action(
            "Zoom",
            Command::request("pane.zoom", json!({ "pane_id": id, "mode": "toggle" })),
        ),
        MenuItem::Separator,
        action("Paste", Command::Paste),
        action(
            "Rename pane…",
            Command::Dialog(Box::new(Dialog::input(
                "Rename pane",
                "Name",
                pane.label.as_deref().unwrap_or_default(),
                "Rename",
                DialogSubmit::request("pane.rename", json!({ "pane_id": id }), "label", false),
            ))),
        ),
    ];
    if pane.label.is_some() {
        items.push(action(
            "Clear pane name",
            Command::request("pane.rename", json!({ "pane_id": id })),
        ));
    }
    if let Some(focused) = focused_pane_id.filter(|focused| *focused != id) {
        items.push(action(
            "Swap with focused pane",
            Command::Requests(vec![
                (
                    "pane.swap",
                    json!({ "source_pane_id": focused, "target_pane_id": id }),
                ),
                ("pane.focus", json!({ "pane_id": focused })),
            ]),
        ));
    }
    let (label, target) = if pane.right_click_passthrough {
        ("Use herdr right-click menu", "herdr")
    } else {
        ("Send right-clicks to pane", "pane")
    };
    items.push(action(
        label,
        Command::request(
            "pane.input.set",
            json!({ "pane_id": id, "right_click": target }),
        ),
    ));
    items.push(MenuItem::Separator);
    items.push(danger(
        "Close pane",
        Command::request("pane.close", json!({ "pane_id": id })),
    ));
    items
}

pub(crate) fn agent_menu(
    agent: &ClientShellAgent,
    pane: Option<&ClientShellPane>,
) -> Vec<MenuItem> {
    let id = agent.pane_id.clone();
    let mut items = vec![action(
        "Go to agent",
        Command::request("pane.focus", json!({ "pane_id": id })),
    )];
    if let Some(pane) = pane {
        items.push(action(
            "Rename…",
            Command::Dialog(Box::new(Dialog::input(
                "Rename agent pane",
                "Name",
                pane.label.as_deref().unwrap_or_default(),
                "Rename",
                DialogSubmit::request("pane.rename", json!({ "pane_id": id }), "label", false),
            ))),
        ));
    }
    items.push(MenuItem::Separator);
    items.push(danger(
        "Close pane",
        Command::request("pane.close", json!({ "pane_id": id })),
    ));
    items
}

fn menu_height(items: &[MenuItem]) -> f32 {
    items
        .iter()
        .map(|item| match item {
            MenuItem::Action { .. } => ITEM_HEIGHT,
            MenuItem::Separator => SEPARATOR_HEIGHT,
        })
        .sum::<f32>()
        + 8.0
}

/// Opens at the pointer, shifted to stay inside the window.
pub(crate) fn placed_origin(
    position: Point<Pixels>,
    items: &[MenuItem],
    viewport: Size<Pixels>,
) -> Point<Pixels> {
    let width = px(MENU_WIDTH);
    let height = px(menu_height(items));
    let x = if position.x + width > viewport.width {
        (viewport.width - width).max(px(0.0))
    } else {
        position.x
    };
    let y = if position.y + height > viewport.height {
        (viewport.height - height).max(px(0.0))
    } else {
        position.y
    };
    gpui::point(x, y)
}

pub(crate) fn render(
    menu: &ContextMenu,
    viewport: Size<Pixels>,
    theme: &Theme,
    cx: &mut Context<GuiApp>,
) -> impl IntoElement {
    let origin = placed_origin(menu.position, &menu.items, viewport);
    let mut panel = div()
        .absolute()
        .left(origin.x)
        .top(origin.y)
        .w(px(MENU_WIDTH))
        .py(px(4.0))
        .flex()
        .flex_col()
        .rounded(px(8.0))
        .bg(theme.panel_bg)
        .border_1()
        .border_color(theme.border)
        .shadow_lg()
        .text_sm()
        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
            this.close_menu();
            cx.notify();
        }));
    for (index, item) in menu.items.iter().enumerate() {
        panel = match item {
            MenuItem::Separator => panel.child(
                div()
                    .h(px(SEPARATOR_HEIGHT))
                    .flex()
                    .items_center()
                    .child(div().h(px(1.0)).w_full().bg(theme.border)),
            ),
            MenuItem::Action { label, danger, .. } => panel.child(
                div()
                    .id(("menu-item", index))
                    .h(px(ITEM_HEIGHT))
                    .mx(px(4.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .when(*danger, |item| item.text_color(theme.danger))
                    .hover(|style| style.bg(theme.selected))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.activate_menu_item(index, window, cx);
                        cx.notify();
                    }))
                    .child(label.clone()),
            ),
        };
    }
    div()
        .id("context-menu-layer")
        .absolute()
        .inset_0()
        .occlude()
        .child(panel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::AgentStatus;

    fn labels(items: &[MenuItem]) -> Vec<String> {
        items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action { label, .. } => Some(label.to_string()),
                MenuItem::Separator => None,
            })
            .collect()
    }

    fn pane(label: Option<&str>, passthrough: bool) -> ClientShellPane {
        ClientShellPane {
            pane_id: "p2".into(),
            workspace_id: "w1".into(),
            tab_id: "t1".into(),
            label: label.map(str::to_owned),
            cwd: None,
            foreground_cwd: None,
            focused: false,
            right_click_passthrough: passthrough,
        }
    }

    fn workspace(linked: bool) -> ClientShellWorkspace {
        ClientShellWorkspace {
            workspace_id: "w1".into(),
            active_tab_id: "t1".into(),
            new_workspace_cwd: String::new(),
            number: 1,
            label: "api".into(),
            custom_label: false,
            branch: Some("main".into()),
            git_ahead_behind: None,
            tokens: Vec::new(),
            worktree: Some(crate::protocol::ClientShellWorktree {
                key: "k".into(),
                label: "api".into(),
                is_linked_worktree: linked,
            }),
            focused: true,
            agent_status: AgentStatus::Idle,
        }
    }

    #[test]
    fn pane_menu_matches_tui_actions() {
        assert_eq!(
            labels(&pane_menu(&pane(Some("build"), false), Some("p1"))),
            [
                "Split right",
                "Split down",
                "Zoom",
                "Paste",
                "Rename pane…",
                "Clear pane name",
                "Swap with focused pane",
                "Send right-clicks to pane",
                "Close pane",
            ]
        );
        let focused = labels(&pane_menu(&pane(None, true), Some("p2")));
        assert!(!focused.contains(&"Swap with focused pane".to_owned()));
        assert!(!focused.contains(&"Clear pane name".to_owned()));
        assert!(focused.contains(&"Use herdr right-click menu".to_owned()));
    }

    #[test]
    fn swap_targets_the_clicked_pane_and_keeps_focus() {
        let items = pane_menu(&pane(None, false), Some("p1"));
        let swap = items.iter().find_map(|item| match item {
            MenuItem::Action { label, command, .. } if label == "Swap with focused pane" => {
                Some(command.clone())
            }
            _ => None,
        });
        assert_eq!(
            swap,
            Some(Command::Requests(vec![
                (
                    "pane.swap",
                    json!({ "source_pane_id": "p1", "target_pane_id": "p2" })
                ),
                ("pane.focus", json!({ "pane_id": "p1" })),
            ]))
        );
    }

    #[test]
    fn worktree_actions_follow_workspace_kind() {
        let linked = labels(&workspace_menu(&workspace(true)));
        assert!(linked.contains(&"New worktree…".to_owned()));
        assert!(linked.contains(&"Delete worktree checkout…".to_owned()));
        let primary = labels(&workspace_menu(&workspace(false)));
        assert!(primary.contains(&"Open worktree…".to_owned()));
        assert!(!primary.contains(&"Delete worktree checkout…".to_owned()));
    }

    #[test]
    fn menus_stay_inside_the_window() {
        let items = vec![action("A", Command::Paste); 4];
        let viewport = gpui::size(px(400.0), px(300.0));
        let origin = placed_origin(gpui::point(px(390.0), px(290.0)), &items, viewport);
        assert_eq!(origin.x, px(400.0 - MENU_WIDTH));
        assert_eq!(origin.y, px(300.0 - menu_height(&items)));
        let origin = placed_origin(gpui::point(px(10.0), px(20.0)), &items, viewport);
        assert_eq!(origin, gpui::point(px(10.0), px(20.0)));
    }
}
