//! Shell: the account sidebar, the selected account's chat list, then the open
//! chat. A status banner sits above all three when there is something to say.

use std::time::Instant;

use eframe::egui::{self, AtomExt, RichText};
use thinwire_core::ViewNow;
use thinwire_core::mutes::ChatMute;
use thinwire_protocol::{
    AccountState, AdapterStatus, ChatAvatar, Delivery, MessageLink, MessagePhoto, MessageTopic,
    ProtocolId, SIGNAL_DEVICE_REMOVED, SIGNAL_LINK_CHECK, StoreAccessReason, topic_mute_id,
};

use super::auth;
use super::icons;
use super::motion;
use super::theme::{self, radius, size, space};
use super::theme_mode::ThemeModeEgui;
use super::thread_layout::{RowLayout, list_time, thread_rows};
#[cfg(feature = "slack-oauth")]
use thinwire_core::SlackIntent;
use thinwire_core::secrets::Persistence;
use thinwire_core::state::{
    AccountRow, AuthKey, AuthScreen, CenterView, InboxState, KEYCHAIN_READ_FAILED,
    LOADING_CHATS_STATUS, OlderState, RESUME_CONNECTING, Snapshot, ThreadState, notice_label,
    unofficial_notice,
};
use thinwire_core::{Intent, TelegramIntent, ThemeMode, View};
use thinwire_core::{SignalIntent, WhatsAppIntent};

/// Shown while the OS keychain is not available. Secrets stay in memory.
pub(crate) const KEYCHAIN_UNAVAILABLE_NOTICE: &str =
    "Sign-in is not saved on this device: keychain unavailable.";

/// Shown when only the kernel keyring works. It is lost at restart.
pub(crate) const KEYCHAIN_UNTIL_RESTART_NOTICE: &str =
    "Sign-in is kept until you restart the computer.";

/// One notice for the whole window. `None` while the keychain loads or keeps
/// the sign-in across restarts.
#[must_use]
pub(crate) fn keychain_notice(persistence: Persistence) -> Option<&'static str> {
    match persistence {
        Persistence::ThisSession => Some(KEYCHAIN_UNAVAILABLE_NOTICE),
        Persistence::UntilRestart => Some(KEYCHAIN_UNTIL_RESTART_NOTICE),
        Persistence::Loading | Persistence::Saved => None,
    }
}

/// Width the account sidebar opens at, and when the window is narrow.
const RAIL_WIDTH: f32 = 216.0;
const RAIL_WIDTH_NARROW: f32 = 188.0;
/// Width the chat list opens at, and when the window is narrow.
const LIST_WIDTH: f32 = 312.0;
const LIST_WIDTH_NARROW: f32 = 256.0;
/// Below this window width the two side columns open narrower, so the thread
/// keeps room for a readable line.
const NARROW_WINDOW: f32 = 960.0;

/// The window is narrow: the side columns open at their smaller width.
fn narrow_window(ui: &egui::Ui) -> bool {
    ui.ctx().content_rect().width() < NARROW_WINDOW
}
/// Gap between two rows of a list.
const ROW_GAP: f32 = 2.0;
const TILE: f32 = theme::TILE_SIDE;
/// Side of the icon on a sidebar button.
const RAIL_ICON: f32 = 16.0;
/// Painter for one of the icons in [`icons`].
type Icon = fn(&egui::Painter, egui::Pos2, f32, egui::Color32);

const COMPOSE_MAX_ROWS: usize = 5;
/// Diameter of the round Send button, and the least height of the compose
/// row. The field grows with the line count.
const SEND_MIN_HEIGHT: f32 = 40.0;
/// Focused field border. Unfocused is 1px. The reserve keeps the thicker ring.
const FIELD_STROKE_MAX: f32 = 2.0;
/// A message bubble uses at most this share of the thread width.
const BUBBLE_WIDTH: f32 = 0.78;
/// Cap so a wide window does not make a line too long to read.
const BUBBLE_MAX: f32 = 540.0;
/// Gap between bubbles in one sender run.
const SAME_RUN_GAP: f32 = 3.0;
/// Font size of a message that is only emoji. It has no bubble.
const EMOJI_SIZE: f32 = 38.0;
/// Height of the soft edge where the messages scroll under the header or
/// the compose bar.
const SCROLL_FADE: f32 = 18.0;
/// Side of an icon button, and of the icon inside it.
const ICON_BUTTON: f32 = 36.0;
const ICON_BUTTON_ICON: f32 = 18.0;
/// Padding inside a bubble, left and right, then top and bottom.
const BUBBLE_PAD_X: i8 = 14;
const BUBBLE_PAD_Y: i8 = 9;
/// Gap before the first bubble of the next sender run.
const NEXT_RUN_GAP: f32 = 12.0;
/// Space left and right of the messages and the compose bar.
const THREAD_INSET: f32 = space::XL;

/// One-shot hints from the core. A hint counts as used only where its widget
/// draws, so a hidden widget does not lose it (qa L1).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Hints {
    focus_compose: bool,
    scroll_to_selected: bool,
    scroll_to_focused: bool,
    used_focus_compose: bool,
    used_scroll_to_selected: bool,
    used_scroll_to_focused: bool,
    /// Snapshot only. See [`Hints::show_open_mute_menus`].
    open_mute_menus: bool,
}

impl Hints {
    /// Hints that wait in the core this frame.
    pub(crate) fn from_view(view: &View<'_>) -> Self {
        Self {
            focus_compose: view.wants_focus_compose(),
            scroll_to_selected: view.wants_scroll_to_selected(),
            scroll_to_focused: view.wants_scroll_to_focused(),
            ..Self::default()
        }
    }

    /// Call only where the compose field draws.
    pub(crate) fn take_focus_compose(&mut self) -> bool {
        self.used_focus_compose = self.focus_compose;
        self.focus_compose
    }

    /// Call only where the inbox rows draw.
    pub(crate) fn take_scroll_to_selected(&mut self) -> bool {
        self.used_scroll_to_selected = self.scroll_to_selected;
        self.scroll_to_selected
    }

    /// The focus hint was used this frame. The app clears it in the core.
    pub(crate) const fn used_focus_compose(self) -> bool {
        self.used_focus_compose
    }

    /// The scroll hint was used this frame. The app clears it in the core.
    pub(crate) const fn used_scroll_to_selected(self) -> bool {
        self.used_scroll_to_selected
    }

    /// Call only where the inbox rows draw.
    pub(crate) fn take_scroll_to_focused(&mut self) -> bool {
        self.used_scroll_to_focused = self.scroll_to_focused;
        self.scroll_to_focused
    }

    /// The keyboard-highlight scroll hint was used this frame.
    pub(crate) const fn used_scroll_to_focused(self) -> bool {
        self.used_scroll_to_focused
    }

    /// Open the row menu once for each mute state. The app leaves this off.
    /// The mute-menu snapshot turns it on (#206). egui keeps one memory popup,
    /// so the snapshot cannot right-click three rows and keep every menu.
    #[cfg_attr(not(all(test, feature = "ui-snapshots")), allow(dead_code))]
    pub(crate) fn show_open_mute_menus(&mut self) {
        self.open_mute_menus = true;
    }
}

/// Draw one frame. User actions go to `out`; the app dispatches them after.
pub(crate) fn draw(
    ui: &mut egui::Ui,
    snapshot: &View<'_>,
    hints: &mut Hints,
    out: &mut Vec<Intent>,
) {
    super::theme_mode::apply(ui.ctx(), snapshot.theme());
    #[cfg(feature = "whatsapp-web")]
    if snapshot.whatsapp_pairing_available() && snapshot.whatsapp_gate_open() {
        super::whatsapp_gate::draw(ui, snapshot, out);
        return;
    }
    #[cfg(feature = "signal-local")]
    if snapshot.signal_linking_available() && snapshot.signal_gate_open() {
        super::signal_gate::draw(ui, snapshot, out);
        return;
    }
    status_strip(ui, snapshot, out);
    account_rail(ui, snapshot, out);
    chat_column(ui, snapshot, hints, out);
    center_panel(ui, snapshot, hints, out);
    notice_review(ui, snapshot, out);
    super::links::confirm(ui);
    super::photos::overlay(ui, snapshot.selected_messages());
}

/// Single-line or multiline text bound to a core value. A change becomes an intent.
pub(crate) fn edited(
    ui: &mut egui::Ui,
    current: &str,
    edit: impl FnOnce(&mut String) -> egui::TextEdit<'_>,
) -> Option<String> {
    let mut text = current.to_owned();
    let changed = ui.add(edit(&mut text)).changed();
    changed.then_some(text)
}

/// Side of the busy spinner in the chat list header.
const BUSY_SPINNER: f32 = 14.0;

/// A running load or send, left of the menu. It takes no row of its own, so
/// the inbox and the thread do not move down and up for each send.
/// Motion on: the busy line and a spinner.
/// OS reduce-motion: the still label [`motion::WORKING`].
/// A finished line or a failure stays on the strip.
fn busy_indicator(ui: &mut egui::Ui, line: Option<&str>) {
    let id = ui.scope_id().with("busy-line");
    let text = match line {
        Some(text) => {
            ui.data_mut(|data| data.insert_temp(id, text.to_owned()));
            text.to_owned()
        }
        // The last line stays on screen while it fades out.
        None => ui
            .data(|data| data.get_temp::<String>(id))
            .unwrap_or_default(),
    };
    let shown = motion::toggle(ui.ctx(), id, line.is_some(), motion::FADE);
    if shown <= 0.0 || text.is_empty() {
        return;
    }
    if !motion::enabled(ui.ctx()) {
        ui.scope(|ui| {
            ui.multiply_opacity(shown);
            let palette = theme::palette(ui);
            ui.add(
                egui::Label::new(
                    RichText::new(motion::WORKING)
                        .text_style(theme::secondary())
                        .color(palette.text2),
                )
                .truncate(),
            );
        });
        return;
    }
    ui.scope(|ui| {
        ui.multiply_opacity(shown);
        let palette = theme::palette(ui);
        let room = (ui.available_width() - BUSY_SPINNER - ui.spacing().item_spacing.x).max(0.0);
        ui.scope(|ui| {
            ui.set_max_width(room);
            ui.add(
                egui::Label::new(
                    RichText::new(&text)
                        .text_style(theme::secondary())
                        .color(palette.text2),
                )
                .truncate(),
            );
        });
        motion::busy_spinner(ui, BUSY_SPINNER, palette.text3);
    });
}

/// Glass drawn in the search field, inside [`theme::search_field_margin`].
const SEARCH_ICON: f32 = 14.0;

fn search_field(ui: &mut egui::Ui, search: &str) -> Option<String> {
    let palette = theme::palette(ui);
    let id = ui.make_persistent_id("chat-search");
    let focused = ui.memory(|memory| memory.has_focus(id));
    let ring = motion::toggle(ui.ctx(), id.with("ring"), focused, motion::STATE);
    let margin = theme::search_field_margin();
    let frame = theme::field_frame(palette, ring, margin);
    let width = ui.available_width();
    let mut text = search.to_owned();
    let output = egui::TextEdit::singleline(&mut text)
        .id(id)
        .frame(frame)
        .desired_width(width)
        .min_size(egui::vec2(0.0, theme::SEARCH_HEIGHT))
        .hint_text(RichText::new("Search chats").color(palette.text3))
        .prefix(egui::Atom::custom(
            egui::IdSalt::new("search-icon"),
            egui::vec2(SEARCH_ICON, SEARCH_ICON),
        ))
        .show(ui);
    let rect = output.response.rect;
    let center = egui::pos2(
        rect.min.x + f32::from(margin.left) + SEARCH_ICON * 0.5,
        rect.center().y,
    );
    icons::search(ui.painter(), center, SEARCH_ICON, palette.text3);
    output.response.changed().then_some(text)
}

/// A quiet full-width button for the foot of the sidebar: a painted icon, then
/// the label at the left. It has no fill until the pointer is on it.
fn rail_button(ui: &mut egui::Ui, label: &str, icon: Icon) -> egui::Response {
    let palette = theme::palette(ui);
    let icon_id = egui::IdSalt::new(("rail-icon", label));
    let button = egui::Button::new((
        egui::Atom::custom(icon_id, egui::vec2(RAIL_ICON, RAIL_ICON)),
        RichText::new(label)
            .color(palette.text2)
            .atom_grow(true)
            .atom_align(egui::Align2::LEFT_CENTER),
    ))
    .frame_when_inactive(false)
    .corner_radius(egui::CornerRadius::same(radius::ROW))
    .min_size(egui::vec2(ui.available_width(), theme::MIN_TARGET + 4.0));
    let shown = button.atom_ui(ui);
    if let Some(rect) = shown.rect(icon_id) {
        icon(ui.painter(), rect.center(), RAIL_ICON, palette.text2);
    }
    shown.response
}

/// The foot of the sidebar: Add account, a Slack workspace, and Settings.
fn rail_footer(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    egui::Panel::bottom("accounts-footer")
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::new().inner_margin(egui::Margin {
            top: space::S as i8,
            ..egui::Margin::ZERO
        }))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = ROW_GAP;
            if snapshot.can_add_account() && rail_button(ui, "Add account", icons::plus).clicked() {
                out.push(Intent::Telegram(TelegramIntent::AddAccount));
            }
            add_slack_workspace(ui, snapshot, out);
            let settings = rail_button(ui, "Settings", icons::gear);
            egui::Popup::menu(&settings)
                .align(egui::RectAlign::TOP_START)
                .gap(space::XS)
                .show(|ui| settings_menu(ui, snapshot, out));
        });
}

/// What the Settings button opens.
fn settings_menu(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    ui.set_min_width(232.0);
    ui.spacing_mut().button_padding = egui::vec2(space::S, 6.0);
    ui.spacing_mut().item_spacing.y = 4.0;
    if ui.button("Refresh").clicked() {
        out.push(Intent::Refresh);
        ui.close();
    }
    if snapshot.can_add_account() && ui.button("Advanced").clicked() {
        out.push(Intent::Telegram(TelegramIntent::OpenApiOverride));
        ui.close();
    }
    ui.separator();
    theme_control(ui, snapshot.theme(), out);
    ui.separator();
    notification_control(ui, snapshot, out);
}

/// One caption over a group of settings.
fn menu_caption(ui: &mut egui::Ui, text: &str) {
    ui.add_space(space::XS);
    ui.label(
        RichText::new(text)
            .small()
            .family(theme::semibold())
            .color(theme::palette(ui).text3),
    );
}

fn theme_control(ui: &mut egui::Ui, current: ThemeMode, out: &mut Vec<Intent>) {
    menu_caption(ui, "Theme");
    ui.horizontal(|ui| {
        for (preference, label) in [
            (egui::ThemePreference::System, "System"),
            (egui::ThemePreference::Light, "Light"),
            (egui::ThemePreference::Dark, "Dark"),
        ] {
            let selected = current.to_egui() == preference;
            if ui.selectable_label(selected, label).clicked() && !selected {
                let chosen = ThemeMode::from_egui(preference);
                out.push(Intent::SetTheme(chosen));
                super::theme_mode::apply(ui.ctx(), chosen);
            }
        }
    });
}

/// Settings switches for desktop notifications (#32).
fn notification_control(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    menu_caption(ui, "Notifications");
    let mut on = snapshot.notifications();
    if ui.checkbox(&mut on, "Show notifications").changed() {
        out.push(Intent::SetNotifications(on));
    }
    let mut hide = !snapshot.notification_preview();
    if ui
        .add_enabled(on, egui::Checkbox::new(&mut hide, "Hide message text"))
        .changed()
    {
        out.push(Intent::SetNotificationPreview(!hide));
    }
}

fn status_strip(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    let content = StripContent::of(snapshot, keychain_notice(snapshot.persistence()));
    let visible = !content.is_empty();
    // While the strip slides away, it draws the last content it had.
    let last = egui::Id::unique("status-strip-content");
    let shown = if visible {
        ui.data_mut(|data| data.insert_temp(last, content.clone()));
        content
    } else {
        ui.data(|data| data.get_temp::<StripContent>(last))
            .unwrap_or_default()
    };
    let palette = theme::palette(ui);
    let fill = motion::mix(palette.bg, shown.tone().ink(palette), 0.10);
    let mut open = visible;
    let mut dismiss = false;
    // Idle chrome with no notice and no error draws no panel, so the strip is 0 px.
    egui::Panel::top("status")
        .resizable(false)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(fill)
                .inner_margin(egui::Margin::symmetric(space::L as i8, space::S as i8)),
        )
        .show_collapsible(ui, &mut open, |ui| shown.draw(ui, &mut dismiss));
    if dismiss && visible {
        out.push(Intent::DismissError);
    }
}

/// How serious the strip is. It picks the tint of the banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    Info,
    Warn,
    Error,
}

impl Tone {
    fn ink(self, palette: &theme::Palette) -> egui::Color32 {
        match self {
            Self::Info => palette.accent,
            Self::Warn => palette.warn,
            Self::Error => palette.error,
        }
    }
}

/// One line of the strip: a colored dot for the tone, then the text.
fn banner_line(ui: &mut egui::Ui, tone: Tone, text: &str, color: egui::Color32) {
    let palette = theme::palette(ui);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = space::S;
        let (dot, _) = ui.allocate_exact_size(egui::vec2(8.0, 14.0), egui::Sense::hover());
        ui.painter()
            .circle_filled(dot.center(), 3.5, tone.ink(palette));
        ui.label(RichText::new(text).color(color));
    });
}

/// What the status strip draws. Owned, so the strip can draw it again while
/// it slides away.
#[derive(Debug, Clone, Default)]
struct StripContent {
    /// A status line that is not a running load, and whether it is a failure.
    line: Option<(String, bool)>,
    notice: Option<&'static str>,
    /// A protocol note (`AdapterEvent::Notice`) for the selected protocol.
    /// Information, not an error.
    note: Option<String>,
    error: Option<thinwire_core::state::UserError>,
}

impl StripContent {
    fn of(snapshot: &Snapshot, notice: Option<&'static str>) -> Self {
        let line = snapshot.status_line();
        Self {
            line: strip_status(snapshot, &line)
                .map(|text| (text.to_owned(), load_failure_text(&line).is_some())),
            notice,
            note: snapshot
                .notice(snapshot.selected_protocol)
                .map(str::to_owned),
            error: if snapshot.auth == AuthScreen::Idle {
                snapshot.error.clone()
            } else {
                None
            },
        }
    }

    fn is_empty(&self) -> bool {
        self.line.is_none() && self.notice.is_none() && self.note.is_none() && self.error.is_none()
    }

    /// The most serious thing on the strip.
    fn tone(&self) -> Tone {
        if self.error.is_some() || self.line.as_ref().is_some_and(|(_, failed)| *failed) {
            Tone::Error
        } else if self.notice.is_some() {
            Tone::Warn
        } else {
            Tone::Info
        }
    }

    fn draw(&self, ui: &mut egui::Ui, dismiss: &mut bool) {
        let palette = theme::palette(ui);
        if let Some((text, failed)) = &self.line {
            let tone = if *failed { Tone::Error } else { Tone::Info };
            banner_line(ui, tone, text, palette.text);
        }
        if let Some(notice) = self.notice {
            banner_line(ui, Tone::Warn, notice, palette.text);
        }
        if let Some(note) = &self.note {
            banner_line(ui, Tone::Info, note, palette.text);
        }
        if let Some(error) = &self.error {
            error_block(ui, error, dismiss);
        }
    }
}

fn error_block(ui: &mut egui::Ui, error: &thinwire_core::state::UserError, dismiss: &mut bool) {
    let palette = theme::palette(ui);
    ui.spacing_mut().item_spacing.y = 2.0;
    egui::Sides::new().shrink_left().show(
        ui,
        |ui| {
            ui.label(
                RichText::new(format!("What happened: {}", error.happened))
                    .family(theme::semibold())
                    .color(palette.error),
            );
        },
        |ui| {
            if ui.button("Dismiss").clicked() {
                *dismiss = true;
            }
        },
    );
    ui.label(RichText::new(format!("Why: {}", error.why)).color(palette.text2));
    ui.label(RichText::new(format!("What to do: {}", error.next)).color(palette.text2));
}

/// A line the strip may show.
///
/// A failed chat list, history load, or read mark stays on the strip. The
/// library name and the code stay off it. Other lines that name the library
/// stay in the log only.
fn public_status(text: &str) -> Option<&str> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(shown) = load_failure_text(text) {
        return Some(shown);
    }
    if text.contains("TDLib") {
        None
    } else {
        Some(text)
    }
}

/// User-facing copy for a failed chat list, history load, or read mark.
fn load_failure_text(text: &str) -> Option<&'static str> {
    let text = text.trim();
    if text.starts_with("Could not load Telegram chats (TDLib ") {
        Some("Could not load chats.")
    } else if text.starts_with("Could not load messages (TDLib ") {
        Some("Could not load messages.")
    } else if text.starts_with("Could not mark messages read (TDLib ") {
        Some("Could not mark messages read.")
    } else {
        None
    }
}

/// Quiet lines. The strip stays hidden when the status is one of these.
fn is_idle_status(text: &str) -> bool {
    match public_status(text) {
        None => true,
        Some(text) => {
            text.starts_with("Sign in with Telegram to get started.")
                || text == "Telegram is ready. Loading the chat list."
                || text == "Recent messages loaded."
        }
    }
}

