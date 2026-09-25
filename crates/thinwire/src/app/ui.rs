//! Shell: account switcher + inbox on the left, thread in the center.

use std::time::Instant;

use eframe::egui::{self, RichText};
use thinwire_core::ViewNow;
use thinwire_protocol::{AdapterStatus, Delivery, ProtocolId};

use super::auth;
use super::theme::{self, radius, size, space};
use super::theme_mode::ThemeModeEgui;
use super::thread_layout::{RowLayout, list_time, thread_rows};
#[cfg(feature = "slack-oauth")]
use thinwire_core::SlackIntent;
use thinwire_core::secrets::Persistence;
use thinwire_core::state::{
    AccountRow, AuthKey, AuthScreen, CenterView, InboxFilter, InboxState, KEYCHAIN_READ_FAILED,
    OlderState, RESUME_CONNECTING, Snapshot, ThreadState,
};
use thinwire_core::{Intent, TelegramIntent, ThemeMode, View};

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

const COMPOSE_MAX_ROWS: usize = 5;
/// Send is at least this tall. The field grows with the line count.
const SEND_MIN_HEIGHT: f32 = 40.0;
/// Focused field border. Unfocused is 1px. The reserve keeps the thicker ring.
const FIELD_STROKE_MAX: f32 = 2.0;
/// A message bubble uses at most this share of the thread width.
const BUBBLE_WIDTH: f32 = 0.75;
/// Cap so a wide window does not make a line too long to read.
const BUBBLE_MAX: f32 = 560.0;
/// Gap between bubbles in one sender run.
const SAME_RUN_GAP: f32 = 2.0;
/// Gap before the first bubble of the next sender run.
const NEXT_RUN_GAP: f32 = 10.0;

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
    top_bar(ui, snapshot, out);
    status_strip(ui, snapshot, out);
    left_panel(ui, snapshot, hints, out);
    center_panel(ui, snapshot, hints, out);
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

fn top_bar(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    egui::Panel::top("top").show(ui, |ui| {
        let palette = theme::palette(ui);
        ui.add_space(space::XS);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("thinwire")
                    .text_style(egui::TextStyle::Heading)
                    .color(palette.text),
            );
            ui.add_space(space::S);
            for filter in InboxFilter::chrome_filters() {
                let selected = snapshot.filter == *filter;
                if filter_pill(ui, filter.label(), selected).clicked() {
                    out.push(Intent::SetFilter(*filter));
                }
            }
            ui.add_space(space::S);
            if let Some(text) = search_field(ui, &snapshot.search) {
                out.push(Intent::SetSearch(text.into()));
            }
            if snapshot.can_add_account() && ui.button("Add account").clicked() {
                out.push(Intent::Telegram(TelegramIntent::AddAccount));
            }
            add_slack_workspace(ui, snapshot, out);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button("⋯", |ui| {
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
                });
            });
        });
        ui.add_space(space::XS);
    });
}

fn filter_pill(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let palette = theme::palette(ui);
    let fill = if selected {
        palette.selected_row
    } else {
        egui::Color32::TRANSPARENT
    };
    let color = if selected {
        palette.text
    } else {
        palette.text2
    };
    ui.add(
        egui::Button::new(RichText::new(label).color(color))
            .fill(fill)
            .corner_radius(egui::CornerRadius::same(radius::PILL))
            .min_size(egui::vec2(0.0, 32.0)),
    )
}

fn search_field(ui: &mut egui::Ui, search: &str) -> Option<String> {
    let palette = theme::palette(ui);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
    let center = rect.center() - egui::vec2(1.0, 1.0);
    let stroke = egui::Stroke::new(1.5, palette.text3);
    ui.painter().circle_stroke(center, 4.5, stroke);
    ui.painter().line_segment(
        [center + egui::vec2(3.2, 3.2), center + egui::vec2(6.0, 6.0)],
        stroke,
    );
    edited(ui, search, |text| {
        egui::TextEdit::singleline(text)
            .desired_width(160.0)
            .hint_text("title / participant")
    })
}

fn theme_control(ui: &mut egui::Ui, current: ThemeMode, out: &mut Vec<Intent>) {
    ui.label("Theme");
    let mut preference = current.to_egui();
    preference.radio_buttons(ui);
    let chosen = ThemeMode::from_egui(preference);
    if chosen != current {
        out.push(Intent::SetTheme(chosen));
        super::theme_mode::apply(ui.ctx(), chosen);
    }
}

