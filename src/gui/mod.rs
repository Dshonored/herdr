//! Native desktop client (`herdr gui`).
//!
//! A GPUI window that is one more client of the Herdr server, like the TUI. It
//! connects to Local and saved SSH machines through the stable endpoint
//! protocol, so agents, workspaces, tabs, and panes behave exactly as they do in
//! the terminal client.

mod app;
mod claude_limits;
mod config_file;
mod connection;
mod dialog;
mod host;
mod host_form;
mod menu;
mod prefs;
mod settings;
mod terminal;
mod theme;
mod updater;
mod usage;
mod usage_view;

use std::io;

use gpui::{px, size, App, AppContext as _, Application, Bounds, WindowBounds, WindowOptions};

pub(crate) fn run() -> io::Result<()> {
    crate::logging::init_file_logging("herdr-gui.log");
    crate::logging::startup("gui");

    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1280.0), px(800.0)), cx);
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(720.0), px(460.0))),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("herdr".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| app::GuiApp::new(window, cx)),
        );
        if let Err(error) = opened {
            eprintln!("herdr: could not open a window: {error}");
            cx.quit();
            return;
        }
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
    Ok(())
}