/// The status line the window shows, if any: not idle, not a quiet line,
/// and free of library names. A running load of any protocol comes first,
/// never a finished Ready line while a load runs (#80).
fn shown_status<'a>(snapshot: &Snapshot, line: &'a str) -> Option<&'a str> {
    if snapshot.status_is_idle() {
        return None;
    }
    public_status(line).filter(|text| !is_idle_status(text))
}

/// A running load, a send, or a refresh. The chat list header shows it beside
/// the title. A failed load is not busy: it stays on the strip.
fn is_busy_status(snapshot: &Snapshot, line: &str, text: &str) -> bool {
    load_failure_text(line).is_none() && (snapshot.status_line_loads() || text == "Refreshing…")
}

/// The status line on the strip: shown, and not busy.
fn strip_status<'a>(snapshot: &Snapshot, line: &'a str) -> Option<&'a str> {
    shown_status(snapshot, line).filter(|text| !is_busy_status(snapshot, line, text))
}

/// The busy line beside the menu, if a load, a send, or a refresh runs.
pub(super) fn busy_line(snapshot: &Snapshot) -> Option<String> {
    let line = snapshot.status_line();
    shown_status(snapshot, &line)
        .filter(|text| is_busy_status(snapshot, &line, text))
        .map(str::to_owned)
}

/// True when the status strip draws a panel.
#[cfg(test)]
pub(super) fn status_strip_visible(snapshot: &Snapshot, notice: Option<&'static str>) -> bool {
    !StripContent::of(snapshot, notice).is_empty()
}

/// Left sidebar: the mark, one row per account, then the foot with Add
/// account and Settings. The chat list is the next column.
fn account_rail(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    let palette = theme::palette(ui);
    egui::Panel::left("accounts")
        .resizable(true)
        .default_size(if narrow_window(ui) {
            RAIL_WIDTH_NARROW
        } else {
            RAIL_WIDTH
        })
        .size_range(RAIL_WIDTH_NARROW..=256.0)
        .show_separator_line(false)
        .frame(theme::rail_frame(palette))
        .show(ui, |ui| {
            rail_footer(ui, snapshot, out);
            brand(ui);
            egui::ScrollArea::vertical()
                .id_salt("accounts-scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = ROW_GAP;
                    account_rows(ui, snapshot, out);
                });
        });
}

/// The mark and the name at the top of the sidebar.
fn brand(ui: &mut egui::Ui) {
    let palette = theme::palette(ui);
    ui.horizontal(|ui| {
        ui.add_space(theme::ROW_INSET.x);
        let (mark, _) = ui.allocate_exact_size(egui::vec2(26.0, 26.0), egui::Sense::hover());
        ui.painter()
            .rect_filled(mark, egui::CornerRadius::same(8), palette.accent);
        ui.painter().text(
            mark.center(),
            egui::Align2::CENTER_CENTER,
            "t",
            egui::FontId::new(size::BODY, theme::semibold()),
            palette.on_accent,
        );
        ui.add_space(space::XS);
        ui.label(
            RichText::new("thinwire")
                .text_style(egui::TextStyle::Heading)
                .color(palette.text),
        );
    });
    ui.add_space(space::S);
}

fn account_rows(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    let mut clicked: Option<ProtocolId> = None;
    for account in snapshot.accounts.iter() {
        if !snapshot.shows_in_switcher(account.caps.id) {
            continue;
        }
        // The old rows are hidden. The line under the list says why.
        if account.signal_retained.is_some() && snapshot.signal_history_kept_line().is_some() {
            continue;
        }
        let unread = snapshot.account_row_unread(account);
        if account_chip(
            ui,
            snapshot,
            account,
            snapshot.account_row_selected(account),
            unread,
            out,
        ) {
            if let Some(slot) = account.signal_retained {
                out.push(Intent::SelectRetainedSignal(slot));
            } else {
                clicked = Some(account.caps.id);
            }
        }
        store_protection_line(ui, account, out);
        helper_actions(ui, account, out);
        removed_signal_link(ui, snapshot, account, out);
    }
    if let Some(protocol) = clicked {
        out.push(Intent::SelectProtocol(protocol));
    }
    // The old rows are hidden. The line under the list says why.
    if let Some(line) = snapshot.signal_history_kept_line() {
        let palette = theme::palette(ui);
        ui.add_space(space::XS);
        ui.horizontal_wrapped(|ui| {
            ui.add_space(theme::ROW_INSET.x);
            ui.label(RichText::new(line).small().color(palette.text2));
        });
    }
}

/// Middle column: the selected account's chats, at the full height of the window.
fn chat_column(ui: &mut egui::Ui, snapshot: &View<'_>, hints: &mut Hints, out: &mut Vec<Intent>) {
    let palette = theme::palette(ui);
    egui::Panel::left("chats")
        .resizable(true)
        .default_size(if narrow_window(ui) {
            LIST_WIDTH_NARROW
        } else {
            LIST_WIDTH
        })
        .size_range(LIST_WIDTH_NARROW..=480.0)
        .frame(theme::list_frame(palette))
        .show(ui, |ui| {
            account_info(ui, snapshot, out);
            link_offer(ui, snapshot, out);
            let inbox_list_id = ui.make_persistent_id("inbox");
            inbox_scroll(ui, snapshot, hints, inbox_list_id, out);
        });
}

/// Header of the chat list: the account name, its risk notice, the busy
/// state, and the search field. Ban risk opens the same README text as Unofficial.
fn account_info(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    let protocol = snapshot.selected_protocol;
    let palette = theme::palette(ui);
    ui.horizontal(|ui| {
        ui.add_space(theme::ROW_INSET.x);
        ui.label(
            RichText::new(protocol.display_name())
                .text_style(egui::TextStyle::Heading)
                .color(palette.text),
        );
        if let Some(label) = notice_label(protocol)
            && ui
                .add(
                    egui::Button::new(RichText::new(label).small().color(palette.text2))
                        .corner_radius(egui::CornerRadius::same(radius::PILL))
                        .min_size(egui::vec2(0.0, 24.0)),
                )
                .clicked()
        {
            out.push(Intent::ShowNotice(protocol));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(theme::ROW_INSET.x);
            busy_indicator(ui, busy_line(snapshot).as_deref());
        });
    });
    ui.add_space(space::XS);
    if let Some(text) = search_field(ui, &snapshot.search) {
        out.push(Intent::SetSearch(text.into()));
    }
    ui.add_space(space::S);
}

/// Link starts here for an unlinked unofficial account. The notice is the next step.
fn link_offer(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    #[cfg(feature = "whatsapp-web")]
    super::whatsapp_gate::link_offer(ui, snapshot, out);
    #[cfg(feature = "signal-local")]
    super::signal_gate::link_offer(ui, snapshot, out);
    #[cfg(not(any(feature = "whatsapp-web", feature = "signal-local")))]
    let _ = (ui, snapshot, out);
}

/// Read-only README text. Closing it does not start or cancel a link.
fn notice_review(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    let Some(protocol) = snapshot.notice_review() else {
        return;
    };
    let Some(lines) = unofficial_notice(protocol) else {
        return;
    };
    let Some(title) = notice_label(protocol) else {
        return;
    };
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ui.ctx(), |ui| {
            ui.set_max_width(480.0);
            for line in lines {
                ui.label(*line);
                ui.add_space(6.0);
            }
            if ui.button("Close").clicked() {
                out.push(Intent::CloseNotice);
            }
        });
}

/// One account of the sidebar: the protocol tile with its status dot, the
/// name, one status line, and the unread count. Returns true on a click.
fn account_chip(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    account: &AccountRow,
    selected: bool,
    unread: u32,
    out: &mut Vec<Intent>,
) -> bool {
    let caps = account.caps;
    let palette = theme::palette(ui);
    let width = ui.available_width();
    let label = chip_label(snapshot, account);
    let unofficial = account.linked() && unofficial_notice(caps.id).is_some();
    let badge = badge_text(unread);
    let badge_room = badge
        .as_deref()
        .map_or(0.0, |text| unread_badge_width(ui, text) + space::S);
    // The column keeps the name and the status off the tile and the badge.
    let text_width = (width - theme::ROW_INSET.x * 2.0 - TILE - space::M - badge_room).max(48.0);
    // A long sentence (a removed Signal device, a stopped helper) wraps, so
    // the row is as tall as that wrapped text.
    let status = ui.painter().layout(
        label.clone(),
        egui::FontId::new(size::CAPTION, egui::FontFamily::Proportional),
        palette.text3,
        text_width,
    );
    let name_height = ui.text_style_height(&theme::row_title());
    let content = TILE.max(name_height + 2.0 + status.size().y);
    let (rect, hover) = ui.allocate_exact_size(
        egui::vec2(width, content + theme::ROW_INSET.y * 2.0),
        egui::Sense::hover(),
    );
    let chip_id = ui
        .scope_id()
        .with(("account-chip", caps.id, account.signal_retained));
    // The row click is under the Unofficial button, so that button takes its own click.
    let response = ui
        .interact(rect, chip_id, egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let fill = row_backdrop(ui, chip_id, rect, palette.rail, selected, hover.hovered());
    let inner = theme::row_content_rect(rect, 0.0);
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::left_to_right(egui::Align::Center))
            .id_salt(("account-chip", caps.id, account.signal_retained)),
    );
    child.spacing_mut().item_spacing.x = space::M;
    // A selectable label takes the click, so only the tile selected the
    // account. Plain labels let the click go to the row.
    child.style_mut().interaction.selectable_labels = false;
    let (tile, tile_response) =
        child.allocate_exact_size(egui::vec2(TILE, TILE), egui::Sense::hover());
    // The tile is painted, so tests need a label to read its left edge.
    #[cfg(test)]
    tile_response
        .widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, "account status"));
    #[cfg(not(test))]
    let _ = tile_response;
    let dot = if account.status == AdapterStatus::Ready {
        palette.ok
    } else {
        palette.text3
    };
    paint_protocol_tile(&child, tile, caps.id, Some((fill, dot)));
    // The name keeps its full width. The Unofficial pill goes next to the
    // name only when both fit. Else it goes on the status line.
    let pill_on_name = unofficial && {
        let name_font = theme::row_title().resolve(child.style());
        let name = child
            .painter()
            .layout_no_wrap(caps.id.display_name().to_owned(), name_font, palette.text)
            .size()
            .x;
        name + space::S + unofficial_pill_width(&child) <= text_width
    };
    let mut unofficial_hit = false;
    let mut pill = |ui: &mut egui::Ui| {
        ui.spacing_mut().button_padding = egui::vec2(space::S, 1.0);
        let button = ui.add(
            egui::Button::new(RichText::new("Unofficial").small().color(palette.text2))
                .corner_radius(egui::CornerRadius::same(radius::PILL)),
        );
        let pointer = ui.input(|input| input.pointer.interact_pos());
        unofficial_hit = button.clicked()
            || response.clicked() && pointer.is_some_and(|pos| button.rect.contains(pos));
        if unofficial_hit {
            out.push(Intent::ShowNotice(caps.id));
        }
    };
    child.vertical(|ui| {
        ui.set_max_width(text_width);
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.spacing_mut().interact_size.y = 0.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = space::S;
            ui.add(
                egui::Label::new(
                    RichText::new(caps.id.display_name())
                        .text_style(theme::row_title())
                        .color(palette.text),
                )
                .truncate(),
            );
            if pill_on_name {
                pill(ui);
            }
        });
        let color = match account.status {
            AdapterStatus::Ready => palette.ok,
            AdapterStatus::Error | AdapterStatus::Refused => palette.warn,
            _ => palette.text3,
        };
        if unofficial && !pill_on_name {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = space::S;
                ui.label(RichText::new(label).small().color(color));
                pill(ui);
            });
        } else {
            ui.label(RichText::new(label).small().color(color));
        }
    });
    if let Some(text) = &badge {
        child.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            unread_badge(ui, text, false);
        });
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::Role::Button, true, selected, caps.id.display_name())
    });
    if let Some(note) = account_footnote(caps.id, account.linked()) {
        ui.label(RichText::new(note).small().weak());
    }
    response.clicked() && !unofficial_hit
}

/// Width of the Unofficial pill of an account row, with its padding.
fn unofficial_pill_width(ui: &egui::Ui) -> f32 {
    let font = egui::TextStyle::Small.resolve(ui.style());
    ui.painter()
        .layout_no_wrap("Unofficial".to_owned(), font, egui::Color32::PLACEHOLDER)
        .size()
        .x
        + space::S * 2.0
}

/// The protocol tile: a tinted square with the protocol mark. `dot` is the
/// row fill and the account state: the dot is cut out of the corner in the
/// row's own fill.
pub(super) fn paint_protocol_tile(
    ui: &egui::Ui,
    tile: egui::Rect,
    protocol: ProtocolId,
    dot: Option<(egui::Color32, egui::Color32)>,
) {
    let ink = theme::avatar_ink(theme::protocol_tile(protocol), ui.visuals().dark_mode);
    let corner = (tile.width() * 0.28).round().clamp(4.0, 24.0) as u8;
    ui.painter()
        .rect_filled(tile, egui::CornerRadius::same(corner), ink.fill);
    super::icons::protocol(
        ui.painter(),
        tile.center(),
        tile.width() * 0.58,
        protocol,
        ink.text,
    );
    if let Some((fill, color)) = dot {
        let at = tile.right_bottom() - egui::vec2(3.0, 3.0);
        ui.painter().circle_filled(at, 6.5, fill);
        ui.painter().circle_filled(at, 4.5, color);
    }
}

/// Link again after the phone removed this Signal device. The button opens
/// the normal QR link flow. The line above it is the only copy on the row.
#[cfg(feature = "signal-local")]
fn removed_signal_link(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    account: &AccountRow,
    out: &mut Vec<Intent>,
) {
    if account.signal_retained.is_some()
        || account.caps.id != ProtocolId::Signal
        || !snapshot.signal_device_removed()
    {
        return;
    }
    ui.horizontal(|ui| {
        ui.add_space(theme::ROW_INSET.x);
        if ui.button("Link").clicked() {
            out.push(Intent::Signal(SignalIntent::Relink));
        }
    });
}

#[cfg(not(feature = "signal-local"))]
fn removed_signal_link(
    _ui: &mut egui::Ui,
    _snapshot: &Snapshot,
    _account: &AccountRow,
    _out: &mut Vec<Intent>,
) {
}

/// The keychain or encryption sentence, under the account row. The status
/// line of the row stays short.
///
/// A denied keychain read shows Retry. Retry reads the key again
/// (`RetryStoreKey`). A failed move shows Retry too. That Retry opens the
/// saved session and, on a wrong key, tries the move again. Nothing is
/// deleted. A wrong key that already moved shows Link. Link starts the same
/// flow as a removed Signal device, and the WhatsApp risk gate.
fn store_protection_line(ui: &mut egui::Ui, account: &AccountRow, out: &mut Vec<Intent>) {
    if account.status != AdapterStatus::Error && account.store_access.is_none() {
        return;
    }
    if !thinwire_protocol::is_store_protection_detail(&account.detail) {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.add_space(theme::ROW_INSET.x);
        ui.label(
            RichText::new(&account.detail)
                .small()
                .color(theme::palette(ui).warn),
        );
        if let Some(intent) = store_row_intent(account)
            && ui
                .small_button(store_row_button(account))
                .on_hover_text(store_row_hover(account))
                .clicked()
        {
            out.push(intent);
        }
    });
}

/// Retry reads the key again. A failed move uses the same command to try
/// the move again. Link starts the existing link flow.
fn store_row_intent(account: &AccountRow) -> Option<Intent> {
    match account.store_access {
        Some(StoreAccessReason::KeychainDenied | StoreAccessReason::MoveFailed) => {
            Some(Intent::RetryStoreKey(account.caps.id))
        }
        Some(StoreAccessReason::WrongKey) if account.caps.id == ProtocolId::Signal => {
            Some(Intent::Signal(SignalIntent::Relink))
        }
        Some(StoreAccessReason::WrongKey) if account.caps.id == ProtocolId::WhatsApp => {
            Some(Intent::WhatsApp(WhatsAppIntent::OpenRiskGate))
        }
        Some(StoreAccessReason::WrongKey) | None => None,
    }
}

fn store_row_button(account: &AccountRow) -> &'static str {
    match account.store_access {
        Some(StoreAccessReason::KeychainDenied | StoreAccessReason::MoveFailed) => "Retry",
        Some(StoreAccessReason::WrongKey) => "Link",
        None => "",
    }
}

fn store_row_hover(account: &AccountRow) -> &'static str {
    match account.store_access {
        Some(StoreAccessReason::KeychainDenied) => "Read the session key from Keychain again",
        Some(StoreAccessReason::MoveFailed) => {
            "Tries to move the saved session aside again. Nothing is deleted."
        }
        Some(StoreAccessReason::WrongKey) => "Link again. The old data stays.",
        None => "",
    }
}

/// Under the row of a protocol that runs in a helper process (ADR 0013). A
/// stopped helper gets Restart: with the status line it reads "Helper
/// stopped. Restart". A missing helper gets the full reason.
fn helper_actions(ui: &mut egui::Ui, account: &AccountRow, out: &mut Vec<Intent>) {
    let protocol = account.caps.id;
    if account.helper_stopped() {
        ui.horizontal(|ui| {
            ui.add_space(theme::ROW_INSET.x);
            let restart = ui.small_button("Restart").on_hover_text(format!(
                "Start the {} helper again",
                protocol.display_name()
            ));
            if restart.clicked() {
                out.push(Intent::RestartHelper(protocol));
            }
        });
    } else if account.helper_missing() {
        ui.horizontal_wrapped(|ui| {
            ui.add_space(theme::ROW_INSET.x);
            ui.label(
                RichText::new(account.helper_missing_text())
                    .small()
                    .color(theme::palette(ui).warn),
            );
        });
    }
}

/// The chat-list time of `at`, in the zone of the view's clock (#120).
fn view_list_time(at: i64, now: ViewNow) -> String {
    match now {
        ViewNow::Local(now) => list_time(at, &now),
        ViewNow::Fixed(now) => list_time(at, &now),
    }
}

/// Rows last frame, and the row the pointer was on.
///
/// At the top of the list a new chat appears there and the rows below it
/// shift down. Further down, the next frame keeps the anchor row on the
/// same screen line so a click still hits that chat.
#[derive(Clone)]
struct InboxAnchor {
    ids: Vec<String>,
    offset: f32,
    stride: f32,
    anchor: Option<String>,
}

/// What one inbox frame painted, for the next frame's anchor.
struct DrawnInbox {
    anchor: Option<String>,
    stride: f32,
}

/// An offset this close to the top still counts as the top.
const INBOX_TOP: f32 = 1.0;

/// Scroll offset that keeps `previous.anchor` on the same screen line.
/// `None` at the top, or when that row did not move.
fn anchored_inbox_offset(previous: &InboxAnchor, ids: &[String]) -> Option<f32> {
    if previous.offset <= INBOX_TOP || previous.stride <= 0.0 {
        return None;
    }
    let anchor = previous.anchor.as_deref()?;
    let before = previous.ids.iter().position(|id| id == anchor)?;
    let after = ids.iter().position(|id| id == anchor)?;
    if before == after {
        return None;
    }
    Some((previous.offset + (after as f32 - before as f32) * previous.stride).max(0.0))
}

fn inbox_row_stride(ui: &egui::Ui) -> f32 {
    inbox_row_height(ui) + ROW_GAP
}

/// The inbox list. A reorder anchors the scroll offset before the rows draw.
fn inbox_scroll(
    ui: &mut egui::Ui,
    snapshot: &View<'_>,
    hints: &mut Hints,
    inbox_list_id: egui::Id,
    out: &mut Vec<Intent>,
) {
    let ids: Vec<String> = snapshot
        .visible_conversations()
        .iter()
        .map(|row| row.id.clone())
        .collect();
    let memo_id = ui.make_persistent_id("inbox-anchor");
    let previous: Option<InboxAnchor> = ui.data(|data| data.get_temp(memo_id));
    let forced = previous
        .as_ref()
        .and_then(|previous| anchored_inbox_offset(previous, &ids));
    let mut area = egui::ScrollArea::vertical()
        .id_salt("inbox")
        .auto_shrink([false, false]);
    if let Some(offset) = forced {
        area = area.vertical_scroll_offset(offset);
    }
    let mut drawn = DrawnInbox {
        anchor: None,
        stride: 0.0,
    };
    let output = area.show(ui, |ui| {
        drawn = inbox(ui, snapshot, hints, inbox_list_id, out);
    });
    ui.data_mut(|data| {
        data.insert_temp(
            memo_id,
            InboxAnchor {
                ids,
                offset: output.state.clamped_offset().y,
                stride: drawn.stride,
                anchor: drawn.anchor,
            },
        );
    });
}

