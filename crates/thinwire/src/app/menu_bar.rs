//! The commands of the app menu bar and the Settings "About" section.
//!
//! macOS shows these commands in the native menu bar (`macos_menu`). Linux
//! and Windows have no native menu bar: Settings has "About Thinwire" and
//! "Check for Updates" there. The mapping from a menu id to a command, and
//! from a command to its effects, is plain code that every OS tests.

// Only the macOS menu bar sends the commands. Linux and Windows build and
// test them, and use only the Settings requests.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use eframe::egui;
use thinwire_core::{Intent, ThemeMode};
use thinwire_protocol::{ProtocolId, ProtocolKind};

/// The address of the project on GitHub.
pub(crate) const PROJECT_PAGE: &str = "https://github.com/jaysonsantos/thinwire";
/// The page where a user reports a problem.
pub(crate) const ISSUES_PAGE: &str = "https://github.com/jaysonsantos/thinwire/issues/new";

/// A command of the menu bar that the app runs. The standard commands
/// (About, Hide, Copy, Minimize, ...) are native and send nothing.
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