fn status_strip(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    let notice = keychain_notice(snapshot.persistence());
    let show_error = snapshot.auth == AuthScreen::Idle && snapshot.error.is_some();
    let line = snapshot.status_line();
    let show_status = !snapshot.status_is_idle()
        && public_status(&line).is_some_and(|text| !is_idle_status(text));
    // Idle chrome with no notice and no error draws no panel, so the strip is 0 px.
    if !status_strip_visible(snapshot, notice) {
        return;
    }
    let mut dismiss = false;
    egui::Panel::top("status").show(ui, |ui| {
        let palette = theme::palette(ui);
        // One line: a running load of any protocol first, never a finished
        // Ready line while a load runs (#80).
        if show_status && let Some(text) = public_status(&line) {
            let color = if load_failure_text(&line).is_some() {
                palette.error
            } else if snapshot.is_loading() || text == "Refreshing…" {
                palette.warn
            } else {
                palette.text2
            };
            ui.label(RichText::new(text).color(color));
        }
        if let Some(notice) = notice {
            ui.colored_label(palette.warn, notice);
        }
        // A protocol note (`AdapterEvent::Notice`) for the selected protocol.
        // Information, not an error.
        if let Some(note) = snapshot.notice(snapshot.selected_protocol) {
            ui.label(RichText::new(note).color(palette.text2));
        }
        if show_error && let Some(error) = &snapshot.error {
            let happened = error.happened.clone();
            let why = error.why.clone();
            let next = error.next.clone();
            egui::Frame::new()
                .fill(palette.surface)
                .inner_margin(egui::Margin::symmetric(space::S as i8, space::S as i8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (bar, _) =
                            ui.allocate_exact_size(egui::vec2(3.0, 56.0), egui::Sense::hover());
                        ui.painter().rect_filled(bar, 0.0, palette.error);
                        ui.add_space(space::S);
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new(format!("What happened: {happened}"))
                                    .color(palette.error),
                            );
                            ui.label(RichText::new(format!("Why: {why}")).color(palette.text2));
                            ui.label(
                                RichText::new(format!("What to do: {next}")).color(palette.text2),
                            );
                            if ui.button("Dismiss").clicked() {
                                dismiss = true;
                            }
                        });
                    });
                });
        }
    });
    if dismiss {
        out.push(Intent::DismissError);
    }
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

/// True when the status strip draws a panel.
pub(super) fn status_strip_visible(snapshot: &Snapshot, notice: Option<&str>) -> bool {
    let show_error = snapshot.auth == AuthScreen::Idle && snapshot.error.is_some();
    let line = snapshot.status_line();
    let show_status = !snapshot.status_is_idle()
        && public_status(&line).is_some_and(|text| !is_idle_status(text));
    let note = snapshot.notice(snapshot.selected_protocol).is_some();
    notice.is_some() || show_error || show_status || note
}

fn left_panel(ui: &mut egui::Ui, snapshot: &View<'_>, hints: &mut Hints, out: &mut Vec<Intent>) {
    egui::Panel::left("switcher")
        .resizable(true)
        .default_size(280.0)
        .size_range(220.0..=400.0)
        .show(ui, |ui| {
            section_header(ui, "Accounts");
            ui.separator();

            let unread_by_protocol: Vec<u32> = snapshot
                .accounts
                .iter()
                .map(|account| snapshot.unread_for(account.caps.id))
                .collect();
            let mut clicked: Option<ProtocolId> = None;
            for (account, unread) in snapshot.accounts.iter().zip(unread_by_protocol) {
                if !snapshot.shows_in_switcher(account.caps.id) {
                    continue;
                }
                if account_chip(
                    ui,
                    snapshot,
                    account,
                    snapshot.selected_protocol == account.caps.id,
                    unread,
                ) {
                    clicked = Some(account.caps.id);
                }
                ui.add_space(4.0);
            }
            if let Some(protocol) = clicked {
                out.push(Intent::SelectProtocol(protocol));
            }

            #[cfg(feature = "whatsapp-web")]
            if snapshot.whatsapp_pairing_available() {
                super::whatsapp_gate::risk_entry(ui, out);
            }

            #[cfg(feature = "signal-local")]
            if snapshot.signal_linking_available() {
                super::signal_gate::notice_entry(ui, out);
            }

            ui.add_space(space::S);
            section_header(ui, "Inbox");
            ui.separator();
            let inbox_list_id = ui.make_persistent_id("inbox");
            egui::ScrollArea::vertical()
                .id_salt("inbox")
                .auto_shrink([false, false])
                .show(ui, |ui| inbox(ui, snapshot, hints, inbox_list_id, out));
        });
}