fn inbox(
    ui: &mut egui::Ui,
    snapshot: &View<'_>,
    hints: &mut Hints,
    inbox_list_id: egui::Id,
    out: &mut Vec<Intent>,
) -> DrawnInbox {
    ui.spacing_mut().item_spacing.y = ROW_GAP;
    let stride = inbox_row_stride(ui);
    let now = snapshot.now();
    let rows: Vec<InboxRow> = snapshot
        .visible_conversations()
        .iter()
        .map(|row| InboxRow {
            protocol: row.protocol,
            id: row.id.clone(),
            title: row.title.clone(),
            preview: row.preview.clone(),
            unread: snapshot.display_unread(row),
            time: view_list_time(row.last_at, now),
            mute: snapshot.chat_mute(row.protocol, &row.id),
            avatar: row.avatar.clone(),
        })
        .collect();

    match snapshot.inbox_state() {
        InboxState::Rows => {}
        InboxState::Loading => {
            ui.add_space(space::L);
            ui.horizontal(|ui| {
                ui.add_space(theme::ROW_INSET.x);
                motion::spinner(ui);
                ui.label(RichText::new(LOADING_CHATS_STATUS).color(theme::palette(ui).text3));
            });
            return DrawnInbox {
                anchor: None,
                stride,
            };
        }
        InboxState::Empty => {
            if let Some(text) = snapshot.inbox_placeholder() {
                empty_note(ui, text);
            }
            return DrawnInbox {
                anchor: None,
                stride,
            };
        }
        InboxState::NoMatch => {
            let query = snapshot.search.trim();
            empty_note(ui, &format!("No chats match '{query}'."));
            return DrawnInbox {
                anchor: None,
                stride,
            };
        }
    }

    let scroll_to_selected = hints.take_scroll_to_selected();
    let scroll_to_focused = hints.take_scroll_to_focused();
    let open_mute_menus = hints.open_mute_menus;
    let widget_focus = ui.ctx().memory(|memory| memory.focused());
    let pointer = ui.input(|input| input.pointer.hover_pos());
    let clip = ui.clip_rect();
    let mut clicked: Option<String> = None;
    let mut focus_from_widget: Option<String> = None;
    let mut under_pointer = None;
    let mut first_visible = None;
    let mut menu_state_open = [false; 3];
    for row in rows {
        let InboxRow {
            protocol,
            id,
            title,
            preview,
            unread,
            time,
            mute,
            avatar,
        } = row;
        let selected = snapshot.selected_conversation.as_deref() == Some(id.as_str());
        let focused = snapshot.focused_row.as_deref() == Some(id.as_str());
        let response = inbox_row(
            ui,
            protocol,
            &id,
            &title,
            &preview,
            &time,
            unread,
            selected,
            mute.is_muted(),
            &avatar,
            out,
        );
        let force_open = open_mute_menus && claim_menu_state(&mut menu_state_open, mute);
        let add_menu = |ui: &mut egui::Ui| {
            if let Some(intent) = mute_control(ui, protocol, &id, mute, "chat") {
                out.push(intent);
                ui.close();
            }
        };
        if force_open {
            // Beside the row, not on the pointer: three states stay visible
            // in one frame. `Popup::menu` is the context menu before it
            // anchors to the pointer.
            egui::Popup::menu(&response)
                .open(true)
                .align(egui::RectAlign::RIGHT_START)
                .align_alternatives(&[])
                .gap(space::S)
                .close_behavior(egui::PopupCloseBehavior::IgnoreClicks)
                .show(add_menu);
        } else {
            response.context_menu(add_menu);
        }
        // The open chat already has its fill. The ring marks only a row the
        // arrow keys moved to, so a click does not leave an outline behind.
        if focused && !selected {
            ui.painter().rect_stroke(
                response.rect,
                egui::CornerRadius::same(radius::ROW),
                egui::Stroke::new(2.0, theme::palette(ui).text),
                egui::StrokeKind::Inside,
            );
        }
        // The arrow keys move the highlight (`inbox_keys`). Tell egui, so its
        // own focus walk does not also move the widget focus a second time.
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    vertical_arrows: true,
                    ..egui::EventFilter::default()
                },
            );
        });
        // AccessKit `selected` is the open chat. The highlight is keyboard focus.
        response.widget_info(|| {
            let label = if mute.is_muted() {
                format!("{title}, muted")
            } else {
                title.clone()
            };
            egui::WidgetInfo::selected(egui::Role::Button, true, selected, label)
        });
        if row_scrolls(selected, focused, scroll_to_selected, scroll_to_focused) {
            response.scroll_to_me(None);
        }
        if under_pointer.is_none() && pointer.is_some_and(|pos| response.rect.contains(pos)) {
            under_pointer = Some(id.clone());
        }
        if first_visible.is_none() && response.rect.intersects(clip) {
            first_visible = Some(id.clone());
        }
        if widget_focus == Some(response.id) && !focused {
            focus_from_widget = Some(id.clone());
        }
        if response.clicked() {
            clicked = Some(id);
        }
    }
    inbox_keys(ui, snapshot, inbox_list_id, out);
    let key_moved = out
        .iter()
        .any(|intent| matches!(intent, Intent::MoveInbox { .. }));
    if let Some(id) = clicked {
        out.push(Intent::SelectConversation { id });
    } else if !key_moved && let Some(id) = focus_from_widget {
        out.push(Intent::FocusInbox { id });
    }
    if let Some(Intent::MoveInbox { delta }) = out
        .iter()
        .find(|intent| matches!(intent, Intent::MoveInbox { .. }))
        && let Some(id) = snapshot.inbox_move_target(*delta)
    {
        let row_id = ui.scope_id().with(("inbox-row", id));
        ui.memory_mut(|memory| memory.request_focus(row_id));
    }
    // The pointer's row wins. With the pointer elsewhere, the first visible
    // row keeps the viewport from jumping when a chat moves to the top.
    DrawnInbox {
        anchor: under_pointer.or(first_visible),
        stride,
    }
}

/// One inbox row, read from the view before the rows are drawn.
struct InboxRow {
    protocol: ProtocolId,
    id: String,
    title: String,
    preview: String,
    unread: u32,
    time: String,
    mute: ChatMute,
    avatar: ChatAvatar,
}

/// First row of each mute state in a snapshot that shows every menu at once.
fn claim_menu_state(seen: &mut [bool; 3], mute: ChatMute) -> bool {
    let slot = match mute {
        ChatMute::None => 0,
        ChatMute::Here => 1,
        ChatMute::Protocol => 2,
    };
    if seen[slot] {
        false
    } else {
        seen[slot] = true;
        true
    }
}

/// Why a protocol mute cannot be toggled in thinwire.
fn protocol_mute_reason(name: &str) -> String {
    format!("Only {name} can unmute it")
}

/// Disabled "Muted in {protocol}" without egui's 0.5 fade (#206).
///
/// The fade blends the label into the fill. In light that drops under 4.5:1.
/// Clicks stay off and AccessKit still reports the button disabled.
fn protocol_mute_button(ui: &mut egui::Ui, title: &str) -> egui::Response {
    let ink = theme::protocol_mute_ink(theme::palette(ui));
    let previous = ui.style().visuals.disabled_alpha;
    ui.style_mut().visuals.disabled_alpha = 1.0;
    let response = ui.add_enabled(
        false,
        egui::Button::new(RichText::new(title).color(ink.title)),
    );
    ui.style_mut().visuals.disabled_alpha = previous;
    response
}

/// The mute item of a row menu (#153): Mute, Unmute, or a disabled "Muted
/// in <protocol>" when the protocol mutes it. A protocol mute wins, so only
/// the protocol can unmute it. `what` follows the verb: "chat" or "topic". The
/// menu puts the reason beside the disabled item. Returns the intent of a
/// click. The thread header has its own icon button: [`header_mute`].
fn mute_control(
    ui: &mut egui::Ui,
    protocol: ProtocolId,
    id: &str,
    mute: ChatMute,
    what: &str,
) -> Option<Intent> {
    let (verb, muted) = match mute {
        ChatMute::None => ("Mute", true),
        ChatMute::Here => ("Unmute", false),
        ChatMute::Protocol => {
            let name = protocol.display_name();
            let title = format!("Muted in {name}");
            let response = ui
                .horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = space::S;
                    let response = protocol_mute_button(ui, &title);
                    let ink = theme::protocol_mute_ink(theme::palette(ui));
                    ui.label(
                        RichText::new(protocol_mute_reason(name))
                            .small()
                            .color(ink.reason),
                    );
                    response
                })
                .inner;
            response.on_disabled_hover_text(format!("Unmute it in {name}."));
            return None;
        }
    };
    ui.button(format!("{verb} {what}"))
        .clicked()
        .then(|| Intent::SetChatMute {
            protocol,
            conversation_id: id.to_owned(),
            muted,
        })
}

/// A quiet round button with a painted icon, 36 px, with no fill until the
/// pointer is on it. `label` is its name for a screen reader.
fn icon_button(ui: &mut egui::Ui, label: &str, icon: Icon, ink: egui::Color32) -> egui::Response {
    let icon_id = egui::IdSalt::new(("icon-button", label));
    let enabled = ui.is_enabled();
    let shown = egui::Button::new(egui::Atom::custom(
        icon_id,
        egui::vec2(ICON_BUTTON_ICON, ICON_BUTTON_ICON),
    ))
    .frame_when_inactive(false)
    .corner_radius(egui::CornerRadius::same(radius::PILL))
    .min_size(egui::vec2(ICON_BUTTON, ICON_BUTTON))
    .atom_ui(ui);
    if let Some(rect) = shown.rect(icon_id) {
        icon(ui.painter(), rect.center(), ICON_BUTTON_ICON, ink);
    }
    let response = shown.response;
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, label));
    response
}

/// The mute button of the thread header: a bell, or a slashed bell once the
/// chat is muted. The name for a screen reader is the verb, or "Muted in
/// <protocol>" when the protocol mutes it. Then nothing here can unmute it.
fn header_mute(
    ui: &mut egui::Ui,
    protocol: ProtocolId,
    id: &str,
    mute: ChatMute,
    topic: bool,
) -> Option<Intent> {
    let palette = theme::palette(ui);
    let what = if topic { "topic" } else { "chat" };
    let intent = |muted| Intent::SetChatMute {
        protocol,
        conversation_id: id.to_owned(),
        muted,
    };
    match mute {
        ChatMute::None => {
            let label = if topic { "Mute topic" } else { "Mute" };
            icon_button(ui, label, icons::bell, palette.text2)
                .on_hover_text(format!("Mute {what}"))
                .clicked()
                .then(|| intent(true))
        }
        ChatMute::Here => {
            let label = if topic { "Unmute topic" } else { "Unmute" };
            icon_button(ui, label, icons::bell_off, palette.text)
                .on_hover_text(format!("Unmute {what}"))
                .clicked()
                .then(|| intent(false))
        }
        ChatMute::Protocol => {
            let name = protocol.display_name();
            let ink = theme::protocol_mute_ink(palette);
            // No 0.5 fade: it would drop the icon under 3:1 (see
            // `protocol_mute_button`).
            let previous = ui.style().visuals.disabled_alpha;
            ui.style_mut().visuals.disabled_alpha = 1.0;
            let response = ui
                .add_enabled_ui(false, |ui| {
                    icon_button(ui, &format!("Muted in {name}"), icons::bell_off, ink.title)
                })
                .inner;
            ui.style_mut().visuals.disabled_alpha = previous;
            response.on_disabled_hover_text(format!(
                "{}. Unmute it in {name}.",
                protocol_mute_reason(name)
            ));
            None
        }
    }
}

/// The highlight scroll wins when both requests exist. It is the Enter target.
fn row_scrolls(selected: bool, focused: bool, scroll_selected: bool, scroll_focused: bool) -> bool {
    if scroll_focused {
        focused
    } else {
        selected && scroll_selected
    }
}

/// Arrow keys move the highlight. Enter opens that chat.
/// Keys apply when nothing has keyboard focus, or the inbox list has it.
/// A thread control such as Retry keeps Enter. Login keeps Enter.
fn inbox_keys(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    inbox_list_id: egui::Id,
    out: &mut Vec<Intent>,
) {
    if snapshot.inbox_state() != InboxState::Rows {
        return;
    }
    if snapshot.center_view() != CenterView::Thread {
        return;
    }
    let row_ids: Vec<egui::Id> = snapshot
        .visible_conversations()
        .iter()
        .map(|row| ui.scope_id().with(("inbox-row", &row.id)))
        .collect();
    let keys_here = ui.ctx().memory(|m| m.focused()).is_none()
        || ui.ctx().memory(|memory| {
            memory
                .focused()
                .is_some_and(|id| id == inbox_list_id || row_ids.contains(&id))
        });
    if !keys_here {
        return;
    }
    let (up, down, enter) = ui.input(|input| {
        (
            input.key_pressed(egui::Key::ArrowUp),
            input.key_pressed(egui::Key::ArrowDown),
            input.key_pressed(egui::Key::Enter),
        )
    });
    let row_focused = ui
        .ctx()
        .memory(|memory| memory.focused().is_some_and(|id| row_ids.contains(&id)));
    if up {
        out.push(Intent::MoveInbox { delta: -1 });
    } else if down {
        out.push(Intent::MoveInbox { delta: 1 });
    } else if enter
        && !row_focused
        && let Some(id) = snapshot.visible_focused_row()
    {
        // A focused row already emits one click for Enter. Do not send a second one.
        out.push(Intent::SelectConversation { id });
    }
}

/// Last finite width for this key. A frame with no width yet keeps the previous one.
///
/// #61 applies the thread scroll offset on the next frame. That frame can
/// report a width of 0. A wrap at 0 draws one letter per line.
fn laid_out_width(ui: &egui::Ui, key: &str) -> f32 {
    let id = ui.scope_id().with(key);
    let raw = ui.available_width();
    if raw.is_finite() && raw >= 48.0 {
        ui.data_mut(|data| data.insert_temp(id, raw));
        raw
    } else {
        ui.data(|data| data.get_temp(id)).unwrap_or(360.0)
    }
}

fn bubble_cap(available: f32) -> f32 {
    (available * BUBBLE_WIDTH).clamp(48.0, BUBBLE_MAX)
}

/// Left widget, then the right widget at the end of the row.
fn row_ends(
    ui: &mut egui::Ui,
    left: impl FnOnce(&mut egui::Ui),
    right: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        let gap = 72.0;
        ui.scope(|ui| {
            ui.set_max_width((ui.available_width() - gap).max(24.0));
            left(ui);
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            right(ui);
        });
    });
}

/// Avatar, or the title and preview lines when those are taller, plus the
/// padding around them. The unread badge adds [`space::XS`] around its caption.
fn inbox_row_height(ui: &egui::Ui) -> f32 {
    let title = ui.text_style_height(&theme::row_title());
    let caption = ui.text_style_height(&egui::TextStyle::Small);
    let preview = ui.text_style_height(&theme::secondary());
    let badge = caption + space::XS;
    let lines = title + 2.0 + preview.max(badge);
    super::photos::AVATAR_SIDE.max(lines) + theme::ROW_INSET.y * 2.0
}

/// A quiet line in an empty list, in line with the row content.
fn empty_note(ui: &mut egui::Ui, text: &str) {
    let palette = theme::palette(ui);
    ui.add_space(space::L);
    ui.horizontal_wrapped(|ui| {
        ui.add_space(theme::ROW_INSET.x);
        ui.label(RichText::new(text).color(palette.text3));
    });
}

/// One inbox row. The whole rect is the click target, including the preview.
#[allow(clippy::too_many_arguments)]
fn inbox_row(
    ui: &mut egui::Ui,
    protocol: ProtocolId,
    id: &str,
    title: &str,
    preview: &str,
    time: &str,
    unread: u32,
    selected: bool,
    muted: bool,
    avatar: &ChatAvatar,
    out: &mut Vec<Intent>,
) -> egui::Response {
    let palette = theme::palette(ui);
    let width = laid_out_width(ui, "inbox-row-width");
    let (rect, hover) = ui.allocate_exact_size(
        egui::vec2(width, inbox_row_height(ui)),
        egui::Sense::hover(),
    );
    let row_id = ui.scope_id().with(("inbox-row", id));
    let row_fill = row_backdrop(ui, row_id, rect, palette.sidebar, selected, hover.hovered());
    let gutter = ui.spacing().scroll.bar_width;
    let inner = theme::row_content_rect(rect, gutter);
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::left_to_right(egui::Align::Center))
            .id_salt(("inbox-row", id)),
    );
    child.spacing_mut().item_spacing.x = space::M;
    let (avatar_rect, avatar_response) = child.allocate_exact_size(
        egui::vec2(super::photos::AVATAR_SIDE, super::photos::AVATAR_SIDE),
        egui::Sense::hover(),
    );
    avatar_response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::Role::Label, true, format!("avatar {title}"))
    });
    super::photos::paint_avatar(ui, avatar_rect, protocol, id, title, avatar, row_fill, out);
    child.vertical(|ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 2.0;
        // A line is as tall as its text. The row, not the line, is the target.
        ui.spacing_mut().interact_size.y = 0.0;
        inbox_row_lines(ui, title, preview, time, unread, muted, selected, palette);
    });
    ui.interact(rect, row_id, egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[allow(clippy::too_many_arguments)]
fn inbox_row_lines(
    ui: &mut egui::Ui,
    title: &str,
    preview: &str,
    time: &str,
    unread: u32,
    muted: bool,
    selected: bool,
    palette: &theme::Palette,
) {
    let title_font = if unread > 0 {
        theme::semibold()
    } else {
        theme::medium()
    };
    row_ends(
        ui,
        |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(title)
                        .text_style(theme::row_title())
                        .family(title_font)
                        .color(palette.text),
                )
                .truncate()
                .halign(egui::Align::LEFT),
            );
        },
        |ui| {
            // An unread chat shows its time in the accent. The open chat keeps
            // the quiet ink: the accent is under 4.5:1 on its fill.
            let time_ink = if unread > 0 && !muted && !selected {
                palette.accent
            } else {
                palette.text3
            };
            ui.label(RichText::new(time).small().color(time_ink));
            // A word, not a glyph: every font has it, and a screen reader
            // reads it (#153).
            if muted {
                ui.label(RichText::new("muted").small().color(palette.text3));
            }
        },
    );
    let preview_ink = if unread > 0 {
        palette.text
    } else {
        palette.text2
    };
    row_ends(
        ui,
        |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(preview)
                        .text_style(theme::secondary())
                        .color(preview_ink),
                )
                .truncate()
                .halign(egui::Align::LEFT),
            );
        },
        |ui| {
            if let Some(text) = badge_text(unread) {
                unread_badge(ui, &text, muted);
            }
        },
    );
}

/// Highlight behind a list row or an account row. Hover and selection fade,
/// so a pointer sweep over the list or a chat switch does not flash. `base`
/// is the panel under the row. Returns the fill it painted.
fn row_backdrop(
    ui: &egui::Ui,
    id: egui::Id,
    rect: egui::Rect,
    base: egui::Color32,
    selected: bool,
    hovered: bool,
) -> egui::Color32 {
    let hovered = motion::toggle(ui.ctx(), id.with("hover"), hovered, motion::HOVER);
    let selected = motion::toggle(ui.ctx(), id.with("selected"), selected, motion::STATE);
    let fill = theme::row_fill(base, theme::palette(ui), selected, hovered);
    if hovered > 0.0 || selected > 0.0 {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(radius::ROW), fill);
    }
    fill
}

fn unread_badge(ui: &mut egui::Ui, text: &str, muted: bool) {
    let palette = theme::palette(ui);
    let ink = if muted {
        palette.text2
    } else {
        palette.on_badge
    };
    let galley = badge_galley(ui, text, ink);
    let width = badge_width(galley.size().x);
    let height = galley.size().y + space::XS;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::Role::Label, true, format!("unread {text}"))
    });
    let corner = egui::CornerRadius::same(radius::PILL);
    if muted {
        ui.painter().rect_stroke(
            rect,
            corner,
            egui::Stroke::new(1.0, theme::muted_badge_ring(palette)),
            egui::StrokeKind::Inside,
        );
    } else {
        ui.painter().rect_filled(rect, corner, palette.badge);
    }
    let pos = egui::pos2(
        rect.center().x - galley.size().x * 0.5,
        rect.center().y - galley.size().y * 0.5,
    );
    ui.painter().galley(pos, galley, ink);
}

fn badge_galley(ui: &egui::Ui, text: &str, ink: egui::Color32) -> std::sync::Arc<egui::Galley> {
    let font = egui::FontId::new(size::CAPTION, theme::semibold());
    ui.painter().layout_no_wrap(text.to_owned(), font, ink)
}

/// Pill width for a text of `text_width`: a circle for one digit.
fn badge_width(text_width: f32) -> f32 {
    (text_width + space::S).max(20.0)
}

/// Width [`unread_badge`] takes for `text`.
fn unread_badge_width(ui: &egui::Ui, text: &str) -> f32 {
    badge_width(badge_galley(ui, text, egui::Color32::PLACEHOLDER).size().x)
}

/// Pill text for an unread count. `None` when there is nothing to show.
#[must_use]
fn badge_text(unread: u32) -> Option<String> {
    match unread {
        0 => None,
        1..=99 => Some(unread.to_string()),
        _ => Some("99+".to_owned()),
    }
}

/// Line under an account chip for a protocol that has no login in this build.
/// A linked account has no "not ready" note. WhatsApp and Signal use the
/// Unofficial label and the Ban risk link instead of a permanent risk line.
#[must_use]
fn account_footnote(id: ProtocolId, linked: bool) -> Option<&'static str> {
    let slack_sign_in = id == ProtocolId::Slack && cfg!(feature = "slack-oauth");
    let has_link_ui = (id == ProtocolId::WhatsApp && cfg!(feature = "whatsapp-web"))
        || (id == ProtocolId::Signal && cfg!(feature = "signal-local"));
    if linked || id == ProtocolId::Telegram || slack_sign_in || has_link_ui {
        None
    } else {
        Some("not ready — no login UI this beat")
    }
}

