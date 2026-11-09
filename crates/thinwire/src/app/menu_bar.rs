//! The commands of the app menu bar and the Settings "About" section.
//!
//! macOS shows these commands in the native menu bar (`macos_menu`). Linux
//! and Windows have no native menu bar: Settings has "About Thinwire" and
//! "Check for Updates" there. The mapping from a menu id to a command, and
//! from a command to its effects, is plain code that every OS tests.

// Only the macOS menu bar sends the commands. Linux and Windows build and
// test them, and use only the Settings requests.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::sync::Arc;

use eframe::egui;
use thinwire_core::{Intent, ThemeMode};
use thinwire_protocol::{ProtocolId, ProtocolKind};

/// The address of the project on GitHub.
pub(crate) const PROJECT_PAGE: &str = "https://github.com/jaysonsantos/thinwire";
/// The page where a user reports a problem.
pub(crate) const ISSUES_PAGE: &str = "https://github.com/jaysonsantos/thinwire/issues/new";

/// A command of the menu bar that the app runs. The standard commands
/// (About, Hide, Minimize, ...) are native and send nothing. The Edit
/// commands are [`EditCommand`]s.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MenuCommand {
    CheckForUpdates,
    Settings,
    Quit,
    ToggleSidebar,
    Theme(ThemeMode),
    SearchChats,
    /// The first account of the protocol at this place in `ProtocolKind::ALL`.
    Protocol(usize),
    ProjectPage,
    ReportIssue,
}

/// What the app does for a command.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum MenuEffect {
    Intent(Intent),
    CheckForUpdates,
    OpenSettings,
    FocusSearch,
    OpenUrl(&'static str),
    Quit,
}

const CHECK_FOR_UPDATES: &str = "thinwire.check-for-updates";
const SETTINGS: &str = "thinwire.settings";
const QUIT: &str = "thinwire.quit";
const TOGGLE_SIDEBAR: &str = "thinwire.toggle-sidebar";
const THEME_SYSTEM: &str = "thinwire.theme.system";
const THEME_LIGHT: &str = "thinwire.theme.light";
const THEME_DARK: &str = "thinwire.theme.dark";
const SEARCH_CHATS: &str = "thinwire.search-chats";
const PROTOCOL_PREFIX: &str = "thinwire.protocol.";
const PROJECT: &str = "thinwire.project-page";
const REPORT_ISSUE: &str = "thinwire.report-issue";

impl MenuCommand {
    /// The id of the menu item of this command.
    pub(crate) fn id(self) -> String {
        match self {
            Self::CheckForUpdates => CHECK_FOR_UPDATES.to_string(),
            Self::Settings => SETTINGS.to_string(),
            Self::Quit => QUIT.to_string(),
            Self::ToggleSidebar => TOGGLE_SIDEBAR.to_string(),
            Self::Theme(ThemeMode::System) => THEME_SYSTEM.to_string(),
            Self::Theme(ThemeMode::Light) => THEME_LIGHT.to_string(),
            Self::Theme(ThemeMode::Dark) => THEME_DARK.to_string(),
            Self::SearchChats => SEARCH_CHATS.to_string(),
            Self::Protocol(index) => format!("{PROTOCOL_PREFIX}{index}"),
            Self::ProjectPage => PROJECT.to_string(),
            Self::ReportIssue => REPORT_ISSUE.to_string(),
        }
    }