fn account_chip(
    ui: &mut egui::Ui,
    snapshot: &Snapshot,
    account: &AccountRow,
    selected: bool,
    unread: u32,
) -> bool {
    let caps = account.caps;
    let palette = theme::palette(ui);
    let width = ui.available_width();
    let (rect, hover) = ui.allocate_exact_size(egui::vec2(width, 44.0), egui::Sense::hover());
    let fill = if selected {
        palette.selected_row
    } else if hover.hovered() {
        palette.surface
    } else {
        egui::Color32::TRANSPARENT
    };
    if fill != egui::Color32::TRANSPARENT {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(radius::CONTROL), fill);
    }
    let inner = rect.shrink2(egui::vec2(space::S, space::XS));
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::left_to_right(egui::Align::Center))
            .id_salt(("account-chip", caps.id)),
    );
    let (dot, _) = child.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    child.painter().circle_filled(
        dot.center(),
        4.0,
        if account.status == AdapterStatus::Ready {
            palette.ok
        } else {
            palette.text3
        },
    );
    child.add_space(space::S);
    child.vertical(|ui| {
        ui.label(
            RichText::new(caps.id.display_name())
                .text_style(theme::row_title())
                .color(palette.text),
        );
        let label = chip_label(snapshot, account);
        let color = if account.status == AdapterStatus::Ready {
            palette.ok
        } else {
            palette.text3
        };
        ui.label(RichText::new(label).small().color(color));
    });
    if let Some(text) = badge_text(unread) {
        child.add_space(space::S);
        unread_badge(&mut child, &text);
    }
    let response = ui
        .interact(
            rect,
            ui.id().with(("account-chip", caps.id)),
            egui::Sense::click(),
        )
        .on_hover_ui(|ui| {
            ui.colored_label(palette.support(caps.support), caps.short_label);
            ui.label(format!("status: {}", account.status.as_str()));
        });
    let slack_sign_in = caps.id == ProtocolId::Slack && cfg!(feature = "slack-oauth");
    if caps.id == ProtocolId::Signal && cfg!(feature = "signal-local") {
        ui.label(
            RichText::new("local build only — not in releases")
                .small()
                .weak(),
        );
    } else if caps.id == ProtocolId::WhatsApp && cfg!(feature = "whatsapp-web") {
        ui.label(
            RichText::new("experimental spike — ban risk")
                .small()
                .weak(),
        );
    } else if !matches!(caps.id, ProtocolId::Telegram) && !slack_sign_in {
        ui.label(
            RichText::new("not ready — no login UI this beat")
                .small()
                .weak(),
        );
    }
    response.clicked()
}

/// "Accounts" and "Inbox": caption, uppercase, `text3`.
fn section_header(ui: &mut egui::Ui, title: &str) {
    ui.label(
        RichText::new(title.to_uppercase())
            .small()
            .color(theme::palette(ui).text3),
    );
}

/// "No chats." is a finished load with zero rows. Before the selected
/// protocol is linked the list stays blank.
#[must_use]
fn show_no_chats(snapshot: &Snapshot) -> bool {
    snapshot
        .accounts
        .iter()
        .any(|row| row.caps.id == snapshot.selected_protocol && row.linked())
}

/// The chat-list time of `at`, in the zone of the view's clock (#120).
fn view_list_time(at: i64, now: ViewNow) -> String {
    match now {
        ViewNow::Local(now) => list_time(at, &now),
        ViewNow::Fixed(now) => list_time(at, &now),
    }
}