/// The status line of an account row. A compiled client reports Connecting
/// at launch. That is "Not signed in" until a login or a resume starts.
///
/// One previous Signal account uses the reason sentence. Two or more use
/// "Previous account N". N is the slot. It is not a phone number.
#[must_use]
fn chip_label(snapshot: &Snapshot, account: &AccountRow) -> String {
    // The check is the first unauthorized open. Removal, and its Link
    // button, wait for the second one.
    if let Some(slot) = account.signal_retained {
        return snapshot.signal_retained_chip_label(slot);
    }
    if account.caps.id == ProtocolId::Signal && snapshot.signal_link_checking() {
        return SIGNAL_LINK_CHECK.to_owned();
    }
    if account.caps.id == ProtocolId::Signal && snapshot.signal_device_removed() {
        return SIGNAL_DEVICE_REMOVED.to_owned();
    }
    // The helper state wins: with no helper, the adapter status is old.
    if let Some(label) = account.helper_label() {
        return label.to_owned();
    }
    // A denied read or a wrong key has its own sentence under the chip.
    if account.store_access.is_some() {
        return "Connection problem".to_owned();
    }
    // A store migration runs while the account is Linking. The row stays
    // on Connecting until that work finishes.
    if account.state == AccountState::Linking
        && !matches!(
            account.status,
            AdapterStatus::Error | AdapterStatus::Refused | AdapterStatus::Ready
        )
    {
        return "Connecting…".to_owned();
    }
    let session_moving = snapshot.auth != AuthScreen::Idle
        || matches!(snapshot.center_view(), CenterView::Resuming { .. });
    if account.caps.id == ProtocolId::Telegram
        && account.status == AdapterStatus::Connecting
        && !snapshot.telegram_ready()
        && !session_moving
    {
        "Not signed in".to_owned()
    } else if account.linked()
        && matches!(
            account.status,
            AdapterStatus::Error | AdapterStatus::Refused
        )
    {
        // Still signed in: an error status never unlinks (shell plan 11).
        "Connection problem".to_owned()
    } else {
        account_label(account.status).to_owned()
    }
}

/// Short account state for the status line of an account row.
#[must_use]
fn account_label(status: AdapterStatus) -> &'static str {
    match status {
        AdapterStatus::Ready => "Online",
        AdapterStatus::Connecting => "Connecting…",
        AdapterStatus::Stubbed | AdapterStatus::Refused | AdapterStatus::Error => "Not signed in",
    }
}

fn center_panel(ui: &mut egui::Ui, snapshot: &View<'_>, hints: &mut Hints, out: &mut Vec<Intent>) {
    let fill = theme::palette(ui).bg;
    // The thread runs edge to edge: its header and compose bar own their
    // padding. The other views sit in a padded frame below.
    let frame = egui::Frame::central_panel(ui.style())
        .fill(fill)
        .inner_margin(0.0);
    egui::CentralPanel::default().frame(frame).show(ui, |ui| {
        // Keys first, before a text field can take Enter. One press, one action.
        let (enter, escape) = ui.input(|input| {
            (
                input.key_pressed(egui::Key::Enter),
                input.key_pressed(egui::Key::Escape),
            )
        });
        if escape {
            let closed =
                super::photos::take_escape(ui.ctx()) || super::links::take_escape(ui.ctx());
            if !closed {
                out.push(Intent::Key(AuthKey::Escape));
            }
        } else if enter && !super::links::take_enter(ui.ctx()) {
            out.push(Intent::Key(AuthKey::Enter));
        }
        // Another view or another login step fades in. A chat switch keeps
        // the view: the thread fades its own messages.
        let view = snapshot.center_view();
        ui.multiply_opacity(motion::fade_in_on_change(
            ui.ctx(),
            ui.scope_id().with("center-fade"),
            egui::Id::unique((
                std::mem::discriminant(&view),
                std::mem::discriminant(&snapshot.auth),
            )),
            motion::FADE,
        ));
        if view == CenterView::Thread {
            thread(ui, snapshot, hints, out);
            return;
        }
        // A card can be taller than a short window: a long error, or a notice
        // that wraps. It scrolls then, so Cancel and Change number stay in
        // reach. `room` is the height that is on screen. Inside a scroll area
        // `available_height` is not, so each view gets it from here to center
        // itself, and a card that does not fit starts at the top.
        let room = (ui.available_height() - space::L * 2.0).max(0.0);
        egui::ScrollArea::vertical()
            .id_salt("center-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::new()
                    .inner_margin(egui::Margin::same(space::L as i8))
                    .show(ui, |ui| match view {
                        CenterView::Auth => auth::draw(ui, snapshot, room, out),
                        CenterView::Resuming { connecting } => {
                            resuming(ui, snapshot, room, connecting);
                        }
                        CenterView::FirstRun => first_run(ui, snapshot, room, out),
                        CenterView::KeychainFailed => keychain_failed(ui, room, out),
                        CenterView::Thread => {}
                    });
            });
    });
}

fn resuming(ui: &mut egui::Ui, snapshot: &Snapshot, room: f32, connecting: bool) {
    let palette = theme::palette(ui);
    let top = (room * 0.30).clamp(24.0, 160.0);
    ui.add_space(top);
    ui.vertical_centered(|ui| {
        motion::spinner(ui);
        ui.add_space(space::M);
        let text = if connecting {
            RESUME_CONNECTING
        } else {
            snapshot.keychain_wait_text(Instant::now())
        };
        ui.label(RichText::new(text).color(palette.text2));
    });
}

fn keychain_failed(ui: &mut egui::Ui, room: f32, out: &mut Vec<Intent>) {
    let top = (room * 0.25).clamp(24.0, 140.0);
    ui.add_space(top);
    ui.vertical_centered(|ui| {
        ui.set_max_width(theme::LOGIN_CARD_WIDTH);
        note_box(ui, theme::palette(ui).warn, KEYCHAIN_READ_FAILED);
        ui.add_space(space::M);
        if ui.button("Try again").clicked() {
            out.push(Intent::RetryKeychain);
        }
    });
}

/// A short message in a rounded box tinted with `ink`. The text stays in the
/// primary color, so the tint never has to carry 4.5:1 contrast on its own.
pub(super) fn note_box(ui: &mut egui::Ui, ink: egui::Color32, text: &str) {
    let palette = theme::palette(ui);
    let fill = motion::mix(palette.bg, ink, 0.12);
    egui::Frame::new()
        .fill(fill)
        .corner_radius(egui::CornerRadius::same(radius::ROW))
        .inner_margin(egui::Margin::symmetric(space::M as i8, space::S as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(text).color(palette.text));
        });
}

fn first_run(ui: &mut egui::Ui, snapshot: &View<'_>, room: f32, out: &mut Vec<Intent>) {
    let id = ui.scope_id().with("first-run-card-height");
    let known = ui.data(|data| data.get_temp::<f32>(id).unwrap_or(0.0));
    let spare = (room - known).max(0.0);
    ui.add_space(spare / 2.0);
    let response = ui
        .scope(|ui| {
            theme::show_centered_card(ui, |ui| {
                let palette = theme::palette(ui);
                let (tile, _) =
                    ui.allocate_exact_size(egui::vec2(52.0, 52.0), egui::Sense::hover());
                paint_protocol_tile(ui, tile, ProtocolId::Telegram, None);
                ui.add_space(space::M);
                ui.label(
                    RichText::new("Start with Telegram")
                        .text_style(theme::display())
                        .color(palette.text),
                );
                ui.add_space(space::S);
                if snapshot.has_api_credentials() {
                    ui.label(
                        RichText::new(
                            "Sign in with your phone number, then the login code, then optional 2FA.",
                        )
                        .text_style(theme::secondary())
                        .color(palette.text2),
                    );
                } else {
                    ui.label(
                        RichText::new(
                            "This build has no Telegram credentials. Open Advanced to set a keychain override.",
                        )
                        .text_style(theme::secondary())
                        .color(palette.text2),
                    );
                }
                if let Some(banner) = super::auth::stub_banner(snapshot) {
                    ui.add_space(space::M);
                    note_box(ui, palette.warn, banner);
                }
                ui.add_space(space::L);
                if ui
                    .add(
                        egui::Button::new(RichText::new("Add Telegram").color(palette.on_accent))
                            .fill(palette.accent)
                            .corner_radius(egui::CornerRadius::same(radius::ROW))
                            .min_size(egui::vec2(ui.available_width(), 44.0)),
                    )
                    .clicked()
                {
                    out.push(Intent::Telegram(TelegramIntent::AddAccount));
                }
                add_slack_workspace(ui, snapshot, out);
            });
        })
        .response;
    ui.data_mut(|data| data.insert_temp(id, response.rect.height()));
}

/// Slack sign-in. Compiled only with `slack-oauth`. The empty twin keeps
/// the call sites in the default build.
#[cfg(feature = "slack-oauth")]
fn add_slack_workspace(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    let slack = snapshot
        .accounts
        .iter()
        .find(|row| row.caps.id == ProtocolId::Slack);
    if slack.is_some_and(|row| row.status == AdapterStatus::Connecting) {
        if ui.button("Cancel").clicked() {
            out.push(Intent::Slack(SlackIntent::Cancel));
        }
        return;
    }
    if slack.is_some_and(|row| row.linked()) {
        return;
    }
    if snapshot.persistence() == Persistence::Loading {
        ui.add_enabled(false, egui::Button::new("Add Slack workspace"));
        return;
    }
    if ui.button("Add Slack workspace").clicked() {
        out.push(Intent::Slack(SlackIntent::Connect));
    }
}

#[cfg(not(feature = "slack-oauth"))]
fn add_slack_workspace(_ui: &mut egui::Ui, _snapshot: &View<'_>, _out: &mut Vec<Intent>) {}

fn thread(ui: &mut egui::Ui, snapshot: &View<'_>, hints: &mut Hints, out: &mut Vec<Intent>) {
    if snapshot.selected_conversation_row().is_none() {
        empty_thread(ui, snapshot);
        return;
    }
    thread_header(ui, snapshot, out);
    let palette = theme::palette(ui);
    let signal_removed =
        snapshot.signal_device_removed() && snapshot.selected_protocol == ProtocolId::Signal;
    if snapshot.signal_read_only_reason().is_some() || signal_removed {
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(THREAD_INSET as i8, space::S as i8))
            .show(ui, |ui| {
                if let Some(reason) = snapshot.signal_read_only_reason() {
                    note_box(ui, palette.warn, reason);
                    // The README risk line. The previous number stays read-only.
                    ui.label(
                        RichText::new("Signal: no supported third-party API, expect breakage.")
                            .color(palette.text2),
                    );
                } else {
                    note_box(ui, palette.warn, SIGNAL_DEVICE_REMOVED);
                    ui.label(
                        RichText::new(thinwire_core::state::SIGNAL_SENDING_OFF)
                            .color(palette.text2),
                    );
                }
            });
    }
    if snapshot.showing_topic_list() {
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(space::S as i8, space::S as i8))
            .show(ui, |ui| topic_list(ui, snapshot, out));
        return;
    }

    let is_group = snapshot
        .selected_conversation_row()
        .is_some_and(|row| row.is_group);
    let layout = match snapshot.now() {
        ViewNow::Local(now) => thread_rows(snapshot.selected_messages(), is_group, &now),
        ViewNow::Fixed(now) => thread_rows(snapshot.selected_messages(), is_group, &now),
    };
    let messages: Vec<Bubble<'_>> = snapshot
        .selected_messages()
        .iter()
        .zip(layout)
        .map(|(message, layout)| Bubble {
            id: &message.id,
            protocol: message.protocol,
            conversation_id: &message.conversation_id,
            outbound: message.outbound,
            sender: &message.sender,
            body: &message.body,
            links: &message.links,
            photo: message.photo.as_ref(),
            delivery: message.delivery,
            group: is_group,
            layout,
        })
        .collect();

    // Compose grows to COMPOSE_MAX_ROWS lines, then scrolls. Leave the whole
    // bar inside the panel when the message list hits its max height.
    let row_height = ui.text_style_height(&egui::TextStyle::Body);
    let compose_rows = compose_line_count(&snapshot.compose);
    let compose_height = compose_row_height(row_height, COMPOSE_MAX_ROWS);
    let reserve = compose_reserve(row_height, compose_rows, ui.spacing().item_spacing.y)
        + super::attach::tray_height(ui, snapshot);

    let state = snapshot.thread_state();
    let older = snapshot.older_state();
    let mut retry: Option<String> = None;
    // One scroll state per chat, and per topic inside a forum, so each
    // thread opens at its newest message.
    let salt = thread_salt(snapshot);
    let memo_id = ui.make_persistent_id(("thread-older", &salt));
    let memo: ThreadMemo = ui.data(|data| data.get_temp(memo_id)).unwrap_or_default();
    let scroll_id = ui.make_persistent_id(("thread-scroll", &salt));
    let scroll: ThreadScroll = ui.data(|data| data.get_temp(scroll_id)).unwrap_or_default();
    // Another chat, or the first rows of this one, fade in. The pass that
    // draws them first still paints at the old offset: `stick_to_bottom`
    // reaches the newest message only at the end of that pass.
    let fade = motion::fade_in_on_change(
        ui.ctx(),
        ui.scope_id().with("thread-fade"),
        egui::Id::unique((
            snapshot.selected_protocol,
            &snapshot.selected_conversation,
            snapshot.selected_topic,
            state == ThreadState::Rows,
        )),
        motion::FADE,
    );
    let mut area = egui::ScrollArea::vertical()
        .id_salt(("thread", salt))
        .auto_shrink([false, true])
        .stick_to_bottom(true)
        .max_height(ui.available_height() - reserve);
    if let Some(offset) = memo.restore {
        area = area.vertical_scroll_offset(offset);
    }
    let output = area.show(ui, |ui| {
        ui.multiply_opacity(fade);
        egui::Frame::new()
            .inner_margin(egui::Margin::symmetric(THREAD_INSET as i8, 0))
            .show(ui, |ui| {
                ui.add_space(space::M);
                match older {
                    OlderState::Loading => {
                        ui.horizontal(|ui| {
                            motion::spinner(ui);
                            ui.label(RichText::new("Loading older messages…").color(palette.text3));
                        });
                    }
                    OlderState::StartOfChat if state == ThreadState::Rows => {
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("Start of chat").small().color(palette.text3));
                        });
                    }
                    OlderState::StartOfChat | OlderState::Idle => {}
                }
                if older != OlderState::Loading
                    && let Some(note) = snapshot.older_note()
                {
                    // A failed older page: a note for this chat, not an error block.
                    // The next successful page clears it (#57).
                    ui.label(RichText::new(note).small().color(theme::palette(ui).warn));
                }
                match state {
                    ThreadState::Loading => {
                        ui.horizontal(|ui| {
                            motion::spinner(ui);
                            ui.label(RichText::new("Loading messages…").color(palette.text3));
                        });
                    }
                    ThreadState::Empty => {
                        ui.label(RichText::new("No messages in this chat.").color(palette.text3));
                    }
                    ThreadState::NoSelection | ThreadState::Rows => {}
                }
                let mut gap_before = None;
                for message in &messages {
                    bubble(ui, message, &mut retry, gap_before, out);
                    gap_before = Some(message.layout.run_end);
                }
                ui.add_space(space::M);
            });
        if scroll.jump {
            ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
        }
    });
    // `stick_to_bottom` moves the offset after this pass painted. Draw the
    // next frame now, not at the idle tick, so a new message shows at once.
    if (output.state.clamped_offset().y - scroll.offset).abs() > 0.01 {
        ui.ctx().request_repaint();
    }
    let below =
        output.content_size.y - output.inner_rect.height() - output.state.clamped_offset().y;
    scroll_fades(
        ui,
        output.inner_rect,
        output.state.clamped_offset().y,
        below,
    );
    let jump = jump_to_latest(
        ui,
        output.inner_rect,
        state == ThreadState::Rows && below > JUMP_AFTER,
    );
    if jump {
        ui.ctx().request_repaint();
    }
    ui.data_mut(|data| {
        data.insert_temp(
            scroll_id,
            ThreadScroll {
                offset: output.state.clamped_offset().y,
                jump,
            },
        );
    });
    let first_id = messages.first().map(|message| message.id.to_owned());
    let step = older_step(
        &memo,
        first_id.as_deref(),
        |id| messages.iter().any(|message| message.id == id),
        output.state.clamped_offset().y,
        output.content_size.y,
        cannot_leave_zone(output.content_size.y, output.inner_rect.height()),
    );
    if step.restore.is_some() {
        // Older rows went in above: the next frame moves down by their height,
        // so the row that was on screen stays there (#30). No
        // `request_discard`: the app dispatches the intents of each pass, and
        // a second pass gets no input, so a redo could lose or repeat a typed
        // change (PR #61 review). The cost is one frame at the old offset.
        ui.ctx().request_repaint();
    }
    ui.data_mut(|data| {
        data.insert_temp(
            memo_id,
            ThreadMemo {
                first_id: first_id.clone(),
                content_height: output.content_size.y,
                restore: step.restore,
                in_zone: step.in_zone,
            },
        );
    });
    // The core says when a request may go out: not during a wait after a
    // page that brought nothing. So a thread that cannot scroll does not
    // send an intent on every repaint (#67).
    if step.at_top
        && state == ThreadState::Rows
        && older == OlderState::Idle
        && snapshot.older_can_ask()
        && let Some((protocol, conversation_id)) = snapshot
            .selected_conversation
            .clone()
            .map(|id| (snapshot.selected_protocol, id))
    {
        out.push(Intent::LoadOlderMessages {
            protocol,
            conversation_id,
            topic: snapshot.selected_topic,
        });
    }
    if let Some(message_id) = retry {
        out.push(Intent::Retry { message_id });
    }

    compose(ui, snapshot, hints, compose_height, out);
    if snapshot.sends_media(snapshot.selected_protocol) {
        super::attach::drop_hint(ui, ui.max_rect());
    }
}

/// A soft edge where messages scroll under the header or the compose bar.
/// `above` and `below` are the points of content past each edge: a fade is
/// only as tall as that, so a chat at its top or bottom edge has none.
fn scroll_fades(ui: &egui::Ui, view: egui::Rect, above: f32, below: f32) {
    let ground = theme::palette(ui).bg;
    let top = above.clamp(0.0, SCROLL_FADE);
    let bottom = below.clamp(0.0, SCROLL_FADE);
    if top > 0.5 {
        let band = egui::Rect::from_min_size(view.min, egui::vec2(view.width(), top));
        fade_band(ui, band, ground, egui::Color32::TRANSPARENT);
    }
    if bottom > 0.5 {
        let band = egui::Rect::from_min_size(
            egui::pos2(view.left(), view.bottom() - bottom),
            egui::vec2(view.width(), bottom),
        );
        fade_band(ui, band, egui::Color32::TRANSPARENT, ground);
    }
}

/// A vertical gradient from `from` at the top of `band` to `to` at the bottom.
fn fade_band(ui: &egui::Ui, band: egui::Rect, from: egui::Color32, to: egui::Color32) {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(band.left_top(), from);
    mesh.colored_vertex(band.right_top(), from);
    mesh.colored_vertex(band.right_bottom(), to);
    mesh.colored_vertex(band.left_bottom(), to);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    ui.painter().add(egui::Shape::mesh(mesh));
}

/// What the thread remembers between frames for its scroll motion.
#[derive(Debug, Clone, Copy, Default)]
struct ThreadScroll {
    /// Scroll offset at the end of the last pass. The next pass paints at it.
    offset: f32,
    /// The jump button was clicked: scroll to the newest message this pass.
    jump: bool,
}

/// Points above the newest message from which the thread offers a jump back.
const JUMP_AFTER: f32 = 200.0;
/// Diameter of the jump button.
const JUMP_SIZE: f32 = 36.0;

/// Round button over the bottom right of the messages. It fades in while the
/// newest message is out of view. True on a click.
fn jump_to_latest(ui: &mut egui::Ui, view: egui::Rect, offered: bool) -> bool {
    let id = ui.scope_id().with("jump-to-latest");
    let shown = motion::toggle(ui.ctx(), id, offered, motion::FADE);
    if shown <= 0.0 {
        return false;
    }
    let palette = theme::palette(ui);
    let radius = JUMP_SIZE * 0.5;
    // It rises into place as it fades in.
    let rise = (1.0 - shown) * space::S;
    let center = egui::pos2(
        view.right() - ui.spacing().scroll.bar_width - space::M - radius,
        view.bottom() - space::M - radius + rise,
    );
    let rect = egui::Rect::from_center_size(center, egui::vec2(JUMP_SIZE, JUMP_SIZE));
    // Fading out, it no longer takes clicks from the messages under it.
    let sense = if offered {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let response = ui.interact(rect, id, sense);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::Role::Button, offered, "Jump to latest message")
    });
    let hover = motion::toggle(
        ui.ctx(),
        id.with("hover"),
        offered && response.hovered(),
        motion::HOVER,
    );
    let mut painter = ui.painter().clone();
    painter.multiply_opacity(shown);
    painter.circle(
        center,
        radius,
        motion::mix(palette.surface, palette.hover, hover),
        egui::Stroke::new(1.0, palette.border_strong),
    );
    let tip = center + egui::vec2(0.0, 3.0);
    painter.add(egui::Shape::line(
        vec![
            tip + egui::vec2(-5.0, -5.0),
            tip,
            tip + egui::vec2(5.0, -5.0),
        ],
        egui::Stroke::new(2.0, palette.text),
    ));
    offered && response.clicked()
}

