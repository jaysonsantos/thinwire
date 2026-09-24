//! Shell: account switcher + inbox on the left, thread in the center.

use std::time::Instant;

use chrono::Local;
use eframe::egui::{self, RichText};
use thinwire_protocol::{AdapterStatus, Delivery, ProtocolId, WhatsAppPhoneVault};

use super::auth;
use super::secrets::{Persistence, SecretStore};
use super::settings::{Settings, ThemeMode};
use super::snapshot::{
    AccountRow, AuthKey, AuthScreen, CenterView, InboxFilter, InboxState, KEYCHAIN_READ_FAILED,
    RESUME_CONNECTING, Snapshot, ThreadState,
};
use super::theme::{self, radius, size, space};
use super::thread_layout::{RowLayout, list_time, thread_rows};

/// Shown while the OS keychain is not available. Secrets stay in memory.
pub(crate) const KEYCHAIN_UNAVAILABLE_NOTICE: &str =
    "Sign-in is not saved on this device: keychain unavailable.";

/// Shown when only the kernel keyring works. It is lost at restart.
pub(crate) const KEYCHAIN_UNTIL_RESTART_NOTICE: &str =
    "Sign-in is kept until you restart the computer.";

/// One notice for the whole window. `None` while the keychain loads or keeps
/// the sign-in across restarts.
#[must_use]
pub(crate) fn keychain_notice(secrets: &SecretStore) -> Option<&'static str> {
    match secrets.persistence() {
        Persistence::ThisSession => Some(KEYCHAIN_UNAVAILABLE_NOTICE),
        Persistence::UntilRestart => Some(KEYCHAIN_UNTIL_RESTART_NOTICE),
        Persistence::Loading | Persistence::Saved => None,
    }
}

const COMPOSE_MAX_ROWS: usize = 5;
/// Send is at least this tall. The field grows with the line count.
const SEND_MIN_HEIGHT: f32 = 40.0;
/// A message bubble uses at most this share of the thread width.
const BUBBLE_WIDTH: f32 = 0.75;
/// Cap so a wide window does not make a line too long to read.
const BUBBLE_MAX: f32 = 560.0;
/// Gap between bubbles in one sender run.
const SAME_RUN_GAP: f32 = 2.0;
/// Gap before the first bubble of the next sender run.
const NEXT_RUN_GAP: f32 = 10.0;

pub(crate) fn draw(
    ui: &mut egui::Ui,
    snapshot: &mut Snapshot,
    settings: &mut Settings,
    secrets: &SecretStore,
    whatsapp_phone: &WhatsAppPhoneVault,
) {
    settings.apply(ui.ctx());
    #[cfg(feature = "whatsapp-web")]
    if snapshot.whatsapp_pairing_available() && snapshot.whatsapp_gate_open() {
        super::whatsapp_gate::draw(ui, snapshot, whatsapp_phone);
        return;
    }
    #[cfg(not(feature = "whatsapp-web"))]
    let _ = whatsapp_phone;
    top_bar(ui, snapshot, settings, secrets);
    status_strip(ui, snapshot, secrets);
    left_panel(ui, snapshot);
    center_panel(ui, snapshot, secrets);
}