fn inbox(
    ui: &mut egui::Ui,
    snapshot: &View<'_>,
    hints: &mut Hints,
    inbox_list_id: egui::Id,
    out: &mut Vec<Intent>,
) {
    let now = snapshot.now();
    let rows: Vec<(String, String, String, u32, String)> = snapshot
        .visible_conversations()
        .iter()
        .map(|row| {
            (
                row.id.clone(),
                row.title.clone(),
                row.preview.clone(),
                row.unread,
                view_list_time(row.last_at, now),
            )
        })
        .collect();

    match snapshot.inbox_state() {
        InboxState::Rows => {}
        InboxState::Loading => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Loading chats…").weak());
            });
            return;
        }
        InboxState::Empty => {
            if show_no_chats(snapshot) {
                ui.label(RichText::new("No chats.").italics().weak());
            }
            return;
        }
        InboxState::NoMatch => {
            let query = snapshot.search.trim();
            ui.label(
                RichText::new(format!("No chats match '{query}'."))
                    .italics()
                    .weak(),
            );
            return;
        }
    }

    let scroll_to_selected = hints.take_scroll_to_selected();
    let scroll_to_focused = hints.take_scroll_to_focused();
    let widget_focus = ui.ctx().memory(|memory| memory.focused());
    let mut clicked: Option<String> = None;
    let mut focus_from_widget: Option<String> = None;
    for (id, title, preview, unread, time) in rows {
        let selected = snapshot.selected_conversation.as_deref() == Some(id.as_str());
        let focused = snapshot.focused_row.as_deref() == Some(id.as_str());
        let response = inbox_row(ui, &id, &title, &preview, &time, unread, selected);
        if focused {
            ui.painter().rect_stroke(
                response.rect,
                egui::CornerRadius::same(radius::CONTROL),
                egui::Stroke::new(2.0, theme::palette(ui).text),
                egui::StrokeKind::Inside,
            );
        }
        // AccessKit `selected` is the open chat. The highlight is keyboard focus.
        response.widget_info(|| {
            egui::WidgetInfo::selected(egui::WidgetType::Button, true, selected, &title)
        });
        if row_scrolls(selected, focused, scroll_to_selected, scroll_to_focused) {
            response.scroll_to_me(None);
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
        let row_id = ui.id().with(("inbox-row", id));
        ui.memory_mut(|memory| memory.request_focus(row_id));
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
        .map(|row| ui.id().with(("inbox-row", &row.id)))
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
    let id = ui.id().with(key);
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

/// Title line, preview line, and the padding around them.
///
/// Each line sits in a horizontal row, and that row is at least
/// [`theme::MIN_TARGET`] tall (`interact_size`). The preview line is also
/// at least as tall as the unread badge, which adds [`space::XS`] around
/// the caption.
fn inbox_row_height(ui: &egui::Ui) -> f32 {
    let line = ui.spacing().interact_size.y;
    let title = line.max(ui.text_style_height(&theme::row_title()));
    let caption = ui.text_style_height(&egui::TextStyle::Small);
    let preview = line.max(ui.text_style_height(&theme::secondary()));
    let badge = caption + space::XS;
    space::S + title + space::XS + preview.max(badge) + space::S
}

/// One inbox row. The whole rect is the click target, including the preview.
fn inbox_row(
    ui: &mut egui::Ui,
    id: &str,
    title: &str,
    preview: &str,
    time: &str,
    unread: u32,
    selected: bool,
) -> egui::Response {
    let palette = theme::palette(ui);
    let width = laid_out_width(ui, "inbox-row-width");
    let (rect, hover) = ui.allocate_exact_size(
        egui::vec2(width, inbox_row_height(ui)),
        egui::Sense::hover(),
    );
    let fill = if selected {
        palette.selected_row
    } else if hover.hovered() {
        palette.surface
    } else {
        egui::Color32::TRANSPARENT
    };
    if fill != egui::Color32::TRANSPARENT {
        ui.painter()
            .rect_filled(rect, egui::CornerRadius::same(radius::CONTROL), fill);
    }
    let gutter = ui.spacing().scroll.bar_width;
    let inner = egui::Rect::from_min_max(
        rect.min + egui::vec2(space::M, space::S),
        rect.max - egui::vec2(space::M + gutter, space::S),
    );
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(inner)
            .layout(egui::Layout::top_down(egui::Align::Min))
            .id_salt(("inbox-row", id)),
    );
    child.spacing_mut().item_spacing.y = space::XS;
    let title_font = if unread > 0 {
        theme::semibold()
    } else {
        theme::medium()
    };
    row_ends(
        &mut child,
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
            ui.label(RichText::new(time).small().color(palette.text3));
        },
    );
    row_ends(
        &mut child,
        |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(preview)
                        .text_style(theme::secondary())
                        .color(palette.text2),
                )
                .truncate()
                .halign(egui::Align::LEFT),
            );
        },
        |ui| {
            if let Some(text) = badge_text(unread) {
                unread_badge(ui, &text);
            }
        },
    );
    ui.interact(rect, ui.id().with(("inbox-row", id)), egui::Sense::click())
}