/// Distance from the top of the thread, in points, that asks for older
/// messages.
const OLDER_TRIGGER: f32 = 24.0;

/// The view cannot scroll out of the top zone: its largest scroll offset is
/// not more than `OLDER_TRIGGER`. A thread that fits, or is only a little
/// taller than the view, is such a thread (#74 review).
fn cannot_leave_zone(content_height: f32, view_height: f32) -> bool {
    content_height - view_height <= OLDER_TRIGGER
}

/// What the thread remembers between frames for older-message paging (#30).
#[derive(Debug, Clone, Default)]
struct ThreadMemo {
    /// The oldest row that the last frame drew.
    first_id: Option<String>,
    content_height: f32,
    /// Scroll offset for the next frame, after older rows went in above.
    restore: Option<f32>,
    /// The last frame was in the top zone. A request goes out only when the
    /// view enters the zone, not on each repaint inside it (#61 review).
    in_zone: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct OlderStep {
    /// The view entered the top zone of rows that did not change since the
    /// last frame: ask for older messages. Once, not on each repaint.
    at_top: bool,
    /// Remember for the next frame: the view is in the top zone.
    in_zone: bool,
    /// Older rows went in above the rows of the last frame: the offset that
    /// keeps the old top row in place.
    restore: Option<f32>,
}

/// Pure frame logic for older-message paging. A chat that just opened (no
/// rows last frame) never asks: `stick_to_bottom` moves it to the newest
/// message first. The ask is edge-triggered: the view must enter the zone.
/// New rows above count as a new entry, so a short chat keeps filling. A
/// page that brought nothing leaves the rows as they are: no new ask until
/// the view leaves the zone and comes back (the core also waits).
fn older_step(
    memo: &ThreadMemo,
    first_id: Option<&str>,
    has_row: impl Fn(&str) -> bool,
    offset: f32,
    content_height: f32,
    stuck: bool,
) -> OlderStep {
    let idle = OlderStep {
        at_top: false,
        restore: None,
        in_zone: false,
    };
    let Some(first) = first_id else {
        return idle;
    };
    let zone = offset <= OLDER_TRIGGER;
    match memo.first_id.as_deref() {
        Some(old) if old == first => OlderStep {
            // A thread that cannot leave the zone (`cannot_leave_zone`) asks
            // while it stays there, and the core paces it (#67).
            at_top: memo.restore.is_none() && zone && (!memo.in_zone || stuck),
            restore: None,
            in_zone: zone,
        },
        Some(old) if memo.restore.is_none() && has_row(old) => OlderStep {
            restore: Some((offset + content_height - memo.content_height).max(0.0)),
            ..idle
        },
        _ => idle,
    }
}

/// Colors of the time and the delivery marks. They depend on what is behind
/// them: the bubble fill, or the plain thread for a message of only emoji.
struct MetaInk {
    time: egui::Color32,
    /// The "read" ticks.
    read: egui::Color32,
    /// "Not sent."
    error: egui::Color32,
    /// Retry.
    action: egui::Color32,
}

/// A message of one to three emoji and nothing else. It draws large, with no
/// bubble. A photo or a link keeps its bubble.
fn is_emoji_only(message: &Bubble<'_>) -> bool {
    message.photo.is_none() && message.links.is_empty() && only_emoji(message.body)
}

/// One to three emoji, with the joiners, skin tones, and spaces between them.
/// A joined sequence, such as a family, counts as one.
fn only_emoji(text: &str) -> bool {
    let is_emoji = |ch: char| {
        matches!(ch,
            '\u{1F000}'..='\u{1FAFF}' | '\u{2600}'..='\u{27BF}' | '\u{2B00}'..='\u{2BFF}'
            | '\u{2300}'..='\u{23FF}')
    };
    let mut units = 0;
    let mut joined = false;
    for ch in text.chars() {
        match ch {
            _ if ch.is_whitespace() => {}
            '\u{200D}' => {
                joined = true;
                continue;
            }
            '\u{FE0F}' | '\u{20E3}' | '\u{1F3FB}'..='\u{1F3FF}' => {}
            _ if is_emoji(ch) => {
                if !joined {
                    units += 1;
                }
            }
            _ => return false,
        }
        joined = false;
    }
    (1..=3).contains(&units)
}

/// One message row, ready to draw. The photo stays a reference so a frame
/// does not copy the image bytes.
struct Bubble<'a> {
    id: &'a str,
    protocol: ProtocolId,
    conversation_id: &'a str,
    outbound: bool,
    sender: &'a str,
    body: &'a str,
    links: &'a [MessageLink],
    photo: Option<&'a MessagePhoto>,
    delivery: Delivery,
    /// Group chat. The AccessKit name keeps the sender on every row.
    group: bool,
    layout: RowLayout,
}

/// Scroll id of the open thread. A topic keeps its own offset.
fn thread_salt(snapshot: &View<'_>) -> String {
    match (
        snapshot.selected_conversation.as_deref(),
        snapshot.selected_topic,
    ) {
        (Some(id), Some(topic)) => topic_mute_id(id, topic),
        (Some(id), None) => id.to_owned(),
        (None, _) => String::new(),
    }
}

/// Stable row id for the topic list anchor.
fn topic_anchor(topic: MessageTopic) -> String {
    match topic {
        MessageTopic::Forum(id) => format!("forum:{id}"),
        MessageTopic::Thread(id) => format!("thread:{id}"),
    }
}

/// The topics of the open forum or bot chat. A reorder keeps the anchor row
/// on the same screen line.
fn topic_list(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    let Some(conversation) = snapshot.selected_conversation_row() else {
        return;
    };
    let topics = snapshot.selected_topics();
    let ids: Vec<String> = topics
        .iter()
        .map(|topic| topic_anchor(topic.topic))
        .collect();
    let memo_id = ui.make_persistent_id(("topic-anchor", conversation.id.clone()));
    let previous: Option<InboxAnchor> = ui.data(|data| data.get_temp(memo_id));
    let forced = previous
        .as_ref()
        .and_then(|previous| anchored_inbox_offset(previous, &ids));
    let mut area = egui::ScrollArea::vertical()
        .id_salt(("topics", &conversation.id))
        .auto_shrink([false, false]);
    if let Some(offset) = forced {
        area = area.vertical_scroll_offset(offset);
    }
    let stride = topic_row_height(ui) + ROW_GAP;
    let mut drawn = DrawnInbox {
        anchor: None,
        stride,
    };
    let output = area.show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = ROW_GAP;
        if topics.is_empty() {
            empty_note(ui, "No topics.");
            return;
        }
        let now = snapshot.now();
        let pointer = ui.input(|input| input.pointer.hover_pos());
        let clip = ui.clip_rect();
        let mut under_pointer = None;
        let mut first_visible = None;
        for topic in &topics {
            let anchor = topic_anchor(topic.topic);
            let mute = snapshot.topic_mute(topic);
            let response = topic_row(
                ui,
                &anchor,
                &topic.name,
                &view_list_time(topic.last_at, now),
                topic.unread,
                topic.closed,
                mute.is_muted(),
            );
            let protocol = topic.protocol;
            let mute_id = topic_mute_id(&topic.conversation_id, topic.topic);
            response.context_menu(|ui| {
                if let Some(intent) = mute_control(ui, protocol, &mute_id, mute, "topic") {
                    out.push(intent);
                    ui.close();
                }
            });
            if under_pointer.is_none() && pointer.is_some_and(|pos| response.rect.contains(pos)) {
                under_pointer = Some(anchor.clone());
            }
            if first_visible.is_none() && response.rect.intersects(clip) {
                first_visible = Some(anchor.clone());
            }
            if response.clicked() {
                out.push(Intent::SelectTopic {
                    conversation_id: topic.conversation_id.clone(),
                    topic: topic.topic,
                });
            }
        }
        drawn.anchor = under_pointer.or(first_visible);
    });
    ui.data_mut(|data| {
        data.insert_temp(
            memo_id,
            InboxAnchor {
                ids,
                offset: output.state.clamped_offset().y,
                stride: drawn.stride,
                anchor: drawn.anchor,
            },
        );
    });
}

/// Side of the tile at the start of a topic row.
const TOPIC_TILE: f32 = 32.0;

/// Height of a topic row: one line beside its tile.
fn topic_row_height(ui: &egui::Ui) -> f32 {
    let title = ui.text_style_height(&theme::row_title());
    TOPIC_TILE.max(title) + theme::ROW_INSET.y * 2.0
}

/// One topic row. A closed topic shows a lock beside the name.
#[allow(clippy::too_many_arguments)]
fn topic_row(
    ui: &mut egui::Ui,
    id: &str,
    name: &str,
    time: &str,
    unread: u32,
    closed: bool,
    muted: bool,
) -> egui::Response {
    let palette = theme::palette(ui);
    let width = laid_out_width(ui, "topic-row-width");
    let (rect, hover) = ui.allocate_exact_size(
        egui::vec2(width, topic_row_height(ui)),
        egui::Sense::hover(),
    );
    let row_id = ui.scope_id().with(("topic-row", id));
    row_backdrop(ui, row_id, rect, palette.bg, false, hover.hovered());
    let gutter = ui.spacing().scroll.bar_width;
    let inner = theme::row_content_rect(rect, gutter);
    // A topic has no photo and no preview: one line, after a small tile.
    // The tile and the line are both centered on the row.
    let tile = egui::Rect::from_center_size(
        egui::pos2(inner.left() + TOPIC_TILE * 0.5, inner.center().y),
        egui::vec2(TOPIC_TILE, TOPIC_TILE),
    );
    ui.painter()
        .rect_filled(tile, egui::CornerRadius::same(8), palette.sidebar);
    icons::hash(ui.painter(), tile.center(), 16.0, palette.text2);
    let line_height = ui.text_style_height(&theme::row_title());
    let line = egui::Rect::from_min_max(
        egui::pos2(
            tile.right() + space::M,
            inner.center().y - line_height * 0.5,
        ),
        egui::pos2(inner.right(), inner.center().y + line_height * 0.5),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(line)
            .layout(egui::Layout::left_to_right(egui::Align::Center))
            .id_salt(("topic-row", id)),
    );
    // A nested row starts as tall as this. Zero would put its top on the
    // center line, and the text would sit under the tile center.
    child.spacing_mut().interact_size.y = line_height;
    child.style_mut().interaction.selectable_labels = false;
    let title_font = if unread > 0 {
        theme::semibold()
    } else {
        theme::medium()
    };
    row_ends(
        &mut child,
        |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = space::XS;
                if closed {
                    lock_icon(ui, palette.text2);
                }
                ui.add(
                    egui::Label::new(
                        RichText::new(name)
                            .text_style(theme::row_title())
                            .family(title_font)
                            .color(if closed { palette.text2 } else { palette.text }),
                    )
                    .truncate()
                    .halign(egui::Align::LEFT),
                );
            });
        },
        |ui| {
            ui.spacing_mut().item_spacing.x = space::S;
            if let Some(text) = badge_text(unread) {
                unread_badge(ui, &text, muted);
            }
            ui.label(RichText::new(time).small().color(palette.text3));
            if muted {
                ui.label(RichText::new("muted").small().color(palette.text3));
            }
        },
    );
    let response = ui
        .interact(rect, row_id, egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    let access = if closed {
        format!("{name}, closed")
    } else {
        name.to_owned()
    };
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, access.clone()));
    response
}

/// A small lock, painted with strokes so the snapshot font does not matter.
fn lock_icon(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Label, true, "closed"));
    let stroke = egui::Stroke::new(1.5, color);
    let body = egui::Rect::from_min_size(
        egui::pos2(rect.left() + 2.0, rect.center().y),
        egui::vec2(rect.width() - 4.0, rect.height() * 0.42),
    );
    ui.painter().rect_stroke(
        body,
        egui::CornerRadius::same(2),
        stroke,
        egui::StrokeKind::Inside,
    );
    let left = body.left() + 2.5;
    let right = body.right() - 2.5;
    let top = rect.top() + 1.5;
    ui.painter().add(egui::Shape::line(
        vec![
            egui::pos2(left, body.top()),
            egui::pos2(left, top + 2.0),
            egui::pos2(right, top + 2.0),
            egui::pos2(right, body.top()),
        ],
        stroke,
    ));
}

/// Avatar, title, a line that says what the chat is, and the mute button,
/// over a hairline.
fn thread_header(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    let palette = theme::palette(ui);
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(THREAD_INSET as i8, space::M as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let Some(conversation) = snapshot.selected_conversation_row() else {
                return;
            };
            let mute = snapshot.chat_mute(conversation.protocol, &conversation.id);
            // One row: the button sits right, and the title takes the rest
            // and truncates.
            let open = snapshot.open_topic();
            let (mute_id, mute) = match open {
                Some(topic) => (
                    topic_mute_id(&conversation.id, topic.topic),
                    snapshot.topic_mute(topic),
                ),
                None => (conversation.id.clone(), mute),
            };
            let kind = if conversation.is_group {
                format!("Group · {}", conversation.protocol.display_name())
            } else {
                conversation.protocol.display_name().to_owned()
            };
            // The left side pushes here: the right side holds `out`.
            let mut avatar_out = Vec::new();
            egui::Sides::new().shrink_left().truncate().show(
                ui,
                |ui| {
                    // An open topic shows Back. It goes to the topic list.
                    if open.is_some()
                        && icon_button(ui, "Back to topics", icons::chevron_left, palette.text2)
                            .on_hover_text("All topics")
                            .clicked()
                    {
                        avatar_out.push(Intent::CloseTopic {
                            conversation_id: conversation.id.clone(),
                        });
                    }
                    let side = super::photos::HEADER_AVATAR_SIDE;
                    let (avatar, _) =
                        ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
                    super::photos::paint_avatar(
                        ui,
                        avatar,
                        conversation.protocol,
                        &conversation.id,
                        &conversation.title,
                        &conversation.avatar,
                        palette.bg,
                        &mut avatar_out,
                    );
                    ui.add_space(space::XS);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = space::XS;
                            let Some(topic) = open else {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&conversation.title)
                                            .heading()
                                            .color(palette.text),
                                    )
                                    .truncate(),
                                );
                                return;
                            };
                            // The breadcrumb: the chat name is a link to its
                            // topic list.
                            // A long chat name takes half the row at most,
                            // so the topic name stays in view.
                            let room = ui.available_width() * 0.5;
                            let crumb = ui
                                .scope(|ui| {
                                    ui.set_max_width(room);
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&conversation.title)
                                                .heading()
                                                .color(palette.text2),
                                        )
                                        .selectable(false)
                                        .truncate()
                                        .sense(egui::Sense::click()),
                                    )
                                })
                                .inner
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text("All topics");
                            if crumb.hovered() {
                                let rect = crumb.rect;
                                ui.painter().hline(
                                    rect.x_range(),
                                    rect.bottom() - 1.0,
                                    egui::Stroke::new(1.0, palette.text2),
                                );
                            }
                            if crumb.clicked() {
                                avatar_out.push(Intent::CloseTopic {
                                    conversation_id: conversation.id.clone(),
                                });
                            }
                            ui.label(RichText::new("›").heading().color(palette.text3));
                            if topic.closed {
                                lock_icon(ui, palette.text2);
                            }
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&topic.name).heading().color(palette.text),
                                )
                                .truncate(),
                            );
                        });
                        ui.label(
                            RichText::new(kind)
                                .text_style(theme::secondary())
                                .color(palette.text3),
                        );
                    });
                },
                |ui| {
                    if let Some(intent) =
                        header_mute(ui, conversation.protocol, &mute_id, mute, open.is_some())
                    {
                        out.push(intent);
                    }
                },
            );
            out.append(&mut avatar_out);
        });
    let y = ui.cursor().min.y;
    ui.painter().hline(
        ui.max_rect().x_range(),
        y,
        egui::Stroke::new(1.0, palette.border),
    );
}

/// The thread column when no chat is open: what to do, and a picture.
fn empty_thread(ui: &mut egui::Ui, snapshot: &View<'_>) {
    let palette = theme::palette(ui);
    let (title, hint) = match snapshot.inbox_state() {
        InboxState::Loading => ("Hang tight", "Your conversations show up here soon."),
        InboxState::Empty => ("Nothing here yet", "New conversations show up here."),
        InboxState::Rows | InboxState::NoMatch => (
            "Select a chat",
            "Pick a conversation from the list to read and reply.",
        ),
    };
    let top = (ui.available_height() * 0.5 - 90.0).max(24.0);
    ui.add_space(top);
    ui.vertical_centered(|ui| {
        let (art, _) = ui.allocate_exact_size(egui::vec2(56.0, 56.0), egui::Sense::hover());
        icons::chat_bubble(ui.painter(), art.center(), 56.0, palette.text3);
        ui.add_space(space::M);
        ui.label(
            RichText::new(title)
                .text_style(egui::TextStyle::Heading)
                .color(palette.text),
        );
        ui.label(
            RichText::new(hint)
                .text_style(theme::secondary())
                .color(palette.text2),
        );
    });
}

/// Own messages sit on the right on `out`. Others sit on the left on `surface`.
///
/// `previous_run_ended` is `None` for the first row. `Some(true)` inserts the
/// gap between runs. `Some(false)` inserts the gap inside a run.
fn bubble(
    ui: &mut egui::Ui,
    message: &Bubble<'_>,
    retry: &mut Option<String>,
    previous_run_ended: Option<bool>,
    out: &mut Vec<Intent>,
) {
    let palette = theme::palette(ui);
    if let Some(day) = &message.layout.day_break {
        ui.add_space(space::M);
        ui.vertical_centered(|ui| {
            egui::Frame::new()
                .fill(palette.surface)
                .corner_radius(egui::CornerRadius::same(radius::PILL))
                .inner_margin(egui::Margin::symmetric(space::S as i8, space::XS as i8))
                .show(ui, |ui| {
                    ui.label(RichText::new(day).small().color(palette.text3));
                });
        });
        ui.add_space(space::M);
    } else if let Some(ended) = previous_run_ended {
        ui.add_space(if ended { NEXT_RUN_GAP } else { SAME_RUN_GAP });
    }
    let max_width = bubble_cap(laid_out_width(ui, "thread-width"));
    let emoji = is_emoji_only(message);
    let align = if message.outbound {
        egui::Align::Max
    } else {
        egui::Align::Min
    };
    let (fill, body, ink) = if emoji {
        (
            egui::Color32::TRANSPARENT,
            palette.text,
            MetaInk {
                time: palette.text3,
                read: palette.accent,
                error: palette.error,
                action: palette.accent,
            },
        )
    } else if message.outbound {
        (
            palette.out,
            palette.on_out,
            MetaInk {
                time: palette.out_meta,
                read: palette.on_out,
                error: palette.out_error,
                action: palette.on_out,
            },
        )
    } else {
        (
            palette.surface,
            palette.text,
            MetaInk {
                time: palette.text3,
                read: palette.text3,
                error: palette.error,
                action: palette.accent,
            },
        )
    };
    let mut body_galley = None;
    ui.with_layout(egui::Layout::top_down(align), |ui| {
        let frame = egui::Frame::new()
            .fill(fill)
            .corner_radius(bubble_radius(message.outbound, message.layout.run_end))
            .inner_margin(egui::Margin::symmetric(BUBBLE_PAD_X, BUBBLE_PAD_Y))
            .show(ui, |ui| {
                ui.set_max_width(max_width);
                let meta_width =
                    meta_column_width(ui, &message.layout.time, message.delivery, message.outbound);
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    if message.layout.show_sender {
                        let ink = theme::sender_ink(message.sender, ui.visuals().dark_mode);
                        let response = ui.label(
                            RichText::new(message.sender)
                                .small()
                                .family(theme::semibold())
                                .color(ink),
                        );
                        // The pane name already includes the sender.
                        hide_from_accesskit(ui, response.id);
                    }
                    if let Some(photo) = message.photo {
                        super::photos::show(
                            ui,
                            message.protocol,
                            message.conversation_id,
                            message.id,
                            photo,
                            out,
                        );
                    }
                    let caption = !message.body.is_empty() || message.photo.is_none();
                    if !caption {
                        meta_line(ui, message, &ink, false);
                        return;
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                        let gap = ui.spacing().item_spacing.x;
                        let text_width = (ui.available_width() - meta_width - gap).max(32.0);
                        let body_rect = ui
                            .vertical(|ui| {
                                ui.set_max_width(text_width);
                                let text = if message.links.is_empty() {
                                    let mut text = RichText::new(message.body).color(body);
                                    if emoji {
                                        text = text.size(EMOJI_SIZE);
                                    }
                                    egui::WidgetText::from(text)
                                } else {
                                    let font = egui::TextStyle::Body.resolve(ui.style());
                                    egui::WidgetText::from(super::links::layout_job(
                                        message.body,
                                        message.links,
                                        body,
                                        if message.outbound {
                                            palette.out_link
                                        } else {
                                            palette.link
                                        },
                                        font,
                                    ))
                                };
                                let (galley_pos, galley, response) = egui::Label::new(text)
                                    .selectable(true)
                                    .wrap()
                                    .halign(egui::Align::LEFT)
                                    .layout_in_ui(ui);
                                response.widget_info(|| {
                                    egui::WidgetInfo::labeled(
                                        egui::Role::Label,
                                        ui.is_enabled(),
                                        galley.text(),
                                    )
                                });
                                // The pane already names this text. Selection is bound
                                // to the pane id below, with the text runs.
                                hide_from_accesskit(ui, response.id);
                                body_galley = Some((galley_pos, galley, response));
                            })
                            .response
                            .rect;
                        let line = ui.text_style_height(&egui::TextStyle::Small);
                        let has_time = !message.layout.time.is_empty();
                        let inline_mark = delivery_inline(message.delivery, message.outbound);
                        let extra = delivery_rows(message.delivery, message.outbound);
                        if has_time || inline_mark || extra > 0 {
                            let gap_y = ui.spacing().item_spacing.y;
                            let time_h = if has_time || inline_mark { line } else { 0.0 };
                            // The time shares the last body line. A failed send
                            // continues on a row below it.
                            let extra_h = if extra == 0 {
                                0.0
                            } else {
                                gap_y + line * extra as f32 + gap_y * (extra - 1) as f32
                            };
                            let meta_top = if has_time || inline_mark {
                                body_rect.bottom() - time_h
                            } else {
                                body_rect.bottom() + gap_y
                            };
                            let meta_rect = egui::Rect::from_min_size(
                                egui::pos2(body_rect.right() + gap, meta_top),
                                egui::vec2(meta_width.max(1.0), time_h + extra_h),
                            );
                            ui.scope_builder(
                                egui::UiBuilder::new()
                                    .max_rect(meta_rect)
                                    .layout(egui::Layout::top_down(egui::Align::Min)),
                                |ui| {
                                    meta_line(ui, message, &ink, message.group);
                                    if message.outbound && message.delivery == Delivery::Failed {
                                        failed_line(ui, message, &ink, retry);
                                    }
                                },
                            );
                        }
                    });
                });
            })
            .response;
        if message.photo.is_some() {
            ui.ctx().data_mut(|data| {
                data.insert_temp(
                    egui::Id::unique("thinwire-photo-row-height"),
                    frame.rect.height(),
                );
            });
        }
        if let Some((galley_pos, galley, mut response)) = body_galley {
            // The text runs and the selection handler share the pane id.
            // A selection action from assistive tech targets that id.
            response.id = frame.id;
            egui::text_selection::LabelSelectionState::label_text_selection(
                ui,
                &response,
                galley_pos,
                galley.clone(),
                body,
                egui::Stroke::NONE,
            );
            if !message.links.is_empty() {
                super::links::interact(
                    ui,
                    &response,
                    galley_pos,
                    &galley,
                    message.body,
                    message.links,
                );
            }
        }
        // Restore the pane after selection, which sets a label role.
        frame.widget_info(|| {
            egui::WidgetInfo::labeled(egui::Role::Pane, true, bubble_access_label(message))
        });
    });
}