    /// The command of a menu item id. `None` for a native item.
    pub(crate) fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            CHECK_FOR_UPDATES => Self::CheckForUpdates,
            SETTINGS => Self::Settings,
            QUIT => Self::Quit,
            TOGGLE_SIDEBAR => Self::ToggleSidebar,
            THEME_SYSTEM => Self::Theme(ThemeMode::System),
            THEME_LIGHT => Self::Theme(ThemeMode::Light),
            THEME_DARK => Self::Theme(ThemeMode::Dark),
            SEARCH_CHATS => Self::SearchChats,
            PROJECT => Self::ProjectPage,
            REPORT_ISSUE => Self::ReportIssue,
            other => {
                let index: usize = other.strip_prefix(PROTOCOL_PREFIX)?.parse().ok()?;
                if index >= ProtocolKind::ALL.len() {
                    return None;
                }
                Self::Protocol(index)
            }
        })
    }

    /// The effect of this command. `rail_collapsed` is the sidebar state now.
    pub(crate) fn effect(self, rail_collapsed: bool) -> MenuEffect {
        match self {
            Self::CheckForUpdates => MenuEffect::CheckForUpdates,
            Self::Settings => MenuEffect::OpenSettings,
            Self::Quit => MenuEffect::Quit,
            Self::ToggleSidebar => MenuEffect::Intent(Intent::SetRailCollapsed(!rail_collapsed)),
            Self::Theme(mode) => MenuEffect::Intent(Intent::SetTheme(mode)),
            Self::SearchChats => MenuEffect::FocusSearch,
            Self::Protocol(index) => MenuEffect::Intent(Intent::SelectProtocol(
                ProtocolId::primary(ProtocolKind::ALL[index]),
            )),
            Self::ProjectPage => MenuEffect::OpenUrl(PROJECT_PAGE),
            Self::ReportIssue => MenuEffect::OpenUrl(ISSUES_PAGE),
        }
    }
}

// region: the Edit menu

/// An item of the Edit menu.
///
/// The native Edit items of AppKit send `copy:`, `paste:`, ... to the first
/// responder. The winit view has no such method, so a native item did
/// nothing, and its key equivalent (⌘C, ⌘V, ...) did not reach egui. These
/// items are app items: the app changes each one into the egui events that
/// egui-winit makes for the same key press.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum EditCommand {
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
}

/// What a paste reads from the clipboard.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Clipboard {
    Text(String),
    Image(Arc<egui::ColorImage>),
    Empty,
}

const EDIT_UNDO: &str = "thinwire.edit.undo";
const EDIT_REDO: &str = "thinwire.edit.redo";
const EDIT_CUT: &str = "thinwire.edit.cut";
const EDIT_COPY: &str = "thinwire.edit.copy";
const EDIT_PASTE: &str = "thinwire.edit.paste";
const EDIT_SELECT_ALL: &str = "thinwire.edit.select-all";

/// The modifiers that egui-winit sets for the ⌘ key on macOS. egui reads
/// `command`, so the same events also work on Linux and Windows.
const COMMAND_KEY: egui::Modifiers = egui::Modifiers {
    alt: false,
    ctrl: false,
    shift: false,
    mac_cmd: true,
    command: true,
};

impl EditCommand {
    /// The id of the menu item of this command.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Undo => EDIT_UNDO,
            Self::Redo => EDIT_REDO,
            Self::Cut => EDIT_CUT,
            Self::Copy => EDIT_COPY,
            Self::Paste => EDIT_PASTE,
            Self::SelectAll => EDIT_SELECT_ALL,
        }
    }

    /// The command of a menu item id. `None` for another item.
    pub(crate) fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            EDIT_UNDO => Self::Undo,
            EDIT_REDO => Self::Redo,
            EDIT_CUT => Self::Cut,
            EDIT_COPY => Self::Copy,
            EDIT_PASTE => Self::Paste,
            EDIT_SELECT_ALL => Self::SelectAll,
            _ => return None,
        })
    }

    /// The egui events of this command: the events that egui-winit makes
    /// for the key press. Only a paste reads the clipboard. Like egui-winit,
    /// a paste prefers text, and an empty text pastes nothing.
    pub(crate) fn events(self, clipboard: impl FnOnce() -> Clipboard) -> Vec<egui::Event> {
        match self {
            Self::Undo => key_tap(egui::Key::Z, COMMAND_KEY),
            Self::Redo => key_tap(egui::Key::Z, COMMAND_KEY | egui::Modifiers::SHIFT),
            Self::Cut => vec![egui::Event::Cut],
            Self::Copy => vec![egui::Event::Copy],
            Self::Paste => match clipboard() {
                Clipboard::Text(text) => {
                    let text = text.replace("\r\n", "\n");
                    if text.is_empty() {
                        Vec::new()
                    } else {
                        vec![egui::Event::Paste(text)]
                    }
                }
                Clipboard::Image(image) => vec![egui::Event::PasteImage(image)],
                Clipboard::Empty => Vec::new(),
            },
            Self::SelectAll => key_tap(egui::Key::A, COMMAND_KEY),
        }
    }

    /// egui-winit already made the event of this command in `events`: the
    /// key press reached the window too.
    fn already_in(self, events: &[egui::Event]) -> bool {
        events.iter().any(|event| match (self, event) {
            (Self::Cut, egui::Event::Cut) | (Self::Copy, egui::Event::Copy) => true,
            (Self::Paste, egui::Event::Paste(_) | egui::Event::PasteImage(_)) => true,
            (
                Self::Undo | Self::Redo | Self::SelectAll,
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                },
            ) if modifiers.command => match self {
                Self::Undo => *key == egui::Key::Z && !modifiers.shift,
                Self::Redo => (*key == egui::Key::Z && modifiers.shift) || *key == egui::Key::Y,
                _ => *key == egui::Key::A,
            },
            _ => false,
        })
    }
}