/// Unread pill. `text` comes from [`badge_text`].
fn unread_badge(ui: &mut egui::Ui, text: &str) {
    let palette = theme::palette(ui);
    let font = egui::FontId::new(size::CAPTION, theme::medium());
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font, palette.on_badge);
    let width = (galley.size().x + space::XS * 2.0).max(20.0);
    let height = galley.size().y + space::XS;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Label, true, format!("unread {text}"))
    });
    ui.painter()
        .rect_filled(rect, egui::CornerRadius::same(radius::PILL), palette.badge);
    let pos = egui::pos2(
        rect.center().x - galley.size().x * 0.5,
        rect.center().y - galley.size().y * 0.5,
    );
    ui.painter().galley(pos, galley, palette.on_badge);
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

/// Chip text. A compiled client reports Connecting at launch. That is
/// "Not signed in" until a login or a resume starts.
#[must_use]
fn chip_label(snapshot: &Snapshot, account: &AccountRow) -> &'static str {
    let session_moving = snapshot.auth != AuthScreen::Idle
        || matches!(snapshot.center_view(), CenterView::Resuming { .. });
    if account.caps.id == ProtocolId::Telegram
        && account.status == AdapterStatus::Connecting
        && !snapshot.telegram_ready()
        && !session_moving
    {
        "Not signed in"
    } else if account.linked()
        && matches!(
            account.status,
            AdapterStatus::Error | AdapterStatus::Refused
        )
    {
        // Still signed in: an error status never unlinks (shell plan 11).
        "Connection problem"
    } else {
        account_label(account.status)
    }
}

/// Short account state for the chip. The raw status stays on the hover text.
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
    let frame = egui::Frame::central_panel(ui.style())
        .fill(fill)
        .inner_margin(space::M);
    egui::CentralPanel::default().frame(frame).show(ui, |ui| {
        // Keys first, before a text field can take Enter. One press, one action.
        let (enter, escape) = ui.input(|input| {
            (
                input.key_pressed(egui::Key::Enter),
                input.key_pressed(egui::Key::Escape),
            )
        });
        if escape {
            out.push(Intent::Key(AuthKey::Escape));
        } else if enter {
            out.push(Intent::Key(AuthKey::Enter));
        }
        match snapshot.center_view() {
            CenterView::Auth => auth::draw(ui, snapshot, out),
            CenterView::Resuming { connecting } => resuming(ui, snapshot, connecting),
            CenterView::FirstRun => first_run(ui, snapshot, out),
            CenterView::KeychainFailed => keychain_failed(ui, out),
            CenterView::Thread => thread(ui, snapshot, hints, out),
        }
    });
}

fn resuming(ui: &mut egui::Ui, snapshot: &Snapshot, connecting: bool) {
    let top = (ui.available_height() * 0.18).clamp(24.0, 96.0);
    ui.add_space(top);
    ui.vertical_centered(|ui| {
        ui.spinner();
        ui.add_space(8.0);
        if connecting {
            ui.label(RESUME_CONNECTING);
        } else {
            ui.label(snapshot.keychain_wait_text(Instant::now()));
        }
    });
}

fn keychain_failed(ui: &mut egui::Ui, out: &mut Vec<Intent>) {
    let top = (ui.available_height() * 0.18).clamp(24.0, 96.0);
    ui.add_space(top);
    ui.vertical_centered(|ui| {
        ui.colored_label(theme::palette(ui).warn, KEYCHAIN_READ_FAILED);
        ui.add_space(12.0);
        if ui.button("Try again").clicked() {
            out.push(Intent::RetryKeychain);
        }
    });
}