/// Drop a painted widget from the screen-reader tree.
fn hide_from_accesskit(ui: &egui::Ui, id: egui::Id) {
    ui.ctx().accesskit_node_builder(id, |node| {
        node.set_hidden();
    });
}

/// A check mark. The painted glyph stays `glyph`. A screen reader reads `access`.
fn delivery_mark(ui: &mut egui::Ui, glyph: &str, access: &str, color: egui::Color32) {
    let response = ui.label(RichText::new(glyph).small().color(color));
    let access = access.to_owned();
    ui.ctx().accesskit_node_builder(response.id, |node| {
        node.set_label(access);
    });
}

/// "Sending…" beside the time of an outgoing message that the server has not
/// accepted.
const SENDING_MARK: &str = "Sending…";
/// One check. The server has the message. Small, in the muted bubble ink. No
/// animation.
const SENT_MARK: &str = "✓";
/// Two checks. The other party has read the message. They use the bright
/// bubble text color: the accent is the bubble fill, so it cannot be the mark.
const READ_MARK: &str = "✓✓";
/// The server did not accept the message.
const FAILED_MARK: &str = "Not sent.";

/// Extra rows under the time line. A failed send adds one: the message and Retry.
fn delivery_rows(delivery: Delivery, outbound: bool) -> i32 {
    i32::from(outbound && delivery == Delivery::Failed)
}

/// The delivery mark sits on the time line for every state but a failure.
fn delivery_inline(delivery: Delivery, outbound: bool) -> bool {
    outbound && delivery != Delivery::Failed
}

/// The time, then the delivery mark on the same line.
fn meta_line(ui: &mut egui::Ui, message: &Bubble<'_>, ink: &MetaInk, hide_time: bool) {
    ui.scope(|ui| {
        ui.spacing_mut().interact_size.y = 0.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = space::XS;
            if !message.layout.time.is_empty() {
                let response =
                    ui.label(RichText::new(&message.layout.time).small().color(ink.time));
                if hide_time {
                    // The pane name already includes the time.
                    hide_from_accesskit(ui, response.id);
                }
            }
            if !delivery_inline(message.delivery, message.outbound) {
                return;
            }
            match message.delivery {
                Delivery::Pending => {
                    ui.label(RichText::new(SENDING_MARK).small().color(ink.time));
                }
                Delivery::Sent => delivery_mark(ui, SENT_MARK, "Sent", ink.time),
                Delivery::Read => delivery_mark(ui, READ_MARK, "Read", ink.read),
                Delivery::Failed => {}
            }
        });
    });
}

/// "Not sent." and Retry, on the row under the time of a failed send.
fn failed_line(ui: &mut egui::Ui, message: &Bubble<'_>, ink: &MetaInk, retry: &mut Option<String>) {
    ui.scope(|ui| {
        ui.spacing_mut().interact_size.y = 0.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = space::S;
            ui.label(RichText::new(FAILED_MARK).small().color(ink.error));
            if ui
                .add(
                    egui::Button::new(RichText::new("Retry").small().underline().color(ink.action))
                        .frame(false),
                )
                .clicked()
            {
                *retry = Some(message.id.to_owned());
            }
        });
    });
}

/// Width of the time and delivery column. Empty when the row has neither.
fn meta_column_width(ui: &egui::Ui, time: &str, delivery: Delivery, outbound: bool) -> f32 {
    let font = egui::FontId::new(size::CAPTION, egui::FontFamily::Proportional);
    let text_width = |text: &str| -> f32 {
        if text.is_empty() {
            0.0
        } else {
            ui.painter()
                .layout_no_wrap(text.to_owned(), font.clone(), egui::Color32::PLACEHOLDER)
                .size()
                .x
        }
    };
    let gap = space::XS;
    let time_width = text_width(time);
    if !outbound {
        return time_width;
    }
    let after_time = |mark: &str| {
        if time_width > 0.0 {
            time_width + gap + text_width(mark)
        } else {
            text_width(mark)
        }
    };
    match delivery {
        Delivery::Pending => after_time(SENDING_MARK),
        Delivery::Sent => after_time(SENT_MARK),
        Delivery::Read => after_time(READ_MARK),
        Delivery::Failed => {
            time_width.max(text_width(FAILED_MARK) + space::S + text_width("Retry"))
        }
    }
}

/// AccessKit name of a bubble. A screen reader reads this once.
///
/// A group row names the sender and the time on every message. A private
/// row names the message. The painted body, the group header, and a group
/// time are hidden, so they do not repeat this name. Text runs on the pane
/// keep partial selection. The visual sender header stays on the first row
/// of a run.
fn bubble_access_label(message: &Bubble<'_>) -> String {
    let body = if message.body.is_empty() && message.photo.is_some() {
        "Photo"
    } else {
        message.body
    };
    if message.group {
        format!("{} {} {body}", message.sender, message.layout.time)
    } else {
        body.to_owned()
    }
}

/// [`radius::BUBBLE`] on every corner. The last bubble of a run uses
/// [`radius::BUBBLE_TAIL`] on the sender side.
fn bubble_radius(outbound: bool, run_end: bool) -> egui::CornerRadius {
    let round = radius::BUBBLE;
    let tail = if run_end { radius::BUBBLE_TAIL } else { round };
    if outbound {
        egui::CornerRadius {
            nw: round,
            ne: round,
            sw: round,
            se: tail,
        }
    } else {
        egui::CornerRadius {
            nw: round,
            ne: round,
            sw: tail,
            se: round,
        }
    }
}

/// Rows in the draft. `str::lines` drops a trailing blank row from Shift+Enter.
fn compose_line_count(text: &str) -> usize {
    text.split('\n').count()
}

/// Height of the field row: text, field padding, focus ring, at least the
/// round Send button.
///
/// `ui.horizontal` only offers `interact_size` of height, and a vertical
/// scroll area otherwise stays at its 64px minimum. Compose uses this as
/// both `min_scrolled_height` and `max_height`, so the bar tracks the text.
fn compose_row_height(row_height: f32, rows: usize) -> f32 {
    let rows = rows.clamp(1, COMPOSE_MAX_ROWS) as f32;
    let pad = space::M * 2.0;
    let field = row_height * rows + pad + FIELD_STROKE_MAX * 2.0;
    field.max(SEND_MIN_HEIGHT)
}

/// Height the message list leaves for the compose bar.
///
/// The outer frame and the field frame each add `space::M` above and below.
/// The field stroke is reserved at its focused width. The Send button is at
/// least [`SEND_MIN_HEIGHT`]. `item_spacing_y` is the gap egui inserts
/// before the bar.
fn compose_reserve(row_height: f32, rows: usize, item_spacing_y: f32) -> f32 {
    let pad = space::M * 2.0;
    pad + compose_row_height(row_height, rows) + item_spacing_y
}

/// Multiline compose. Enter sends; Shift+Enter adds a line.
fn compose(
    ui: &mut egui::Ui,
    snapshot: &View<'_>,
    hints: &mut Hints,
    max_height: f32,
    out: &mut Vec<Intent>,
) {
    let compose_id = egui::Id::unique("thread-compose");
    // The chat this frame drew. Drafts and sends name it, so a click on
    // another chat in the same frame cannot move them (PR #48 review).
    let chat = snapshot
        .selected_conversation
        .clone()
        .map(|id| (snapshot.selected_protocol, id, snapshot.selected_topic));
    let closed = snapshot.topic_closed();
    // A closed topic already disables this field. Sending is off after the
    // phone removes the device, so the field uses that same switch.
    let sending_off = snapshot.selected_protocol == ProtocolId::Signal
        && (snapshot.signal_device_removed() || snapshot.signal_read_only_reason().is_some());
    let compose_open = !closed && !sending_off;
    // Dropped and pasted photos and videos go to the tray of this thread.
    // This runs before the field, so a paste of files adds no text.
    if compose_open
        && snapshot.sends_media(snapshot.selected_protocol)
        && let Some(thread) = chat.as_ref()
    {
        let focused = ui.memory(|memory| memory.has_focus(compose_id));
        super::attach::take_input(ui, thread, focused, out);
    }
    if hints.take_focus_compose() {
        ui.memory_mut(|memory| memory.request_focus(compose_id));
    }
    if ui.memory(|memory| memory.has_focus(compose_id)) {
        let (enter, shift, other) = ui.input(|input| {
            let modifiers = input.modifiers;
            (
                input.key_pressed(egui::Key::Enter),
                modifiers.shift,
                modifiers.ctrl || modifiers.alt || modifiers.command || modifiers.mac_cmd,
            )
        });
        // Eat plain Enter before the text field sees it, so it does not add a line.
        if enter && !other && !shift && compose_open {
            if let Some((protocol, conversation_id, topic)) = chat.clone() {
                out.push(Intent::SendDraft {
                    protocol,
                    conversation_id,
                    topic,
                });
            }
            ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
        }
    }
    let palette = theme::palette(ui);
    let y = ui.cursor().min.y;
    ui.painter().hline(
        ui.max_rect().x_range(),
        y,
        egui::Stroke::new(1.0, palette.border),
    );
    if closed {
        ui.add_space(space::S);
        ui.horizontal(|ui| {
            ui.add_space(THREAD_INSET);
            ui.label(RichText::new("This topic is closed.").color(palette.text2));
        });
    }
    let focused = ui.memory(|memory| memory.has_focus(compose_id));
    let ring = motion::toggle(ui.ctx(), compose_id.with("ring"), focused, motion::STATE);
    let field = egui::Frame::new()
        .fill(palette.input)
        .stroke(egui::Stroke::new(
            egui::lerp(1.0..=FIELD_STROKE_MAX, ring),
            motion::mix(palette.border_strong, palette.accent, ring),
        ))
        .corner_radius(radius::FIELD)
        .inner_margin(egui::Margin::symmetric(space::L as i8, space::M as i8));
    egui::Frame::new()
        .fill(palette.bg)
        .inner_margin(egui::Margin::symmetric(THREAD_INSET as i8, space::M as i8))
        .show(ui, |ui| {
            if let Some(thread) = chat.as_ref() {
                super::attach::tray(ui, snapshot, thread, out);
            }
            ui.horizontal(|ui| {
                let width = (ui.available_width() - SEND_MIN_HEIGHT - space::S).max(0.0);
                let visible = compose_row_height(
                    ui.text_style_height(&egui::TextStyle::Body),
                    compose_line_count(&snapshot.compose),
                )
                .min(max_height);
                egui::ScrollArea::vertical()
                    .id_salt("thread-compose-scroll")
                    .max_height(visible)
                    .min_scrolled_height(visible)
                    .max_width(width)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        let hint = snapshot.compose_hint();
                        if let Some(text) = edited(ui, &snapshot.compose, |text| {
                            egui::TextEdit::multiline(text)
                                .id(compose_id)
                                .frame(field)
                                .desired_rows(1)
                                .desired_width(width)
                                .interactive(compose_open)
                                .hint_text(RichText::new(hint.clone()).color(palette.text3))
                        }) && compose_open
                            && let Some((protocol, conversation_id, topic)) = chat.clone()
                        {
                            out.push(Intent::SetDraft {
                                protocol,
                                conversation_id,
                                topic,
                                text: text.into(),
                            });
                        }
                    });
                let can_send = snapshot.can_send();
                let ready =
                    motion::toggle(ui.ctx(), compose_id.with("send"), can_send, motion::STATE);
                let label =
                    motion::mix(send_label(false, palette), send_label(true, palette), ready);
                let fill = motion::mix(send_fill(false, palette), send_fill(true, palette), ready);
                if ui
                    .add_enabled_ui(snapshot.can_send(), |ui| send_button(ui, fill, label))
                    .inner
                    .clicked()
                    && let Some((protocol, conversation_id, topic)) = chat.clone()
                {
                    out.push(Intent::SendDraft {
                        protocol,
                        conversation_id,
                        topic,
                    });
                }
            });
        });
}

/// The round Send button: an arrow on `fill`. Its name for a screen reader is
/// "Send". A disabled button does not answer clicks (the caller disables it).
fn send_button(ui: &mut egui::Ui, fill: egui::Color32, ink: egui::Color32) -> egui::Response {
    let icon_id = egui::IdSalt::new("send-icon");
    let enabled = ui.is_enabled();
    let shown = egui::Button::new(egui::Atom::custom(icon_id, egui::vec2(18.0, 18.0)))
        .fill(fill)
        .corner_radius(egui::CornerRadius::same(radius::PILL))
        .min_size(egui::vec2(SEND_MIN_HEIGHT, SEND_MIN_HEIGHT))
        .atom_ui(ui);
    if let Some(rect) = shown.rect(icon_id) {
        icons::arrow_up(ui.painter(), rect.center(), 18.0, ink);
    }
    let response = shown.response;
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, "Send"));
    response
}

/// Fill of the Send button. Disabled uses `surface`, not a faded accent.
fn send_fill(can_send: bool, palette: &theme::Palette) -> egui::Color32 {
    if can_send {
        palette.accent
    } else {
        palette.surface
    }
}

/// Label color of the Send button.
fn send_label(can_send: bool, palette: &theme::Palette) -> egui::Color32 {
    if can_send {
        palette.on_accent
    } else {
        palette.text3
    }
}

#[cfg(test)]
mod tests {
    use super::{InboxAnchor, account_label, anchored_inbox_offset, badge_text, row_scrolls};

    #[test]
    fn only_one_to_three_emoji_lose_their_bubble() {
        use super::only_emoji;

        assert!(only_emoji("👍"));
        assert!(only_emoji("👍 🎉"));
        assert!(only_emoji("👍👍👍"));
        assert!(only_emoji("❤️"));
        assert!(only_emoji("👋🏽"), "a skin tone belongs to its emoji");
        assert!(
            only_emoji("👨\u{200D}👩\u{200D}👧"),
            "a family is one emoji"
        );
        assert!(!only_emoji("👍👍👍👍"), "four is a line of text");
        assert!(!only_emoji("ok 👍"));
        assert!(!only_emoji("…"));
        assert!(!only_emoji("1"));
        assert!(!only_emoji(""));
        assert!(!only_emoji("   "));
    }

    #[test]
    fn both_scroll_requests_scroll_the_highlight() {
        assert!(row_scrolls(false, true, true, true));
        assert!(!row_scrolls(true, false, true, true));
        assert!(row_scrolls(true, false, true, false));
        assert!(!row_scrolls(false, true, true, false));
    }

    fn anchor(ids: &[&str], offset: f32, stride: f32, held: &str) -> InboxAnchor {
        InboxAnchor {
            ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            offset,
            stride,
            anchor: Some(held.to_owned()),
        }
    }

    #[test]
    fn the_top_of_the_inbox_does_not_anchor_and_a_scrolled_row_does() {
        let before = ["a", "b", "c"];
        let after = ["c", "a", "b"];
        assert!(
            anchored_inbox_offset(&anchor(&before, 0.0, 48.0, "b"), &owned(&after)).is_none(),
            "at the top the new chat appears there and the other rows shift down"
        );
        let held = anchored_inbox_offset(&anchor(&before, 120.0, 48.0, "b"), &owned(&after))
            .expect("scrolled");
        assert!((held - 168.0).abs() < 0.01, "b moved down one row: {held}");
        assert!(
            anchored_inbox_offset(&anchor(&before, 120.0, 48.0, "b"), &owned(&before)).is_none(),
            "the same order does not scroll"
        );
        assert!(
            anchored_inbox_offset(&anchor(&before, 120.0, 48.0, "gone"), &owned(&after)).is_none()
        );
    }