/// A press and a release of `key`.
fn key_tap(key: egui::Key, modifiers: egui::Modifiers) -> Vec<egui::Event> {
    [true, false]
        .into_iter()
        .map(|pressed| egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers,
        })
        .collect()
}

/// Add the events of the chosen Edit items to the input of the next frame.
/// A command whose event egui-winit already made in this input adds
/// nothing, so one key press never acts two times.
pub(crate) fn push_edit_events(
    input: &mut egui::RawInput,
    commands: &[EditCommand],
    mut clipboard: impl FnMut() -> Clipboard,
) {
    let from_window = input.events.len();
    for command in commands {
        if command.already_in(&input.events[..from_window]) {
            continue;
        }
        let events = command.events(&mut clipboard);
        input.events.extend(events);
    }
}

/// The text or the image on the OS clipboard, as egui-winit reads it.
pub(crate) fn read_clipboard() -> Clipboard {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return Clipboard::Empty;
    };
    if let Ok(text) = clipboard.get_text() {
        return Clipboard::Text(text);
    }
    match clipboard.get_image() {
        Ok(image) => Clipboard::Image(Arc::new(egui::ColorImage::from_rgba_unmultiplied(
            [image.width, image.height],
            &image.bytes,
        ))),
        Err(_) => Clipboard::Empty,
    }
}

// endregion

/// The text of the build line in Settings.
pub(crate) fn build_text(build: Option<&str>) -> String {
    match build {
        Some(build) => format!("Thinwire {build}"),
        None => "Thinwire (local build)".to_string(),
    }
}

// region: requests between the app, the menus, and the UI

/// The build id that Settings shows. The app sets it each frame.
fn about_id() -> egui::Id {
    egui::Id::unique("thinwire-about-build")
}

fn check_id() -> egui::Id {
    egui::Id::unique("thinwire-check-for-updates")
}

fn settings_id() -> egui::Id {
    egui::Id::unique("thinwire-open-settings")
}

fn search_id() -> egui::Id {
    egui::Id::unique("thinwire-focus-search")
}

pub(crate) fn set_about_build(ctx: &egui::Context, build: Option<&str>) {
    let build = build.map(str::to_owned);
    ctx.data_mut(|data| data.insert_temp(about_id(), build));
}

pub(crate) fn about_build(ctx: &egui::Context) -> Option<String> {
    ctx.data(|data| data.get_temp::<Option<String>>(about_id()))
        .flatten()
}

fn request(ctx: &egui::Context, id: egui::Id) {
    ctx.data_mut(|data| data.insert_temp(id, true));
    ctx.request_repaint();
}

fn take(ctx: &egui::Context, id: egui::Id) -> bool {
    ctx.data_mut(|data| data.remove_temp::<bool>(id))
        .unwrap_or(false)
}

/// Settings or the menu bar asked for an update check.
pub(crate) fn request_update_check(ctx: &egui::Context) {
    request(ctx, check_id());
}

pub(crate) fn take_update_check(ctx: &egui::Context) -> bool {
    take(ctx, check_id())
}

/// The menu bar asked to open the Settings menu.
pub(crate) fn request_open_settings(ctx: &egui::Context) {
    request(ctx, settings_id());
}