fn first_run(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    let id = ui.id().with("first-run-card-height");
    let known = ui.data(|data| data.get_temp::<f32>(id).unwrap_or(0.0));
    let spare = (ui.available_height() - known).max(0.0);
    ui.add_space(spare / 2.0);
    let response = ui
        .scope(|ui| {
            theme::show_centered_card(ui, |ui| {
                let palette = theme::palette(ui);
                ui.label(
                    RichText::new("Start with Telegram")
                        .text_style(theme::display())
                        .color(palette.text),
                );
                ui.add_space(space::M);
                if let Some(banner) = super::auth::stub_banner(snapshot) {
                    ui.colored_label(palette.warn, banner);
                    ui.add_space(space::S);
                }
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
                ui.add_space(space::M);
                if ui
                    .add(
                        egui::Button::new(RichText::new("Add Telegram").color(palette.on_accent))
                            .fill(palette.accent)
                            .min_size(egui::vec2(ui.available_width(), 40.0)),
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
    if ui.button("Add Slack workspace").clicked() {
        out.push(Intent::Slack(SlackIntent::Connect));
    }
}

#[cfg(not(feature = "slack-oauth"))]
fn add_slack_workspace(_ui: &mut egui::Ui, _snapshot: &View<'_>, _out: &mut Vec<Intent>) {}

fn thread(ui: &mut egui::Ui, snapshot: &View<'_>, hints: &mut Hints, out: &mut Vec<Intent>) {
    thread_header(ui, snapshot);

    let is_group = snapshot
        .selected_conversation_row()
        .is_some_and(|row| row.is_group);
    let layout = match snapshot.now() {
        ViewNow::Local(now) => thread_rows(snapshot.selected_messages(), is_group, &now),
        ViewNow::Fixed(now) => thread_rows(snapshot.selected_messages(), is_group, &now),
    };
    let messages: Vec<Bubble> = snapshot
        .selected_messages()
        .iter()
        .zip(layout)
        .map(|(message, layout)| Bubble {
            id: message.id.clone(),
            outbound: message.outbound,
            sender: message.sender.clone(),
            body: message.body.clone(),
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
    let reserve = compose_reserve(row_height, compose_rows, ui.spacing().item_spacing.y);

    let state = snapshot.thread_state();
    let older = snapshot.older_state();
    let mut retry: Option<String> = None;
    // One scroll state per chat, so each chat opens at its newest message.
    let salt = snapshot.selected_conversation.clone().unwrap_or_default();
    let memo_id = ui.make_persistent_id(("thread-older", &salt));
    let memo: ThreadMemo = ui.data(|data| data.get_temp(memo_id)).unwrap_or_default();
    let mut area = egui::ScrollArea::vertical()
        .id_salt(("thread", salt))
        .auto_shrink([false, true])
        .stick_to_bottom(true)
        .max_height(ui.available_height() - reserve);
    if let Some(offset) = memo.restore {
        area = area.vertical_scroll_offset(offset);
    }
    let output = area.show(ui, |ui| {
        match older {
            OlderState::Loading => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Loading older messages…").weak());
                });
            }
            OlderState::StartOfChat if state == ThreadState::Rows => {
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("Start of chat").small().weak());
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
                    ui.spinner();
                    ui.label(RichText::new("Loading messages…").weak());
                });
            }
            ThreadState::Empty => {
                ui.label(RichText::new("No messages in this chat.").italics().weak());
            }
            ThreadState::NoSelection | ThreadState::Rows => {}
        }
        let mut gap_before = None;
        for message in &messages {
            bubble(ui, message, &mut retry, gap_before);
            gap_before = Some(message.layout.run_end);
        }
    });
    let first_id = messages.first().map(|message| message.id.clone());
    let step = older_step(
        &memo,
        first_id.as_deref(),
        |id| messages.iter().any(|message| message.id == id),
        output.state.offset.y,
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
        });
    }
    if let Some(message_id) = retry {
        out.push(Intent::Retry { message_id });
    }

    compose(ui, snapshot, hints, compose_height, out);
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

/// One message row, ready to draw.
struct Bubble {
    id: String,
    outbound: bool,
    sender: String,
    body: String,
    delivery: Delivery,
    /// Group chat. The AccessKit name keeps the sender on every row.
    group: bool,
    layout: RowLayout,
}

/// Title, optional "group" line, and a bottom border.
fn thread_header(ui: &mut egui::Ui, snapshot: &Snapshot) {
    let palette = theme::palette(ui);
    egui::Frame::new()
        .inner_margin(egui::Margin::same(space::M as i8))
        .show(ui, |ui| match snapshot.selected_conversation_row() {
            Some(conversation) => {
                ui.label(
                    RichText::new(&conversation.title)
                        .heading()
                        .color(palette.text),
                );
                if conversation.is_group {
                    ui.label(RichText::new("group").small().color(palette.text3));
                }
            }
            None => {
                ui.label(RichText::new("Thread").heading().color(palette.text));
                ui.label(
                    RichText::new("Select a conversation in the inbox.")
                        .small()
                        .color(palette.text3),
                );
            }
        });
    let y = ui.cursor().min.y;
    ui.painter().hline(
        ui.max_rect().x_range(),
        y,
        egui::Stroke::new(1.0, palette.border),
    );
}

/// Own messages sit on the right on `out`. Others sit on the left on `surface`.
///
/// `previous_run_ended` is `None` for the first row. `Some(true)` inserts the
/// gap between runs. `Some(false)` inserts the gap inside a run.
fn bubble(
    ui: &mut egui::Ui,
    message: &Bubble,
    retry: &mut Option<String>,
    previous_run_ended: Option<bool>,
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
    let (align, fill, body, meta) = if message.outbound {
        (
            egui::Align::Max,
            palette.out,
            palette.on_out,
            palette.out_meta,
        )
    } else {
        (
            egui::Align::Min,
            palette.surface,
            palette.text,
            palette.text3,
        )
    };
    ui.with_layout(egui::Layout::top_down(align), |ui| {
        egui::Frame::new()
            .fill(fill)
            .corner_radius(bubble_radius(message.outbound, message.layout.run_end))
            .inner_margin(egui::Margin::symmetric(space::M as i8, space::S as i8))
            .show(ui, |ui| {
                ui.set_max_width(max_width);
                let meta_width = meta_column_width(ui, &message.layout.time, message.delivery);
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    if message.layout.show_sender {
                        ui.label(RichText::new(&message.sender).small().color(palette.text));
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Min), |ui| {
                        let gap = ui.spacing().item_spacing.x;
                        let text_width = (ui.available_width() - meta_width - gap).max(32.0);
                        let body_rect = ui
                            .vertical(|ui| {
                                ui.set_max_width(text_width);
                                ui.add(
                                    egui::Label::new(RichText::new(&message.body).color(body))
                                        .selectable(true)
                                        .wrap()
                                        .halign(egui::Align::LEFT),
                                );
                            })
                            .response
                            .rect;
                        let line = ui.text_style_height(&egui::TextStyle::Small);
                        let has_time = !message.layout.time.is_empty();
                        let extra = match message.delivery {
                            Delivery::Pending => 1,
                            Delivery::Failed => 2,
                            Delivery::Sent => 0,
                        };
                        if has_time || extra > 0 {
                            let gap_y = ui.spacing().item_spacing.y;
                            let time_h = if has_time { line } else { 0.0 };
                            // The time shares the last body line. Status rows continue below it.
                            let extra_h = if extra == 0 {
                                0.0
                            } else {
                                gap_y + line * extra as f32 + gap_y * (extra - 1) as f32
                            };
                            let meta_top = if has_time {
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
                                    if !message.layout.time.is_empty() {
                                        ui.label(
                                            RichText::new(&message.layout.time).small().color(meta),
                                        );
                                    }
                                    match message.delivery {
                                        Delivery::Pending => {
                                            ui.label(RichText::new("Sending…").small().color(meta));
                                        }
                                        Delivery::Failed => {
                                            ui.colored_label(
                                                palette.out_error,
                                                RichText::new("Not sent").small(),
                                            );
                                            if ui
                                                .add(
                                                    egui::Button::new(
                                                        RichText::new("Retry")
                                                            .color(palette.accent),
                                                    )
                                                    .frame(false),
                                                )
                                                .clicked()
                                            {
                                                *retry = Some(message.id.clone());
                                            }
                                        }
                                        Delivery::Sent => {}
                                    }
                                },
                            );
                        }
                    });
                });
            })
            .response
            .widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Panel,
                    true,
                    bubble_access_label(message),
                )
            });
    });
}