fn top_bar(
    ui: &mut egui::Ui,
    snapshot: &mut Snapshot,
    settings: &mut Settings,
    secrets: &SecretStore,
) {
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
                    snapshot.set_filter(*filter);
                }
            }
            ui.add_space(space::S);
            search_field(ui, &mut snapshot.search);
            if snapshot.can_add_account() && ui.button("Add account").clicked() {
                snapshot.open_add_account(secrets);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.menu_button("⋯", |ui| {
                    if ui.button("Refresh").clicked() {
                        snapshot.refresh_visible();
                        ui.close();
                    }
                    if snapshot.can_add_account() && ui.button("Advanced").clicked() {
                        snapshot.open_api_override(secrets);
                        ui.close();
                    }
                    ui.separator();
                    theme_control(ui, settings);
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

fn search_field(ui: &mut egui::Ui, search: &mut String) {
    let palette = theme::palette(ui);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
    let center = rect.center() - egui::vec2(1.0, 1.0);
    let stroke = egui::Stroke::new(1.5, palette.text3);
    ui.painter().circle_stroke(center, 4.5, stroke);
    ui.painter().line_segment(
        [center + egui::vec2(3.2, 3.2), center + egui::vec2(6.0, 6.0)],
        stroke,
    );
    ui.add(
        egui::TextEdit::singleline(search)
            .desired_width(160.0)
            .hint_text("title / participant"),
    );
}

fn theme_control(ui: &mut egui::Ui, settings: &mut Settings) {
    ui.label("Theme");
    let mut preference = settings.theme().to_egui();
    preference.radio_buttons(ui);
    let chosen = ThemeMode::from_egui(preference);
    if chosen != settings.theme() {
        settings.set_theme(chosen);
        settings.apply(ui.ctx());
    }
}

fn status_strip(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    let notice = keychain_notice(secrets);
    let show_error = snapshot.auth == AuthScreen::Idle && snapshot.error.is_some();
    let show_status =
        public_status(&snapshot.status_text).is_some_and(|text| !is_idle_status(text));
    // Idle chrome with no notice and no error draws no panel, so the strip is 0 px.
    if !status_strip_visible(snapshot, notice) {
        return;
    }
    let mut dismiss = false;
    egui::Panel::top("status").show(ui, |ui| {
        let palette = theme::palette(ui);
        if show_status && let Some(text) = public_status(&snapshot.status_text) {
            let color = if load_failure_text(&snapshot.status_text).is_some() {
                palette.error
            } else if text == "Sending…" || text == "Refreshing…" {
                palette.warn
            } else {
                palette.text2
            };
            ui.label(RichText::new(text).color(color));
        }
        if let Some(notice) = notice {
            ui.colored_label(palette.warn, notice);
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
        snapshot.error = None;
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
fn status_strip_visible(snapshot: &Snapshot, notice: Option<&str>) -> bool {
    let show_error = snapshot.auth == AuthScreen::Idle && snapshot.error.is_some();
    let show_status =
        public_status(&snapshot.status_text).is_some_and(|text| !is_idle_status(text));
    notice.is_some() || show_error || show_status
}

fn left_panel(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
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
                snapshot.select_protocol(protocol);
            }

            #[cfg(feature = "whatsapp-web")]
            if snapshot.whatsapp_pairing_available() {
                super::whatsapp_gate::risk_entry(ui, snapshot);
            }

            ui.add_space(space::S);
            section_header(ui, "Inbox");
            ui.separator();
            egui::ScrollArea::vertical()
                .id_salt("inbox")
                .auto_shrink([false, false])
                .show(ui, |ui| inbox(ui, snapshot));
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
    if caps.id == ProtocolId::WhatsApp && cfg!(feature = "whatsapp-web") {
        ui.label(
            RichText::new("experimental spike — ban risk")
                .small()
                .weak(),
        );
    } else if !matches!(caps.id, ProtocolId::Telegram) {
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

/// "No chats." is a finished load with zero rows. Before sign-in the list stays blank.
#[must_use]
fn show_no_chats(snapshot: &Snapshot) -> bool {
    snapshot.telegram_ready() || snapshot.selected_protocol != ProtocolId::Telegram
}

fn inbox(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    let now = Local::now();
    let rows: Vec<(String, String, String, u32, String)> = snapshot
        .visible_conversations()
        .iter()
        .map(|row| {
            (
                row.id.clone(),
                row.title.clone(),
                row.preview.clone(),
                row.unread,
                list_time(row.last_at, &now),
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

    let scroll_to_selected = snapshot.take_scroll_to_selected();
    let mut clicked: Option<String> = None;
    for (id, title, preview, unread, time) in rows {
        let selected = snapshot.selected_conversation.as_deref() == Some(id.as_str());
        let response = inbox_row(ui, &id, &title, &preview, &time, unread, selected);
        if selected && scroll_to_selected {
            response.scroll_to_me(None);
        }
        if response.clicked() {
            clicked = Some(id);
        }
    }
    if let Some(id) = clicked {
        snapshot.select_conversation(id);
    }
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
    let width = ui.available_width();
    let (rect, hover) = ui.allocate_exact_size(egui::vec2(width, 60.0), egui::Sense::hover());
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
    child.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(time).small().color(palette.text3));
            ui.add(
                egui::Label::new(
                    RichText::new(title)
                        .text_style(theme::row_title())
                        .family(title_font)
                        .color(palette.text),
                )
                .truncate(),
            );
        });
    });
    child.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if let Some(text) = badge_text(unread) {
                unread_badge(ui, &text);
            }
            ui.add(
                egui::Label::new(
                    RichText::new(preview)
                        .text_style(theme::secondary())
                        .color(palette.text2),
                )
                .truncate(),
            );
        });
    });
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
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
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

fn center_panel(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
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
            snapshot.center_key(AuthKey::Escape, secrets);
        } else if enter {
            snapshot.center_key(AuthKey::Enter, secrets);
        }
        match snapshot.center_view() {
            CenterView::Auth => auth::draw(ui, snapshot, secrets),
            CenterView::Resuming { connecting } => resuming(ui, snapshot, connecting),
            CenterView::FirstRun => first_run(ui, snapshot, secrets),
            CenterView::KeychainFailed => keychain_failed(ui, snapshot),
            CenterView::Thread => thread(ui, snapshot),
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

fn keychain_failed(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    let top = (ui.available_height() * 0.18).clamp(24.0, 96.0);
    ui.add_space(top);
    ui.vertical_centered(|ui| {
        ui.colored_label(theme::palette(ui).warn, KEYCHAIN_READ_FAILED);
        ui.add_space(12.0);
        if ui.button("Try again").clicked() {
            snapshot.retry_keychain();
        }
    });
}

fn first_run(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
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
        if snapshot.has_api_credentials(secrets) {
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
            snapshot.open_telegram(secrets);
        }
            });
        })
        .response;
    ui.data_mut(|data| data.insert_temp(id, response.rect.height()));
}

