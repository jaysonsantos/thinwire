//! The native menu bar of macOS (`muda`).
//!
//! The standard items (About, Services, Hide, Minimize, ...) are native
//! AppKit items. Each other item has the id of a [`MenuCommand`], and
//! `menu_bar` maps it to an existing intent or app action. The Edit items
//! have the id of an [`EditCommand`]: the app gives egui the events of the
//! key press. Linux and Windows have no native menu bar.

use std::sync::{Mutex, PoisonError};

use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{AboutMetadata, CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use thinwire_core::ThemeMode;
use thinwire_protocol::ProtocolKind;

use super::menu_bar::{EditCommand, MenuCommand};

/// The menu bar of the app. It must live as long as the app: AppKit keeps
/// only a weak link to the items.
pub(crate) struct NativeMenu {
    _menu: Menu,
    sidebar: CheckMenuItem,
    themes: Vec<(ThemeMode, CheckMenuItem)>,
    shown: Option<(bool, ThemeMode)>,
}

fn cmd(code: Code) -> Option<Accelerator> {
    Some(Accelerator::new(Modifiers::META, code))
}

fn cmd_shift(code: Code) -> Option<Accelerator> {
    Some(Accelerator::new(Modifiers::META | Modifiers::SHIFT, code))
}

fn item(command: MenuCommand, text: &str, accelerator: Option<Accelerator>) -> MenuItem {
    MenuItem::with_id(command.id(), text, true, accelerator)
}

/// An Edit item. Not the native AppKit item: that one sends `copy:`, ... to
/// the winit view, which ignores it (see [`EditCommand`]).
fn edit_item(command: EditCommand, text: &str, accelerator: Option<Accelerator>) -> MenuItem {
    MenuItem::with_id(command.id(), text, true, accelerator)
}

const PROTOCOL_KEYS: [Code; 9] = [
    Code::Digit1,
    Code::Digit2,
    Code::Digit3,
    Code::Digit4,
    Code::Digit5,
    Code::Digit6,
    Code::Digit7,
    Code::Digit8,
    Code::Digit9,
];

impl NativeMenu {
    /// Build the menu bar and give it to `NSApp`. Call it on the main thread,
    /// after the event loop started. `None` if a menu could not be built.
    pub(crate) fn install(build: Option<&str>) -> Option<Self> {
        match Self::build(build) {
            Ok(menu) => Some(menu),
            Err(_) => {
                tracing::warn!(target: "thinwire::menu", "the menu bar was not built");
                None
            }
        }
    }

    fn build(build: Option<&str>) -> muda::Result<Self> {
        let about = AboutMetadata {
            name: Some("Thinwire".to_string()),
            version: build.map(str::to_owned),
            short_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            website: Some(super::menu_bar::PROJECT_PAGE.to_string()),
            ..AboutMetadata::default()
        };
        let app_menu = Submenu::with_items(
            "Thinwire",
            true,
            &[
                &PredefinedMenuItem::about(Some("About Thinwire"), Some(about)),
                &item(MenuCommand::CheckForUpdates, "Check for Updates…", None),
                &PredefinedMenuItem::separator(),
                &item(MenuCommand::Settings, "Settings…", cmd(Code::Comma)),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::services(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::hide(Some("Hide Thinwire")),
                &PredefinedMenuItem::hide_others(None),
                &PredefinedMenuItem::show_all(None),
                &PredefinedMenuItem::separator(),
                &item(MenuCommand::Quit, "Quit Thinwire", cmd(Code::KeyQ)),
            ],
        )?;
        let edit_menu = Submenu::with_items(
            "Edit",
            true,
            &[
                &edit_item(EditCommand::Undo, "Undo", cmd(Code::KeyZ)),
                &edit_item(EditCommand::Redo, "Redo", cmd_shift(Code::KeyZ)),
                &PredefinedMenuItem::separator(),
                &edit_item(EditCommand::Cut, "Cut", cmd(Code::KeyX)),
                &edit_item(EditCommand::Copy, "Copy", cmd(Code::KeyC)),
                &edit_item(EditCommand::Paste, "Paste", cmd(Code::KeyV)),
                &edit_item(EditCommand::SelectAll, "Select All", cmd(Code::KeyA)),
            ],
        )?;

        let sidebar = CheckMenuItem::with_id(
            MenuCommand::ToggleSidebar.id(),
            "Collapse Sidebar",
            true,
            false,
            cmd_shift(Code::KeyS),
        );
        let themes: Vec<(ThemeMode, CheckMenuItem)> = [
            (ThemeMode::System, "System"),
            (ThemeMode::Light, "Light"),
            (ThemeMode::Dark, "Dark"),
        ]
        .into_iter()
        .map(|(mode, text)| {
            let check =
                CheckMenuItem::with_id(MenuCommand::Theme(mode).id(), text, true, false, None);
            (mode, check)
        })
        .collect();
        let theme_menu = Submenu::new("Theme", true);
        for (_, check) in &themes {
            theme_menu.append(check)?;
        }
        let view_menu = Submenu::with_items(
            "View",
            true,
            &[
                &sidebar,
                &theme_menu,
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::fullscreen(None),
            ],
        )?;

        let go_menu = Submenu::with_items(
            "Go",
            true,
            &[&item(
                MenuCommand::SearchChats,
                "Search Chats",
                cmd(Code::KeyF),
            )],
        )?;
        go_menu.append(&PredefinedMenuItem::separator())?;
        for (index, kind) in ProtocolKind::ALL.iter().enumerate() {
            let accelerator = PROTOCOL_KEYS.get(index).and_then(|code| cmd(*code));
            go_menu.append(&item(
                MenuCommand::Protocol(index),
                kind.display_name(),
                accelerator,
            ))?;
        }

        let window_menu = Submenu::with_items(
            "Window",
            true,
            &[
                &PredefinedMenuItem::minimize(None),
                &PredefinedMenuItem::maximize(Some("Zoom")),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::bring_all_to_front(None),
            ],
        )?;
        let help_menu = Submenu::with_items(
            "Help",
            true,
            &[
                &item(MenuCommand::ProjectPage, "Thinwire on GitHub", None),
                &item(MenuCommand::ReportIssue, "Report an Issue", None),
            ],
        )?;

        let menu = Menu::with_items(&[
            &app_menu,
            &edit_menu,
            &view_menu,
            &go_menu,
            &window_menu,
            &help_menu,
        ])?;
        menu.init_for_nsapp();
        window_menu.set_as_windows_menu_for_nsapp();
        help_menu.set_as_help_menu_for_nsapp();
        Ok(Self {
            _menu: menu,
            sidebar,
            themes,
            shown: None,
        })
    }

    /// A click on a check item flips its mark in AppKit. Set every mark
    /// again at the next `sync`, also when the setting did not change.
    pub(crate) fn invalidate(&mut self) {
        self.shown = None;
    }

    /// Set the check marks to the settings of this frame. It changes an
    /// item only when its setting changed.
    pub(crate) fn sync(&mut self, rail_collapsed: bool, theme: ThemeMode) {
        if self.shown == Some((rail_collapsed, theme)) {
            return;
        }
        self.shown = Some((rail_collapsed, theme));
        self.sidebar.set_checked(rail_collapsed);
        for (mode, check) in &self.themes {
            check.set_checked(*mode == theme);
        }
    }
}

/// The menu items that the user chose since the last frame.
static CHOSEN: Mutex<Vec<MenuCommand>> = Mutex::new(Vec::new());

/// The Edit items that the user chose since the last frame.
static EDITS: Mutex<Vec<EditCommand>> = Mutex::new(Vec::new());

/// Keep each chosen command for the next frame, and ask for that frame at
/// once, so an idle window runs the command with no wait.
pub(crate) fn wake_on_event(ctx: &eframe::egui::Context) {
    let ctx = ctx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let id = event.id().as_ref();
        if let Some(command) = EditCommand::from_id(id) {
            EDITS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(command);
        } else if let Some(command) = MenuCommand::from_id(id) {
            CHOSEN
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(command);
        }
        ctx.request_repaint();
    }));
}

/// The commands that the user chose in the menu bar since the last frame.
pub(crate) fn take_commands() -> Vec<MenuCommand> {
    std::mem::take(&mut *CHOSEN.lock().unwrap_or_else(PoisonError::into_inner))
}

/// The Edit items that the user chose since the last frame.
pub(crate) fn take_edits() -> Vec<EditCommand> {
    std::mem::take(&mut *EDITS.lock().unwrap_or_else(PoisonError::into_inner))
}