/// Width of the time and delivery column. Empty when the row has neither.
fn meta_column_width(ui: &egui::Ui, time: &str, delivery: Delivery) -> f32 {
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
    let mut width = text_width(time);
    match delivery {
        Delivery::Pending => width = width.max(text_width("Sending…")),
        Delivery::Failed => {
            width = width.max(text_width("Not sent"));
            let pad = ui.spacing().button_padding.x * 2.0;
            width = width.max(text_width("Retry") + pad);
        }
        Delivery::Sent => {}
    }
    width
}

/// AccessKit name of a bubble.
///
/// A group row names the sender and the time on every message. A private
/// row names the message. The visual sender header stays on the first row
/// of a run.
fn bubble_access_label(message: &Bubble) -> String {
    if message.group {
        format!(
            "{} {} {}",
            message.sender, message.layout.time, message.body
        )
    } else {
        message.body.clone()
    }
}

/// 12 on every corner. The last bubble of a run uses 4 on the sender side.
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

/// Height of the field row: text, field padding, focus ring, at least Send.
///
/// `ui.horizontal` only offers `interact_size` of height, and a vertical
/// scroll area otherwise stays at its 64px minimum. Compose uses this as
/// both `min_scrolled_height` and `max_height`, so the bar tracks the text.
/// Rows in the draft. `str::lines` drops a trailing blank row from Shift+Enter.
fn compose_line_count(text: &str) -> usize {
    text.split('\n').count()
}

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
    let compose_id = egui::Id::new("thread-compose");
    // The chat this frame drew. Drafts and sends name it, so a click on
    // another chat in the same frame cannot move them (PR #48 review).
    let chat = snapshot
        .selected_conversation
        .clone()
        .map(|id| (snapshot.selected_protocol, id));
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
        if enter && !other && !shift {
            if let Some((protocol, conversation_id)) = chat.clone() {
                out.push(Intent::SendDraft {
                    protocol,
                    conversation_id,
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
    let focused = ui.memory(|memory| memory.has_focus(compose_id));
    let field = egui::Frame::new()
        .fill(palette.input)
        .stroke(egui::Stroke::new(
            if focused { FIELD_STROKE_MAX } else { 1.0 },
            if focused {
                palette.accent
            } else {
                palette.border_strong
            },
        ))
        .corner_radius(radius::FIELD)
        .inner_margin(egui::Margin::symmetric(space::M as i8, space::M as i8));
    egui::Frame::new()
        .fill(palette.sidebar)
        .inner_margin(egui::Margin::same(space::M as i8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let send_width = 72.0;
                let width = (ui.available_width() - send_width - space::S).max(0.0);
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
                        if let Some(text) = edited(ui, &snapshot.compose, |text| {
                            egui::TextEdit::multiline(text)
                                .id(compose_id)
                                .frame(field)
                                .desired_rows(1)
                                .desired_width(width)
                                .hint_text(RichText::new("Message").color(palette.text3))
                        }) && let Some((protocol, conversation_id)) = chat.clone()
                        {
                            out.push(Intent::SetDraft {
                                protocol,
                                conversation_id,
                                text: text.into(),
                            });
                        }
                    });
                let can_send = snapshot.can_send();
                if ui
                    .add_enabled(snapshot.can_send(), {
                        egui::Button::new(
                            RichText::new("Send").color(send_label(can_send, palette)),
                        )
                        .fill(send_fill(can_send, palette))
                        .min_size(egui::vec2(send_width, SEND_MIN_HEIGHT))
                    })
                    .clicked()
                    && let Some((protocol, conversation_id)) = chat.clone()
                {
                    out.push(Intent::SendDraft {
                        protocol,
                        conversation_id,
                    });
                }
            });
        });
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
    use super::{account_label, badge_text, row_scrolls};

    #[test]
    fn both_scroll_requests_scroll_the_highlight() {
        assert!(row_scrolls(false, true, true, true));
        assert!(!row_scrolls(true, false, true, true));
        assert!(row_scrolls(true, false, true, false));
        assert!(!row_scrolls(false, true, true, false));
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
                meta = super::meta_column_width(ui, "10:00", Delivery::Sent);
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
                        memory.request_focus(egui::Id::new("thread-compose"));
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
                                memory.request_focus(egui::Id::new("thread-compose"));
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
    fn inbox_stays_blank_before_sign_in() {
        use super::show_no_chats;
        use thinwire_core::state::Snapshot;

        let mut snapshot = Snapshot::new();
        assert!(!show_no_chats(&snapshot));
        snapshot.apply(thinwire_protocol::AdapterEvent::Account {
            protocol: thinwire_protocol::ProtocolId::Telegram,
            state: thinwire_protocol::AccountState::Linked,
        });
        assert!(show_no_chats(&snapshot), "the selected protocol is linked");
    }

    #[test]
    fn add_account_hides_when_telegram_is_linked() {
        use thinwire_core::state::{InboxFilter, Snapshot};

        let mut snapshot = Snapshot::new();
        assert!(snapshot.can_add_account());
        snapshot.telegram_authorized = true;
        assert!(!snapshot.can_add_account());
        assert!(InboxFilter::chrome_filters().len() + 2 <= 6);
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
}