pub(crate) fn take_open_settings(ctx: &egui::Context) -> bool {
    take(ctx, settings_id())
}

/// The menu bar asked to focus the chat search.
pub(crate) fn request_focus_search(ctx: &egui::Context) {
    request(ctx, search_id());
}

pub(crate) fn take_focus_search(ctx: &egui::Context) -> bool {
    take(ctx, search_id())
}

// endregion

#[cfg(test)]
mod tests {
    use super::*;

    fn every_command() -> Vec<MenuCommand> {
        let mut all = vec![
            MenuCommand::CheckForUpdates,
            MenuCommand::Settings,
            MenuCommand::Quit,
            MenuCommand::ToggleSidebar,
            MenuCommand::Theme(ThemeMode::System),
            MenuCommand::Theme(ThemeMode::Light),
            MenuCommand::Theme(ThemeMode::Dark),
            MenuCommand::SearchChats,
            MenuCommand::ProjectPage,
            MenuCommand::ReportIssue,
        ];
        all.extend((0..ProtocolKind::ALL.len()).map(MenuCommand::Protocol));
        all
    }

    #[test]
    fn each_command_id_reads_back_to_its_command() {
        for command in every_command() {
            assert_eq!(MenuCommand::from_id(&command.id()), Some(command));
        }
    }

    #[test]
    fn a_native_or_unknown_id_is_no_command() {
        assert_eq!(MenuCommand::from_id("1"), None);
        assert_eq!(MenuCommand::from_id("thinwire.protocol.x"), None);
        let past_the_end = format!("{PROTOCOL_PREFIX}{}", ProtocolKind::ALL.len());
        assert_eq!(MenuCommand::from_id(&past_the_end), None);
    }

    #[test]
    fn the_commands_reuse_the_intents_of_the_app() {
        assert_eq!(
            MenuCommand::ToggleSidebar.effect(false),
            MenuEffect::Intent(Intent::SetRailCollapsed(true))
        );
        assert_eq!(
            MenuCommand::ToggleSidebar.effect(true),
            MenuEffect::Intent(Intent::SetRailCollapsed(false))
        );
        assert_eq!(
            MenuCommand::Theme(ThemeMode::Dark).effect(false),
            MenuEffect::Intent(Intent::SetTheme(ThemeMode::Dark))
        );
        assert_eq!(
            MenuCommand::Protocol(0).effect(false),
            MenuEffect::Intent(Intent::SelectProtocol(ProtocolId::primary(
                ProtocolKind::ALL[0]
            )))
        );
        assert_eq!(
            MenuCommand::CheckForUpdates.effect(false),
            MenuEffect::CheckForUpdates
        );
        assert_eq!(
            MenuCommand::Settings.effect(false),
            MenuEffect::OpenSettings
        );
        assert_eq!(
            MenuCommand::SearchChats.effect(false),
            MenuEffect::FocusSearch
        );
        assert_eq!(MenuCommand::Quit.effect(false), MenuEffect::Quit);
        assert_eq!(
            MenuCommand::ProjectPage.effect(false),
            MenuEffect::OpenUrl(PROJECT_PAGE)
        );
        assert_eq!(
            MenuCommand::ReportIssue.effect(false),
            MenuEffect::OpenUrl(ISSUES_PAGE)
        );
    }

    const EDITS: [EditCommand; 6] = [
        EditCommand::Undo,
        EditCommand::Redo,
        EditCommand::Cut,
        EditCommand::Copy,
        EditCommand::Paste,
        EditCommand::SelectAll,
    ];

    fn no_clipboard() -> Clipboard {
        panic!("only a paste reads the clipboard")
    }