fn thread(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    thread_header(ui, snapshot);

    let is_group = snapshot
        .selected_conversation_row()
        .is_some_and(|row| row.is_group);
    let layout = thread_rows(snapshot.selected_messages(), is_group, &Local::now());
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
            layout,
        })
        .collect();

    // Compose grows to COMPOSE_MAX_ROWS lines, then scrolls. Reserve its height.
    let row_height = ui.text_style_height(&egui::TextStyle::Body);
    let compose_rows = snapshot.compose.lines().count().clamp(1, COMPOSE_MAX_ROWS);
    let compose_height = row_height * COMPOSE_MAX_ROWS as f32;
    let reserve = compose_reserve(row_height, compose_rows);

    let state = snapshot.thread_state();
    let mut retry: Option<String> = None;
    // One scroll state per chat, so each chat opens at its newest message.
    let salt = snapshot.selected_conversation.clone().unwrap_or_default();
    egui::ScrollArea::vertical()
        .id_salt(("thread", salt))
        .auto_shrink([false, true])
        .stick_to_bottom(true)
        .max_height(ui.available_height() - reserve)
        .show(ui, |ui| {
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
    if let Some(id) = retry {
        snapshot.retry_send(&id);
    }

    compose(ui, snapshot, compose_height);
}

/// One message row, ready to draw.
struct Bubble {
    id: String,
    outbound: bool,
    sender: String,
    body: String,
    delivery: Delivery,
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
    let max_width = (ui.available_width() * BUBBLE_WIDTH).min(BUBBLE_MAX);
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
                ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                    if message.layout.show_sender {
                        ui.label(RichText::new(&message.sender).small().color(palette.text));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Max), |ui| {
                        ui.vertical(|ui| {
                            if !message.layout.time.is_empty() {
                                ui.label(RichText::new(&message.layout.time).small().color(meta));
                            }
                            match message.delivery {
                                Delivery::Sent => {}
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
                                                RichText::new("Retry").color(palette.accent),
                                            )
                                            .frame(false),
                                        )
                                        .clicked()
                                    {
                                        *retry = Some(message.id.clone());
                                    }
                                }
                            }
                        });
                        ui.add(
                            egui::Label::new(RichText::new(&message.body).color(body))
                                .selectable(true)
                                .wrap(),
                        );
                    });
                });
            });
    });
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