    fn owned(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn a_zero_width_frame_keeps_the_previous_thread_width() {
        use thinwire_protocol::Delivery;

        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            ..Default::default()
        };
        let mut stored = 0.0;
        let mut meta = 0.0;
        let mut output = ctx.run_ui(input.clone(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                stored = super::laid_out_width(ui, "thread-width");
                meta = super::meta_column_width(ui, "10:00", Delivery::Sent, false);
            });
        });
        output.textures_delta.clear();
        let mut fallback = 0.0;
        let mut output = ctx.run_ui(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.set_max_width(0.0);
                fallback = super::laid_out_width(ui, "thread-width");
            });
        });
        output.textures_delta.clear();
        assert!(stored >= 48.0, "the first frame records a real width");
        assert_eq!(fallback, stored, "a 0-width frame reuses that width");
        assert!(
            meta > 0.0 && meta < 72.0,
            "the time column is only as wide as the time"
        );
    }
    use thinwire_protocol::AdapterStatus;

    #[test]
    fn older_paging_asks_at_the_top_and_keeps_the_row_on_screen() {
        use super::{OLDER_TRIGGER, OlderStep, ThreadMemo, older_step};

        let rows = ["t:1:50", "t:1:51"];
        let has = |id: &str| rows.contains(&id) || id == "t:1:40";
        // A chat that just opened: no ask, even at offset 0.
        let fresh = ThreadMemo::default();
        assert_eq!(
            older_step(&fresh, Some("t:1:50"), has, 0.0, 800.0, false),
            OlderStep {
                at_top: false,
                restore: None,
                in_zone: false,
            }
        );
        // Same rows as the last frame, the view enters the top: ask.
        let seen = ThreadMemo {
            first_id: Some("t:1:50".into()),
            content_height: 800.0,
            restore: None,
            in_zone: false,
        };
        assert!(older_step(&seen, Some("t:1:50"), has, OLDER_TRIGGER, 800.0, false).at_top);
        assert!(
            !older_step(
                &seen,
                Some("t:1:50"),
                has,
                OLDER_TRIGGER + 1.0,
                800.0,
                false
            )
            .at_top
        );
        // 300 points of older rows went in above: move down by 300.
        let step = older_step(&seen, Some("t:1:40"), has, 4.0, 1100.0, false);
        assert_eq!(
            step,
            OlderStep {
                at_top: false,
                restore: Some(304.0),
                in_zone: false,
            }
        );
        // The pass that applies the offset does not ask again.
        let restoring = ThreadMemo {
            first_id: Some("t:1:40".into()),
            content_height: 1100.0,
            restore: Some(304.0),
            in_zone: false,
        };
        assert!(!older_step(&restoring, Some("t:1:40"), has, 304.0, 1100.0, false).at_top);
        // Another chat's rows (the old top row is gone): no offset change.
        assert_eq!(
            older_step(&seen, Some("t:2:9"), |id| id == "t:2:9", 0.0, 500.0, false).restore,
            None
        );
        // No rows: nothing.
        assert!(!older_step(&seen, None, has, 0.0, 0.0, false).at_top);
    }

    #[test]
    fn a_thread_that_cannot_leave_the_zone_keeps_asking_at_the_top() {
        use super::{ThreadMemo, older_step};

        let has = |id: &str| id == "t:1:50";
        let memo = ThreadMemo {
            first_id: Some("t:1:50".into()),
            content_height: 300.0,
            restore: None,
            in_zone: true,
        };
        // It cannot scroll, so it never leaves the zone: still asks (#67).
        assert!(older_step(&memo, Some("t:1:50"), has, 0.0, 300.0, true).at_top);
        // 10 pt taller than the view: the largest offset is 10 pt, inside
        // the zone, so it gets the timed retry too.
        use super::{OLDER_TRIGGER, cannot_leave_zone};
        assert!(cannot_leave_zone(300.0, 300.0));
        assert!(cannot_leave_zone(310.0, 300.0));
        assert!(cannot_leave_zone(300.0 + OLDER_TRIGGER, 300.0));
        assert!(!cannot_leave_zone(301.0 + OLDER_TRIGGER, 300.0));
        let taller = cannot_leave_zone(310.0, 300.0);
        assert!(older_step(&memo, Some("t:1:50"), has, 10.0, 310.0, taller).at_top);
        // A thread that can scroll out waits for leave and return.
        assert!(!older_step(&memo, Some("t:1:50"), has, 0.0, 300.0, false).at_top);
        let ui = include_str!("ui.rs");
        let thread = &ui[ui.find("fn thread(").expect("thread")..];
        let thread = &thread[..thread.find("\nfn ").expect("next")];
        assert!(
            thread.contains("snapshot.older_can_ask()"),
            "the core paces it"
        );
    }

    #[test]
    fn repaints_at_the_top_ask_once_and_a_new_ask_needs_leave_and_return() {
        use super::{ThreadMemo, older_step};

        let has = |id: &str| id == "t:1:50";
        let mut memo = ThreadMemo {
            first_id: Some("t:1:50".into()),
            content_height: 800.0,
            restore: None,
            in_zone: false,
        };
        let frame = |memo: &mut ThreadMemo, offset: f32| {
            let step = older_step(memo, Some("t:1:50"), has, offset, 800.0, false);
            memo.in_zone = step.in_zone;
            memo.restore = step.restore;
            step.at_top
        };
        // 30 repaints at the top, and the page brought nothing: one ask.
        let asks = (0..30).filter(|_| frame(&mut memo, 0.0)).count();
        assert_eq!(asks, 1, "no request loop (#61 review)");
        // Leave the zone, then come back: one more ask.
        assert!(!frame(&mut memo, 200.0));
        assert!(frame(&mut memo, 0.0));
        assert!(!frame(&mut memo, 0.0));
    }

    #[test]
    fn the_thread_shows_older_rows_and_asks_the_core() {
        let ui = include_str!("ui.rs");
        let thread = &ui[ui.find("fn thread(").expect("thread")..];
        let thread = &thread[..thread.find("\nfn ").expect("next")];
        assert!(thread.contains("\"Loading older messages…\""));
        assert!(thread.contains("\"Start of chat\""));
        assert!(
            thread.contains("snapshot.older_note()"),
            "the chat note shows"
        );
        assert!(thread.contains("out.push(Intent::LoadOlderMessages {"));
        assert!(
            thread.contains("older == OlderState::Idle"),
            "one request at a time"
        );
        assert!(thread.contains("vertical_scroll_offset(offset)"));
        assert!(thread.contains("ui.ctx().request_repaint()"));
        // Each pass dispatches its intents once, so no pass may be redone:
        // a typed character must reach the core exactly once (PR #61 review).
        for src in [ui, include_str!("mod.rs")] {
            assert!(!src.contains(concat!("request_", "discard(")));
        }
        let app = include_str!("mod.rs");
        let pass = &app[app.find("fn ui(&mut self").expect("ui")..];
        let pass = &pass[..pass.find("\n    }\n").expect("end")];
        assert_eq!(
            pass.matches("self.core.dispatch(intent)").count(),
            1,
            "one dispatch per pass"
        );
    }

    #[test]
    fn compose_reserve_counts_the_frame_padding() {
        use super::{FIELD_STROKE_MAX, SEND_MIN_HEIGHT, compose_reserve, compose_row_height};
        use crate::app::theme::space;

        let row = 20.0;
        let pad = space::M * 2.0;
        let spacing = 6.0;
        let field = row + pad + FIELD_STROKE_MAX * 2.0;
        assert_eq!(compose_row_height(row, 1), field.max(SEND_MIN_HEIGHT));
        assert_eq!(
            compose_reserve(row, 1, spacing),
            pad + field.max(SEND_MIN_HEIGHT) + spacing
        );
        assert!(compose_reserve(row, 1, spacing) > row + 32.0);
        assert!(compose_reserve(1.0, 1, 0.0) >= SEND_MIN_HEIGHT + pad);
        let ui = include_str!("ui.rs");
        let thread = &ui[ui.find("fn thread(").expect("thread")..];
        let thread = &thread[..thread.find("\nfn ").expect("next")];
        assert!(thread.contains("compose_reserve("));
        assert!(thread.contains("item_spacing"));
        assert!(!thread.contains("+ 32.0"));
        let compose = &ui[ui.find("fn compose(").expect("compose")..];
        let compose = &compose[..compose.find("\nfn ").expect("next")];
        assert!(compose.contains("min_scrolled_height("));
        assert!(compose.contains("compose_row_height("));
        assert!(compose.contains("compose_line_count("));
        assert!(compose.contains("FIELD_STROKE_MAX"));
        assert!(compose.contains("SEND_MIN_HEIGHT"));
        assert!(thread.contains("compose_line_count("));
        assert!(!thread.contains(".lines().count()"));
        assert!(!compose.contains(".lines().count()"));
    }

    #[test]
    fn compose_line_count_keeps_a_trailing_blank_row() {
        use super::compose_line_count;

        assert_eq!(compose_line_count(""), 1);
        assert_eq!(compose_line_count("hi"), 1);
        assert_eq!(compose_line_count("a\nb"), 2);
        assert_eq!("hi\n".lines().count(), 1);
        assert_eq!(compose_line_count("hi\n"), 2);
        assert_eq!(compose_line_count("a\nb\n"), 3);
    }

    fn compose_body(rows: usize) -> String {
        (0..rows)
            .map(|row| format!("line {row}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A long thread hits the message scroll's max height. The bar, including
    /// the focused ring, still ends inside that panel.
    #[test]
    fn compose_bar_stays_inside_the_reserved_clip() {
        use super::{
            COMPOSE_MAX_ROWS, FIELD_STROKE_MAX, SEND_MIN_HEIGHT, compose, compose_line_count,
            compose_reserve, compose_row_height,
        };
        use crate::app::theme::{self, space};
        use thinwire_core::state::Snapshot;

        let ctx = egui::Context::default();
        theme::install(&ctx);
        ctx.set_theme(egui::Theme::Dark);
        let mut snapshot = Snapshot::new();
        let store = thinwire_core::secrets::SecretStore::memory();
        let settings = thinwire_core::settings::Settings::load_from(
            std::env::temp_dir().join("thinwire-ui-compose-test.toml"),
        );
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 700.0),
            )),
            ..Default::default()
        };

        for rows in 1..=COMPOSE_MAX_ROWS {
            snapshot.compose = compose_body(rows);
            let mut output = ctx.run_ui(input.clone(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let spacing = ui.spacing().item_spacing.y;
                    assert_eq!(spacing, 6.0);
                    let row_height = ui.text_style_height(&egui::TextStyle::Body);
                    let reserve = compose_reserve(row_height, rows, spacing);
                    let top = ui.cursor().min.y;
                    ui.memory_mut(|memory| {
                        memory.request_focus(egui::Id::unique("thread-compose"));
                    });
                    compose(
                        ui,
                        &thinwire_core::View::from_parts(&snapshot, &store, &settings),
                        &mut super::Hints::default(),
                        compose_row_height(row_height, COMPOSE_MAX_ROWS),
                        &mut Vec::new(),
                    );
                    let height = ui.cursor().min.y - top - spacing;
                    assert!(
                        height + spacing <= reserve + 0.05,
                        "{rows} rows: bar {height} + gap {spacing} exceeds reserve {reserve}"
                    );
                    assert!(height + 0.05 >= SEND_MIN_HEIGHT);
                    if rows == 1 {
                        let old = row_height + 32.0;
                        assert!(
                            height + spacing > old,
                            "old reserve {old} still covers the bar ({height} + {spacing})"
                        );
                    }
                    // The scroll cap must hold the field, including its margin.
                    // A short cap clips lines 4 and 5.
                    let cap = compose_row_height(row_height, rows);
                    let mut body = snapshot.compose.clone();
                    let palette = theme::palette(ui);
                    let field = egui::Frame::new()
                        .inner_margin(egui::Margin::symmetric(space::M as i8, space::M as i8))
                        .stroke(egui::Stroke::new(FIELD_STROKE_MAX, palette.accent));
                    let edit = ui.add(
                        egui::TextEdit::multiline(&mut body)
                            .id_salt(("compose-cap", rows))
                            .frame(field)
                            .desired_rows(rows)
                            .desired_width(320.0),
                    );
                    assert!(
                        edit.rect.height() <= cap + 0.05,
                        "{rows} rows: field {} exceeds cap {cap}",
                        edit.rect.height()
                    );
                });
            });
            output.textures_delta.clear();
        }

        for rows in [1, COMPOSE_MAX_ROWS] {
            snapshot.compose = compose_body(rows);
            let mut output = ctx.run_ui(input.clone(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let origin = ui.cursor().min;
                    let rect = egui::Rect::from_min_size(origin, egui::vec2(640.0, 360.0));
                    ui.scope_builder(
                        egui::UiBuilder::new()
                            .max_rect(rect)
                            .id_salt(("compose-clip", rows)),
                        |ui| {
                            ui.set_clip_rect(rect);
                            let spacing = ui.spacing().item_spacing.y;
                            let row_height = ui.text_style_height(&egui::TextStyle::Body);
                            let reserve = compose_reserve(
                                row_height,
                                compose_line_count(&snapshot.compose),
                                spacing,
                            );
                            let available = ui.available_height();
                            let before = ui.cursor().min.y;
                            egui::ScrollArea::vertical()
                                .id_salt(("reserve-clip", rows))
                                .auto_shrink([false, true])
                                .stick_to_bottom(true)
                                .max_height(available - reserve)
                                .show(ui, |ui| {
                                    ui.set_min_height(8_000.0);
                                });
                            let scroll_height = ui.cursor().min.y - before - spacing;
                            assert!(
                                (scroll_height - (available - reserve)).abs() < 1.0,
                                "{rows} rows: scroll {scroll_height} did not hit max height"
                            );
                            ui.memory_mut(|memory| {
                                memory.request_focus(egui::Id::unique("thread-compose"));
                            });
                            compose(
                                ui,
                                &thinwire_core::View::from_parts(&snapshot, &store, &settings),
                                &mut super::Hints::default(),
                                compose_row_height(row_height, COMPOSE_MAX_ROWS),
                                &mut Vec::new(),
                            );
                            let compose_bottom = ui.cursor().min.y - spacing;
                            assert!(
                                compose_bottom <= rect.bottom() + 0.05,
                                "{rows} rows: compose ends at {compose_bottom}, clip at {}",
                                rect.bottom()
                            );
                        },
                    );
                });
            });
            output.textures_delta.clear();
        }
    }

    #[test]
    fn send_button_colors_change_with_the_enabled_state() {
        use super::{send_fill, send_label};
        use crate::app::theme::Palette;

        for palette in [Palette::DARK, Palette::LIGHT] {
            assert_ne!(send_fill(true, &palette), send_fill(false, &palette));
            assert_ne!(send_label(true, &palette), send_label(false, &palette));
            assert_eq!(send_fill(false, &palette), palette.surface);
            assert_eq!(send_label(false, &palette), palette.text3);
        }
    }

    #[test]
    fn badge_text_caps_above_99() {
        assert_eq!(badge_text(0), None);
        assert_eq!(badge_text(1).as_deref(), Some("1"));
        assert_eq!(badge_text(12).as_deref(), Some("12"));
        assert_eq!(badge_text(99).as_deref(), Some("99"));
        assert_eq!(badge_text(100).as_deref(), Some("99+"));
        assert_eq!(badge_text(u32::MAX).as_deref(), Some("99+"));
    }

    #[test]
    fn bubble_tail_is_the_sender_side_bottom_corner() {
        use super::bubble_radius;
        use crate::app::theme::radius;

        let inbound_end = bubble_radius(false, true);
        assert_eq!(inbound_end.sw, radius::BUBBLE_TAIL);
        assert_eq!(inbound_end.se, radius::BUBBLE);
        let outbound_end = bubble_radius(true, true);
        assert_eq!(outbound_end.se, radius::BUBBLE_TAIL);
        assert_eq!(outbound_end.sw, radius::BUBBLE);
        let mid = bubble_radius(false, false);
        assert_eq!(mid.sw, radius::BUBBLE);
        assert_eq!(mid.se, radius::BUBBLE);
    }

    #[test]
    fn account_label_maps_adapter_status() {
        assert_eq!(account_label(AdapterStatus::Ready), "Online");
        assert_eq!(account_label(AdapterStatus::Connecting), "Connecting…");
        assert_eq!(account_label(AdapterStatus::Stubbed), "Not signed in");
        assert_eq!(account_label(AdapterStatus::Refused), "Not signed in");
        assert_eq!(account_label(AdapterStatus::Error), "Not signed in");
    }

    #[test]
    fn chip_says_not_signed_in_before_login_starts() {
        use super::chip_label;
        use thinwire_core::state::{AuthScreen, Snapshot};
        use thinwire_protocol::ProtocolId;

        let mut snapshot = Snapshot::new();
        // No saved session: the start-up resume ends at once. A build with
        // `telegram-tdlib` waits for this before it says "Not signed in".
        snapshot.poll_resume(&thinwire_core::secrets::SecretStore::memory());
        snapshot
            .accounts
            .iter_mut()
            .find(|account| account.caps.id == ProtocolId::Telegram)
            .expect("telegram")
            .status = AdapterStatus::Connecting;
        let account = snapshot
            .accounts
            .iter()
            .find(|account| account.caps.id == ProtocolId::Telegram)
            .expect("telegram");
        assert_eq!(chip_label(&snapshot, account), "Not signed in");
        snapshot.auth = AuthScreen::TelegramConnecting;
        let account = snapshot
            .accounts
            .iter()
            .find(|account| account.caps.id == ProtocolId::Telegram)
            .expect("telegram");
        assert_eq!(chip_label(&snapshot, account), "Connecting…");
    }

    /// A store migration keeps the row on Connecting. A denied keychain
    /// read is its own chip, even before the account is linked.
    #[test]
    fn chip_shows_connecting_during_migration_and_the_denied_key() {
        use super::chip_label;
        use thinwire_core::state::Snapshot;
        use thinwire_protocol::{AccountState, ProtocolId, StoreAccessReason};

        let mut snapshot = Snapshot::new();
        snapshot.poll_resume(&thinwire_core::secrets::SecretStore::memory());
        {
            let account = snapshot
                .accounts
                .iter_mut()
                .find(|account| account.caps.id == ProtocolId::WhatsApp)
                .expect("whatsapp");
            account.state = AccountState::Linking;
            account.status = AdapterStatus::Stubbed;
        }
        let account = snapshot
            .accounts
            .iter()
            .find(|account| account.caps.id == ProtocolId::WhatsApp)
            .expect("whatsapp");
        assert_eq!(chip_label(&snapshot, account), "Connecting…");

        {
            let account = snapshot
                .accounts
                .iter_mut()
                .find(|account| account.caps.id == ProtocolId::Signal)
                .expect("signal");
            account.state = AccountState::Linking;
            account.status = AdapterStatus::Error;
            account.detail = thinwire_protocol::KEYCHAIN_ACCESS_DENIED.into();
            account.store_access = Some(StoreAccessReason::KeychainDenied);
        }
        let account = snapshot
            .accounts
            .iter()
            .find(|account| account.caps.id == ProtocolId::Signal)
            .expect("signal");
        assert_eq!(chip_label(&snapshot, account), "Connection problem");
        assert_eq!(account.detail, thinwire_protocol::KEYCHAIN_ACCESS_DENIED);

        {
            let account = snapshot
                .accounts
                .iter_mut()
                .find(|account| account.caps.id == ProtocolId::Signal)
                .expect("signal");
            account.state = AccountState::Linked;
            account.status = AdapterStatus::Connecting;
            account.store_access = None;
            account.detail = "Opening the saved session.".into();
        }
        let account = snapshot
            .accounts
            .iter()
            .find(|account| account.caps.id == ProtocolId::Signal)
            .expect("signal");
        assert_eq!(chip_label(&snapshot, account), "Connecting…");
    }

    /// Retry restarts the key read. Link on a wrong key is the removed-device
    /// relink (`SignalIntent::Relink`), which starts the QR flow.
    #[test]
    fn a_wrong_key_link_button_starts_the_relink() {
        use super::{store_row_button, store_row_hover, store_row_intent};
        use thinwire_core::state::AccountRow;
        use thinwire_core::{Intent, SignalIntent, WhatsAppIntent};
        use thinwire_protocol::{
            AccountState, AdapterStatus, ProtocolId, StoreAccessReason, catalog,
        };

        let signal = catalog()
            .into_iter()
            .find(|caps| caps.id == ProtocolId::Signal)
            .expect("signal");
        let whatsapp = catalog()
            .into_iter()
            .find(|caps| caps.id == ProtocolId::WhatsApp)
            .expect("whatsapp");
        let mut account = AccountRow {
            caps: signal,
            status: AdapterStatus::Error,
            detail: thinwire_protocol::signal_session_kept().into(),
            state: AccountState::Unlinked,
            helper: None,
            signal_retained: None,
            store_access: Some(StoreAccessReason::WrongKey),
        };
        assert_eq!(
            store_row_intent(&account),
            Some(Intent::Signal(SignalIntent::Relink))
        );
        assert_eq!(store_row_button(&account), "Link");
        account.store_access = Some(StoreAccessReason::KeychainDenied);
        account.state = AccountState::Linked;
        assert_eq!(
            store_row_intent(&account),
            Some(Intent::RetryStoreKey(ProtocolId::Signal))
        );
        assert_eq!(store_row_button(&account), "Retry");
        account.caps = whatsapp;
        account.store_access = Some(StoreAccessReason::WrongKey);
        assert_eq!(
            store_row_intent(&account),
            Some(Intent::WhatsApp(WhatsAppIntent::OpenRiskGate))
        );
        account.store_access = Some(StoreAccessReason::MoveFailed);
        account.state = AccountState::Linked;
        account.detail = thinwire_protocol::whatsapp_key_wrong_kept().into();
        assert_eq!(
            store_row_intent(&account),
            Some(Intent::RetryStoreKey(ProtocolId::WhatsApp))
        );
        assert_eq!(store_row_button(&account), "Retry");
        assert_eq!(
            store_row_hover(&account),
            "Tries to move the saved session aside again. Nothing is deleted."
        );
        account.caps = signal;
        account.detail = thinwire_protocol::signal_key_wrong_kept().into();
        assert_eq!(
            store_row_intent(&account),
            Some(Intent::RetryStoreKey(ProtocolId::Signal))
        );
    }

    /// The failed-move row draws Retry. The click sends `RetryStoreKey`.
    #[test]
    fn a_failed_move_row_draws_retry() {
        use eframe::egui::accesskit::Role;
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;
        use std::cell::RefCell;
        use std::rc::Rc;
        use thinwire_core::Intent;
        use thinwire_core::state::AccountRow;
        use thinwire_protocol::{
            AccountState, AdapterStatus, ProtocolId, StoreAccessReason, catalog,
        };

        use super::{store_protection_line, store_row_hover};
        use crate::app::theme;

        let signal = catalog()
            .into_iter()
            .find(|caps| caps.id == ProtocolId::Signal)
            .expect("signal");
        let account = AccountRow {
            caps: signal,
            status: AdapterStatus::Error,
            detail: thinwire_protocol::signal_key_wrong_kept().into(),
            state: AccountState::Linked,
            helper: None,
            signal_retained: None,
            store_access: Some(StoreAccessReason::MoveFailed),
        };
        assert_eq!(
            store_row_hover(&account),
            "Tries to move the saved session aside again. Nothing is deleted."
        );
        let intents = Rc::new(RefCell::new(Vec::new()));
        let intents_ui = Rc::clone(&intents);
        let mut harness = Harness::builder()
            .with_size(egui::vec2(520.0, 96.0))
            .build_ui(move |ui| {
                theme::install(ui.ctx());
                let mut out = Vec::new();
                store_protection_line(ui, &account, &mut out);
                intents_ui.borrow_mut().extend(out);
            });
        harness.run_steps(3);
        harness.get_by_role_and_label(Role::Button, "Retry").click();
        harness.run_steps(2);
        assert_eq!(
            intents.borrow().as_slice(),
            [Intent::RetryStoreKey(ProtocolId::Signal)]
        );
    }

    /// Shell plan 11: a linked account with an error is not "Not signed in".
    #[test]
    fn chip_keeps_a_linked_account_signed_in_on_an_error() {
        use super::chip_label;
        use thinwire_core::state::Snapshot;
        use thinwire_protocol::{AccountState, ProtocolId};

        let mut snapshot = Snapshot::new();
        let row = snapshot
            .accounts
            .iter_mut()
            .find(|account| account.caps.id == ProtocolId::Telegram)
            .expect("telegram");
        row.state = AccountState::Linked;
        row.status = AdapterStatus::Error;
        let account = snapshot.accounts[0].clone();
        assert_eq!(chip_label(&snapshot, &account), "Connection problem");
    }

    #[test]
    fn a_linked_account_has_no_not_ready_line() {
        use super::account_footnote;
        use thinwire_protocol::ProtocolId;

        assert_eq!(
            account_footnote(ProtocolId::Discord, true),
            None,
            "a linked Discord bot is not 'not ready'"
        );
        assert_eq!(
            account_footnote(ProtocolId::Discord, false),
            Some("not ready — no login UI this beat")
        );
        assert_eq!(account_footnote(ProtocolId::Telegram, false), None);
        assert_eq!(account_footnote(ProtocolId::Slack, true), None);
    }

    #[test]
    fn inbox_stays_blank_before_sign_in() {
        use thinwire_core::state::{NO_CHATS_YET, Snapshot};

        let mut snapshot = Snapshot::new();
        assert_eq!(snapshot.inbox_placeholder(), None);
        snapshot.apply(thinwire_protocol::AdapterEvent::Account {
            protocol: thinwire_protocol::ProtocolId::Telegram,
            state: thinwire_protocol::AccountState::Linked,
        });
        assert_eq!(snapshot.inbox_placeholder(), Some(NO_CHATS_YET));
    }

    #[test]
    fn add_account_hides_when_telegram_is_linked() {
        use thinwire_core::state::Snapshot;

        let mut snapshot = Snapshot::new();
        assert!(snapshot.can_add_account());
        snapshot.telegram_authorized = true;
        assert!(!snapshot.can_add_account());
    }

    #[test]
    fn idle_status_and_tdlib_lines_hide_the_strip() {
        use super::{is_idle_status, public_status, status_strip_visible};
        use thinwire_core::state::Snapshot;

        assert!(is_idle_status(""));
        assert!(is_idle_status("Sign in with Telegram to get started."));
        assert!(is_idle_status("Telegram is ready. Loading the chat list."));
        assert!(is_idle_status("Recent messages loaded."));
        assert!(!is_idle_status("Sending…"));
        assert!(!is_idle_status("Refreshing…"));
        assert_eq!(
            public_status("TDLib unavailable in this build. No live session."),
            None
        );

        let mut snapshot = Snapshot::new();
        snapshot.telegram_authorized = true;
        snapshot.status_text = "Recent messages loaded.".into();
        assert!(
            !status_strip_visible(&snapshot, None),
            "a signed-in thread with a quiet status draws no strip"
        );
        snapshot.status_text = "Sending…".into();
        assert!(status_strip_visible(&snapshot, None));

        // A note of the selected protocol shows the strip too.
        snapshot.status_text = "Recent messages loaded.".into();
        snapshot.apply(thinwire_protocol::AdapterEvent::Notice {
            protocol: snapshot.selected_protocol,
            text: "Read-only channel.".into(),
        });
        assert!(status_strip_visible(&snapshot, None));
    }

    #[test]
    fn a_ready_status_such_as_message_sent_is_idle() {
        use super::status_strip_visible;
        use thinwire_core::state::Snapshot;
        use thinwire_protocol::{AdapterEvent, AdapterStatus, ProtocolId};

        let mut snapshot = Snapshot::new();
        snapshot.telegram_authorized = true;
        let status = |status, detail: &str| AdapterEvent::Status {
            protocol: ProtocolId::Telegram,
            status,
            detail: detail.into(),
        };
        snapshot.apply(status(AdapterStatus::Ready, "Message sent."));
        assert!(snapshot.status_is_idle(), "the kind comes from Ready");
        assert!(!status_strip_visible(&snapshot, None), "no busy look");

        // A local busy line replaces it: shown again.
        snapshot.status_text = "Sending…".into();
        assert!(!snapshot.status_is_idle());
        assert!(status_strip_visible(&snapshot, None));

        // The same text as an error stays on the strip.
        snapshot.apply(status(AdapterStatus::Error, "Message sent."));
        assert!(!snapshot.status_is_idle());
        assert!(status_strip_visible(&snapshot, None));
    }

    #[test]
    fn load_failures_stay_on_the_status_strip() {
        use super::{load_failure_text, public_status, status_strip_visible};
        use thinwire_core::state::Snapshot;

        let cases = [
            (
                "Could not load Telegram chats (TDLib 500).",
                "Could not load chats.",
            ),
            (
                "Could not load messages (TDLib 500).",
                "Could not load messages.",
            ),
            (
                "Could not mark messages read (TDLib 401).",
                "Could not mark messages read.",
            ),
        ];
        let mut snapshot = Snapshot::new();
        snapshot.telegram_authorized = true;
        for (raw, shown) in cases {
            assert_eq!(load_failure_text(raw), Some(shown));
            assert_eq!(public_status(raw), Some(shown));
            assert!(!shown.contains("TDLib"));
            snapshot.status_text = raw.into();
            assert!(
                status_strip_visible(&snapshot, None),
                "a load failure stays on the strip"
            );
        }
        assert_eq!(
            public_status("Telegram did not send the message (TDLib 500)."),
            None
        );
    }

    #[test]
    fn sign_in_line_stays_off_the_status_strip() {
        let ui = include_str!("ui.rs");
        let strip = &ui[ui.find("fn status_strip(").expect("strip")..];
        let strip = &strip[..strip.find("\nfn ").expect("next")];
        assert!(!strip.contains("stub_banner"));
        assert!(!strip.contains("Telegram is not signed in yet."));
        assert!(ui.contains("stub_banner"));
    }

    #[test]
    fn first_run_card_uses_the_spare_height() {
        let ui = include_str!("ui.rs");
        let first = &ui[ui.find("fn first_run(").expect("first")..];
        let first = &first[..first.find("\nfn ").expect("next")];
        assert!(first.contains("first-run-card-height"));
        assert!(first.contains("spare / 2.0"));
        assert!(!first.contains("0.18"));
    }

    /// #247: OS reduce-motion sets `animation_time` to 0 and the busy
    /// indicator is the still "Working…" text.
    #[test]
    fn with_reduce_motion_the_busy_indicator_is_still_working_text() {
        use eframe::egui::accesskit::Role;
        use egui_kittest::Harness;
        use egui_kittest::kittest::{By, Queryable};

        use super::busy_indicator;
        use crate::app::{motion, theme};

        let mut harness = Harness::builder()
            .with_size(egui::vec2(480.0, 48.0))
            .build_ui(|ui| {
                // `install` lands on the next pass. This pass can return
                // before the new text styles exist.
                theme::install(ui.ctx());
                motion::apply(ui.ctx(), true);
                if !ui.style().text_styles.contains_key(&theme::secondary()) {
                    return;
                }
                busy_indicator(ui, Some("Sending…"));
            });
        harness.run_steps(3);
        assert_eq!(
            harness.ctx.global_style().animation_time,
            0.0,
            "reduce-motion sets animation_time to 0"
        );
        harness.get_by_label(motion::WORKING);
        assert!(
            harness
                .query_all(By::new().role(Role::ProgressIndicator))
                .next()
                .is_none(),
            "the busy indicator has no spinner"
        );
        assert!(
            harness.query_by_label("Sending…").is_none(),
            "the still label replaces the busy line and the spinner"
        );
    }

    #[test]
    fn with_motion_the_busy_indicator_keeps_the_line_and_a_spinner() {
        use eframe::egui::accesskit::Role;
        use egui_kittest::Harness;
        use egui_kittest::kittest::{By, Queryable};

        use crate::app::{motion, theme};

        let mut harness = Harness::builder()
            .with_size(egui::vec2(480.0, 48.0))
            .build_ui(|ui| {
                theme::install(ui.ctx());
                motion::apply(ui.ctx(), false);
                if !ui.style().text_styles.contains_key(&theme::secondary()) {
                    return;
                }
                super::busy_indicator(ui, Some("Sending…"));
            });
        // A spinner keeps requesting frames.
        harness.run_steps(3);
        assert!(harness.ctx.global_style().animation_time > 0.0);
        harness.get_by_label("Sending…");
        assert!(harness.query_by_label(motion::WORKING).is_none());
        assert!(
            harness
                .query_all(By::new().role(Role::ProgressIndicator))
                .next()
                .is_some()
        );
    }

    /// #247 review: a spinner ignores `animation_time` and keeps turning.
    /// Each loading surface paints the still "Working…" text instead.
    #[test]
    fn with_reduce_motion_every_spinner_is_still_working_text() {
        use eframe::egui::accesskit::Role;
        use egui_kittest::Harness;
        use egui_kittest::kittest::{By, Queryable};
        use thinwire_core::View;
        use thinwire_core::demo::Scenario;
        use thinwire_core::secrets::SecretStore;
        use thinwire_core::settings::Settings;
        use thinwire_core::state::test_support::{
            complete_telegram, ready_with_chats, seed_override,
        };
        use thinwire_core::state::{
            AuthScreen, CenterView, InboxState, RESUME_CONNECTING, Snapshot, ThreadState,
        };
        use thinwire_protocol::AdapterEvent;
        use tokio::runtime::Builder;

        use super::{Hints, draw};
        use crate::app::{motion, startup, theme};

        let store = SecretStore::memory();
        let settings = Settings::in_memory();
        seed_override(&store);

        fn assert_still(size: [f32; 2], label: Option<&str>, mut show: impl FnMut(&mut egui::Ui)) {
            let mut harness = Harness::builder()
                .with_size(egui::vec2(size[0], size[1]))
                .build_ui(|ui| {
                    theme::install(ui.ctx());
                    motion::apply(ui.ctx(), true);
                    if !ui.style().text_styles.contains_key(&theme::secondary()) {
                        return;
                    }
                    show(ui);
                });
            harness.run_steps(3);
            assert_eq!(harness.ctx.global_style().animation_time, 0.0);
            assert!(
                harness
                    .query_all(By::new().role(Role::ProgressIndicator))
                    .next()
                    .is_none(),
                "{label:?}: no spinner"
            );
            assert!(
                harness.query_all_by_label(motion::WORKING).next().is_some(),
                "{label:?}: still text"
            );
            if let Some(label) = label {
                assert!(
                    harness.query_all_by_label(label).next().is_some(),
                    "{label} stays"
                );
            }
        }

        fn draw_snapshot(
            ui: &mut egui::Ui,
            snapshot: &Snapshot,
            store: &SecretStore,
            settings: &Settings,
        ) {
            let view = View::from_parts(snapshot, store, settings);
            let mut hints = Hints::default();
            let mut out = Vec::new();
            draw(ui, &view, &mut hints, &mut out);
        }

        assert_still(
            [440.0, 220.0],
            Some(startup::CLOSING_NOTE),
            startup::waiting,
        );

        let mut connecting = Snapshot::new();
        connecting.open_telegram(&store);
        assert!(connecting.auth_busy);
        assert_eq!(connecting.auth, AuthScreen::TelegramConnecting);
        assert_still([480.0, 640.0], Some("Connecting to Telegram…"), |ui| {
            draw_snapshot(ui, &connecting, &store, &settings);
        });

        let mut waiting_for_code = Snapshot::new();
        complete_telegram(&mut waiting_for_code, &store);
        waiting_for_code.auth = AuthScreen::TelegramPhone;
        waiting_for_code.auth_busy = true;
        assert_still([480.0, 640.0], Some("Waiting for Telegram…"), |ui| {
            draw_snapshot(ui, &waiting_for_code, &store, &settings);
        });

        let mut inbox = Snapshot::new();
        complete_telegram(&mut inbox, &store);
        assert_eq!(inbox.inbox_state(), InboxState::Loading);
        assert_still([800.0, 600.0], Some("Loading chats…"), |ui| {
            draw_snapshot(ui, &inbox, &store, &settings);
        });

        let messages = ready_with_chats(&store);
        assert_eq!(messages.thread_state(), ThreadState::Loading);
        assert_still([800.0, 600.0], Some("Loading messages…"), |ui| {
            draw_snapshot(ui, &messages, &store, &settings);
        });

        let mut resume = Snapshot::new();
        complete_telegram(&mut resume, &store);
        resume.apply(AdapterEvent::TelegramSessionEnded);
        assert_eq!(
            resume.center_view(),
            CenterView::Resuming { connecting: true }
        );
        assert_still([800.0, 600.0], Some(RESUME_CONNECTING), |ui| {
            draw_snapshot(ui, &resume, &store, &settings);
        });

        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let core = Scenario::LongChatLoadingOlder.build(runtime.handle());
        assert_still([800.0, 600.0], Some("Loading older messages…"), |ui| {
            let view = core.view();
            let mut hints = Hints::default();
            let mut out = Vec::new();
            draw(ui, &view, &mut hints, &mut out);
        });
        drop(core);
        drop(runtime);
    }

    /// The first unauthorized open. The row says it is checking. Link waits
    /// for the second open, which is the removed-device state.
    #[cfg(feature = "signal-local")]
    #[test]
    fn a_signal_link_check_shows_the_checking_line_and_no_link_button() {
        use eframe::egui::accesskit::Role;
        use egui_kittest::Harness;
        use egui_kittest::kittest::{By, Queryable};
        use thinwire_core::View;
        use thinwire_core::secrets::SecretStore;
        use thinwire_core::settings::Settings;
        use thinwire_core::state::Snapshot;
        use thinwire_protocol::{
            AccountState, AdapterEvent, AdapterStatus, ProtocolId, SIGNAL_DEVICE_REMOVED,
            SIGNAL_LINK_CHECK,
        };

        use super::{Hints, draw};
        use crate::app::theme;

        let store = SecretStore::memory();
        let settings = Settings::in_memory();
        let mut snapshot = Snapshot::new();
        snapshot.open_signal_notice();
        snapshot.acknowledge_signal_notice();
        snapshot.begin_signal_link();
        let _ = snapshot.take_commands();
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Ready,
            detail: "Signal is linked. Experimental client: expect breakage.".into(),
        });
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Connecting,
            detail: SIGNAL_LINK_CHECK.into(),
        });
        assert!(snapshot.signal_link_checking());
        assert!(!snapshot.signal_device_removed());

        let mut harness = Harness::builder()
            .with_size(egui::vec2(1100.0, 720.0))
            .build_ui(|ui| {
                theme::install(ui.ctx());
                if !ui.style().text_styles.contains_key(&theme::row_title()) {
                    return;
                }
                let view = View::from_parts(&snapshot, &store, &settings);
                let mut hints = Hints::default();
                let mut out = Vec::new();
                draw(ui, &view, &mut hints, &mut out);
            });
        harness.run_steps(4);
        let copies = harness
            .query_all(
                By::new()
                    .role(Role::Label)
                    .label(SIGNAL_LINK_CHECK)
                    .include_labels(),
            )
            .count();
        assert_eq!(copies, 1, "the checking line is on the account row");
        assert!(
            harness
                .query_all(
                    By::new()
                        .role(Role::Label)
                        .label(SIGNAL_DEVICE_REMOVED)
                        .include_labels(),
                )
                .next()
                .is_none(),
            "the removed line stays off during the check"
        );
        assert!(
            harness
                .query_all(By::new().role(Role::Button).label("Link").include_labels(),)
                .next()
                .is_none(),
            "Link waits for the second unauthorized open"
        );
    }

    /// Two previous Signal numbers. The view marks the open row, blocks
    /// sending, and labels each row by slot. The label has no phone number.
    #[cfg(feature = "signal-local")]
    #[test]
    fn previous_signal_rows_are_labeled_and_read_only() {
        use eframe::egui::accesskit::Role;
        use egui_kittest::Harness;
        use egui_kittest::kittest::{By, Queryable};
        use thinwire_core::View;
        use thinwire_core::secrets::SecretStore;
        use thinwire_core::settings::Settings;
        use thinwire_core::state::Snapshot;
        use thinwire_core::state::test_support::show_only;
        use thinwire_protocol::{
            AccountState, AdapterEvent, Conversation, ProtocolId, SIGNAL_PREVIOUS_ACCOUNT,
            signal_retained_chat_id,
        };

        use super::{Hints, chip_label, draw};
        use crate::app::theme;

        let store = SecretStore::memory();
        let settings = Settings::in_memory();
        let mut snapshot = Snapshot::new();
        show_only(&mut snapshot, &[ProtocolId::Signal]);
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        for (slot, name) in [(1u32, "ada"), (2, "bea")] {
            snapshot.apply(AdapterEvent::ConversationUpsert {
                conversation: Conversation {
                    protocol: ProtocolId::Signal,
                    id: signal_retained_chat_id(slot, name),
                    title: name.into(),
                    participant: name.into(),
                    preview: String::new(),
                    unread: 0,
                    order: i64::from(slot),
                    last_at: 0,
                    is_group: false,
                    writable: false,
                    placeholder: false,
                    muted: false,
                    has_topics: false,
                    avatar: Default::default(),
                },
            });
        }
        snapshot.select_retained_signal(1);
        let view = View::from_parts(&snapshot, &store, &settings);
        let slot_one = view
            .accounts
            .iter()
            .find(|row| row.signal_retained == Some(1))
            .expect("slot 1")
            .clone();
        let slot_two = view
            .accounts
            .iter()
            .find(|row| row.signal_retained == Some(2))
            .expect("slot 2")
            .clone();
        let live = view
            .accounts
            .iter()
            .find(|row| row.caps.id == ProtocolId::Signal && row.signal_retained.is_none())
            .expect("live row")
            .clone();
        assert!(view.account_row_selected(&slot_one));
        assert!(!view.account_row_selected(&slot_two));
        assert!(!view.account_row_selected(&live));
        assert_eq!(
            view.signal_read_only_reason(),
            Some(SIGNAL_PREVIOUS_ACCOUNT)
        );
        assert_eq!(chip_label(&snapshot, &slot_one), "Previous account 1");
        assert_eq!(chip_label(&snapshot, &slot_two), "Previous account 2");
        assert!(
            !chip_label(&snapshot, &slot_one).contains('+')
                && !chip_label(&snapshot, &slot_two).contains('-'),
            "a chip has no phone number and no account id"
        );

        let mut harness = Harness::builder()
            .with_size(egui::vec2(1100.0, 720.0))
            .build_ui(|ui| {
                theme::install(ui.ctx());
                if !ui.style().text_styles.contains_key(&theme::row_title()) {
                    return;
                }
                let view = View::from_parts(&snapshot, &store, &settings);
                let mut hints = Hints::default();
                let mut out = Vec::new();
                draw(ui, &view, &mut hints, &mut out);
            });
        harness.run_steps(4);
        for label in [
            "Previous account 1",
            "Previous account 2",
            SIGNAL_PREVIOUS_ACCOUNT,
        ] {
            assert!(
                harness
                    .query_all(By::new().role(Role::Label).label(label).include_labels())
                    .next()
                    .is_some(),
                "{label} is on screen"
            );
        }
        assert!(
            harness
                .query_all(
                    By::new()
                        .role(Role::Label)
                        .label("Signal: no supported third-party API, expect breakage.")
                        .include_labels(),
                )
                .next()
                .is_some(),
            "the open previous account keeps the Signal risk line"
        );
    }

    /// Old rows are hidden. The account list shows the kept line.
    /// The line has no phone number.
    #[cfg(feature = "signal-local")]
    #[test]
    fn a_hidden_signal_history_shows_the_kept_line() {
        use eframe::egui::accesskit::Role;
        use egui_kittest::Harness;
        use egui_kittest::kittest::{By, Queryable};
        use thinwire_core::View;
        use thinwire_core::secrets::SecretStore;
        use thinwire_core::settings::Settings;
        use thinwire_core::state::Snapshot;
        use thinwire_core::state::test_support::show_only;
        use thinwire_protocol::{
            AccountState, AdapterEvent, AdapterStatus, ProtocolId, SIGNAL_HISTORY_KEPT,
        };

        use super::{Hints, draw};
        use crate::app::theme;

        let store = SecretStore::memory();
        let settings = Settings::in_memory();
        let mut snapshot = Snapshot::new();
        show_only(&mut snapshot, &[ProtocolId::Signal]);
        snapshot.apply(AdapterEvent::Account {
            protocol: ProtocolId::Signal,
            state: AccountState::Linked,
        });
        snapshot.apply(AdapterEvent::Status {
            protocol: ProtocolId::Signal,
            status: AdapterStatus::Ready,
            detail: SIGNAL_HISTORY_KEPT.into(),
        });
        assert_eq!(
            snapshot.signal_history_kept_line(),
            Some(SIGNAL_HISTORY_KEPT)
        );
        assert!(snapshot.signal_read_only_reason().is_none());

        let mut harness = Harness::builder()
            .with_size(egui::vec2(1100.0, 720.0))
            .build_ui(|ui| {
                theme::install(ui.ctx());
                if !ui.style().text_styles.contains_key(&theme::row_title()) {
                    return;
                }
                let view = View::from_parts(&snapshot, &store, &settings);
                let mut hints = Hints::default();
                let mut out = Vec::new();
                draw(ui, &view, &mut hints, &mut out);
            });
        harness.run_steps(4);
        assert!(
            harness
                .query_all(
                    By::new()
                        .role(Role::Label)
                        .label(SIGNAL_HISTORY_KEPT)
                        .include_labels(),
                )
                .next()
                .is_some(),
            "the kept line is on screen"
        );
        assert!(
            harness
                .query_all(
                    By::new()
                        .role(Role::Label)
                        .label("Previous account 1")
                        .include_labels(),
                )
                .next()
                .is_none(),
            "hidden rows are not chips"
        );
    }
}