    fn key(key: egui::Key, pressed: bool, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn each_edit_id_reads_back_and_is_no_menu_command() {
        for command in EDITS {
            assert_eq!(EditCommand::from_id(command.id()), Some(command));
            assert_eq!(MenuCommand::from_id(command.id()), None);
        }
        for command in every_command() {
            assert_eq!(EditCommand::from_id(&command.id()), None);
        }
    }

    #[test]
    fn edit_commands_make_the_events_of_their_key_press() {
        let cmd = egui::Modifiers::MAC_CMD | egui::Modifiers::COMMAND;
        assert_eq!(EditCommand::Cut.events(no_clipboard), [egui::Event::Cut]);
        assert_eq!(EditCommand::Copy.events(no_clipboard), [egui::Event::Copy]);
        assert_eq!(
            EditCommand::SelectAll.events(no_clipboard),
            [key(egui::Key::A, true, cmd), key(egui::Key::A, false, cmd)]
        );
        assert_eq!(
            EditCommand::Undo.events(no_clipboard),
            [key(egui::Key::Z, true, cmd), key(egui::Key::Z, false, cmd)]
        );
        let shift = cmd | egui::Modifiers::SHIFT;
        assert_eq!(
            EditCommand::Redo.events(no_clipboard),
            [
                key(egui::Key::Z, true, shift),
                key(egui::Key::Z, false, shift)
            ]
        );
    }

    #[test]
    fn a_paste_reads_text_or_an_image_from_the_clipboard() {
        assert_eq!(
            EditCommand::Paste.events(|| Clipboard::Text("one\r\ntwo".into())),
            [egui::Event::Paste("one\ntwo".into())]
        );
        assert!(
            EditCommand::Paste
                .events(|| Clipboard::Text(String::new()))
                .is_empty()
        );
        assert!(EditCommand::Paste.events(|| Clipboard::Empty).is_empty());
        let image = Arc::new(egui::ColorImage::filled(
            [1, 1],
            egui::Color32::from_rgb(1, 2, 3),
        ));
        assert_eq!(
            EditCommand::Paste.events(|| Clipboard::Image(image.clone())),
            [egui::Event::PasteImage(image)]
        );
    }

    #[test]
    fn a_key_press_that_reached_the_window_acts_one_time() {
        let mut input = egui::RawInput {
            events: vec![
                egui::Event::Copy,
                egui::Event::Paste("from the window".into()),
                key(egui::Key::A, true, egui::Modifiers::COMMAND),
            ],
            ..Default::default()
        };
        let before = input.events.clone();
        push_edit_events(
            &mut input,
            &[
                EditCommand::Copy,
                EditCommand::Paste,
                EditCommand::SelectAll,
            ],
            no_clipboard,
        );
        assert_eq!(input.events, before);

        // A release only, as AppKit sends a key up after the menu took the
        // key down: the menu item still acts.
        let mut input = egui::RawInput {
            events: vec![key(egui::Key::A, false, egui::Modifiers::COMMAND)],
            ..Default::default()
        };
        push_edit_events(
            &mut input,
            &[EditCommand::SelectAll, EditCommand::Cut],
            no_clipboard,
        );
        assert_eq!(input.events.len(), 4);
        assert_eq!(input.events[3], egui::Event::Cut);
    }

    #[test]
    fn two_menu_pastes_in_one_frame_both_paste() {
        let mut input = egui::RawInput::default();
        push_edit_events(
            &mut input,
            &[EditCommand::Paste, EditCommand::Paste],
            || Clipboard::Text("x".into()),
        );
        assert_eq!(
            input.events,
            [
                egui::Event::Paste("x".into()),
                egui::Event::Paste("x".into())
            ]
        );
    }

    #[test]
    fn settings_names_the_build_or_says_local_build() {
        assert_eq!(
            build_text(Some("0.1.0.r298.g0be7e95")),
            "Thinwire 0.1.0.r298.g0be7e95"
        );
        assert_eq!(build_text(None), "Thinwire (local build)");
    }

    #[test]
    fn a_request_is_taken_one_time() {
        let ctx = egui::Context::default();
        assert!(!take_update_check(&ctx));
        request_update_check(&ctx);
        assert!(take_update_check(&ctx));
        assert!(!take_update_check(&ctx));
        request_open_settings(&ctx);
        assert!(take_open_settings(&ctx));
        request_focus_search(&ctx);
        assert!(take_focus_search(&ctx));
        set_about_build(&ctx, Some("0.1.0.r1.g0000001"));
        assert_eq!(about_build(&ctx).as_deref(), Some("0.1.0.r1.g0000001"));
    }
}