/// Height the message list leaves for the compose bar.
///
/// The outer frame and the field frame each add `space::M` above and below.
/// The Send button is at least [`SEND_MIN_HEIGHT`].
fn compose_reserve(row_height: f32, rows: usize) -> f32 {
    let rows = rows.clamp(1, COMPOSE_MAX_ROWS) as f32;
    let pad = space::M * 2.0;
    let field = row_height * rows + pad;
    pad + field.max(SEND_MIN_HEIGHT)
}

/// Multiline compose. Enter sends; Shift+Enter adds a line.
fn compose(ui: &mut egui::Ui, snapshot: &mut Snapshot, max_height: f32) {
    let compose_id = egui::Id::new("thread-compose");
    if snapshot.take_focus_compose() {
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
        if enter && !other && snapshot.compose_enter(shift) {
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
            if focused { 2.0 } else { 1.0 },
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
                egui::ScrollArea::vertical()
                    .id_salt("thread-compose-scroll")
                    .max_height(max_height)
                    .max_width(width)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut snapshot.compose)
                                .id(compose_id)
                                .frame(field)
                                .desired_rows(1)
                                .desired_width(width)
                                .hint_text(RichText::new("Message").color(palette.text3)),
                        );
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
                {
                    snapshot.send_compose();
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
    use super::{account_label, badge_text};
    use thinwire_protocol::AdapterStatus;

    #[test]
    fn compose_reserve_counts_the_frame_padding() {
        use super::compose_reserve;
        use crate::app::theme::space;

        let row = 20.0;
        let pad = space::M * 2.0;
        assert_eq!(compose_reserve(row, 1), pad + (row + pad).max(40.0));
        assert!(compose_reserve(row, 1) > row + 32.0);
        let ui = include_str!("ui.rs");
        let thread = &ui[ui.find("fn thread(").expect("thread")..];
        let thread = &thread[..thread.find("\nfn ").expect("next")];
        assert!(thread.contains("compose_reserve("));
        assert!(!thread.contains("+ 32.0"));
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
        use crate::app::snapshot::{AuthScreen, Snapshot};
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

    #[test]
    fn inbox_stays_blank_before_sign_in() {
        use super::show_no_chats;
        use crate::app::snapshot::Snapshot;

        let mut snapshot = Snapshot::new();
        assert!(!show_no_chats(&snapshot));
        snapshot.telegram_authorized = true;
        assert!(show_no_chats(&snapshot));
    }

    #[test]
    fn add_account_hides_when_telegram_is_linked() {
        use crate::app::snapshot::{InboxFilter, Snapshot};

        let mut snapshot = Snapshot::new();
        assert!(snapshot.can_add_account());
        snapshot.telegram_authorized = true;
        assert!(!snapshot.can_add_account());
        assert!(InboxFilter::chrome_filters().len() + 2 <= 6);
    }

    #[test]
    fn idle_status_and_tdlib_lines_hide_the_strip() {
        use super::{is_idle_status, public_status, status_strip_visible};
        use crate::app::snapshot::Snapshot;

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
    }

    #[test]
    fn load_failures_stay_on_the_status_strip() {
        use super::{load_failure_text, public_status, status_strip_visible};
        use crate::app::snapshot::Snapshot;

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
