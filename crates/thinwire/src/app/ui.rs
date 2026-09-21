//! Shell: account switcher + inbox on the left, thread in the center.

use eframe::egui::{self, Color32, RichText};
use thinwire_protocol::{ProtocolId, SupportClass};

use super::auth;
use super::secrets::SecretStore;
use super::settings::{Settings, ThemeMode};
use super::snapshot::{AccountRow, InboxFilter, Snapshot};

const SUPPORTED: Color32 = Color32::from_rgb(96, 176, 128);
const EXPERIMENTAL: Color32 = Color32::from_rgb(214, 160, 64);
const CONSTRAINED: Color32 = Color32::from_rgb(196, 148, 88);
const MUTED: Color32 = Color32::from_rgb(160, 160, 168);

pub(crate) fn draw(
    ui: &mut egui::Ui,
    snapshot: &mut Snapshot,
    settings: &mut Settings,
    secrets: &SecretStore,
) {
    settings.apply(ui.ctx());
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
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading("thinwire");
            ui.separator();
            for filter in InboxFilter::ALL {
                if ui
                    .selectable_label(snapshot.filter == filter, filter.label())
                    .clicked()
                {
                    snapshot.set_filter(filter);
                }
            }
            ui.separator();
            ui.label("Search");
            ui.add(
                egui::TextEdit::singleline(&mut snapshot.search)
                    .desired_width(160.0)
                    .hint_text("title / participant"),
            );
            if ui.button("Refresh").clicked() {
                snapshot.refresh_visible();
            }
            if ui.button("Add account").clicked() {
                snapshot.open_add_account(secrets);
            }
            ui.separator();
            theme_control(ui, settings);
        });
        ui.add_space(2.0);
    });
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
    egui::Panel::top("status").show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Status").strong());
            ui.label(&snapshot.status_text);
            if snapshot.auth != super::snapshot::AuthScreen::Idle && ui.button("Cancel").clicked() {
                snapshot.cancel_auth(secrets);
            }
        });
        if snapshot.auth != super::snapshot::AuthScreen::Idle && !super::auth::tdlib_compiled() {
            ui.colored_label(
                Color32::from_rgb(214, 160, 64),
                super::auth::TDLIB_UNAVAILABLE_BANNER,
            );
        }
        if let Some(error) = &snapshot.error {
            let happened = &error.happened;
            let why = &error.why;
            let next = &error.next;
            ui.colored_label(
                Color32::from_rgb(200, 80, 80),
                format!("What happened: {happened}"),
            );
            ui.label(format!("Why: {why}"));
            ui.label(format!("What to do: {next}"));
        }
        ui.colored_label(
            MUTED,
            "Offline — no live protocol session. Linking and refresh stay on the tokio worker.",
        );
        ui.label(
            RichText::new(
                "Supported goals: Telegram via TDLib and Slack via OAuth. Experimental modules are not marketed here.",
            )
            .small()
            .color(MUTED),
        );
    });
}

fn left_panel(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    egui::Panel::left("switcher")
        .resizable(true)
        .default_size(280.0)
        .size_range(220.0..=400.0)
        .show(ui, |ui| {
            ui.heading("Accounts");
            ui.label(
                RichText::new("Experimental chips stay visible")
                    .small()
                    .color(MUTED),
            );
            ui.separator();

            let unread_by_protocol: Vec<u32> = snapshot
                .accounts
                .iter()
                .map(|account| snapshot.unread_for(account.caps.id))
                .collect();
            let mut clicked: Option<ProtocolId> = None;
            for (account, unread) in snapshot.accounts.iter().zip(unread_by_protocol) {
                if !snapshot.filter.shows_in_switcher(account.caps.id) {
                    continue;
                }
                if account_chip(
                    ui,
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

            ui.add_space(8.0);
            ui.heading("Inbox");
            ui.separator();
            inbox(ui, snapshot);
        });
}

fn account_chip(ui: &mut egui::Ui, account: &AccountRow, selected: bool, unread: u32) -> bool {
    let caps = account.caps;
    let mark = if account.linked { "●" } else { "○" };
    let name = caps.id.display_name();
    let label = if unread == 0 {
        format!("{mark} {name}")
    } else {
        format!("{mark} {name}  ({unread})")
    };
    let response = ui.selectable_label(selected, label);
    ui.colored_label(support_color(caps.support), caps.short_label);
    ui.label(
        RichText::new(format!("status: {}", account.status.as_str()))
            .small()
            .color(MUTED),
    );
    if !matches!(caps.id, ProtocolId::Telegram) {
        ui.label(
            RichText::new("not ready — no login UI this beat")
                .small()
                .color(MUTED),
        );
    }
    response.clicked()
}

fn inbox(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    let rows: Vec<(String, String, String, u32)> = snapshot
        .visible_conversations()
        .iter()
        .map(|row| {
            (
                row.id.clone(),
                row.title.clone(),
                row.preview.clone(),
                row.unread,
            )
        })
        .collect();

    if rows.is_empty() {
        ui.label(
            RichText::new("No conversations in this filter.")
                .italics()
                .color(MUTED),
        );
        return;
    }

    let mut clicked: Option<String> = None;
    for (id, title, preview, unread) in rows {
        let selected = snapshot.selected_conversation.as_deref() == Some(id.as_str());
        let label = if unread == 0 {
            title
        } else {
            format!("{title}  ({unread})")
        };
        if ui.selectable_label(selected, label).clicked() {
            clicked = Some(id);
        }
        ui.label(RichText::new(preview).small().weak());
    }
    if let Some(id) = clicked {
        snapshot.select_conversation(id);
    }
}

fn center_panel(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    egui::CentralPanel::default().show(ui, |ui| {
        if snapshot.auth != super::snapshot::AuthScreen::Idle {
            auth::draw(ui, snapshot, secrets);
            return;
        }

        if !snapshot.has_primary_account() {
            first_run(ui, snapshot, secrets);
            return;
        }

        thread(ui, snapshot);
    });
}

fn first_run(ui: &mut egui::Ui, snapshot: &mut Snapshot, secrets: &SecretStore) {
    ui.heading("Start with Telegram");
    if !super::auth::tdlib_compiled() {
        ui.colored_label(EXPERIMENTAL, super::auth::TDLIB_UNAVAILABLE_BANNER);
    }
    ui.label("Help: https://my.telegram.org — API development tools.");
    ui.add_space(8.0);
    if ui.button("Add Telegram").clicked() {
        snapshot.open_telegram(secrets);
    }
    ui.add_space(12.0);
    ui.label(
        RichText::new(
            "WhatsApp, Discord, and Slack are not ready. They are stubs, not login peers of Telegram this beat.",
        )
        .color(EXPERIMENTAL),
    );
}

fn thread(ui: &mut egui::Ui, snapshot: &mut Snapshot) {
    match snapshot.selected_conversation_row() {
        Some(conversation) => {
            ui.heading(&conversation.title);
            if let Some(account) = snapshot.selected_account() {
                ui.label(RichText::new(account.caps.detail).small().color(MUTED));
            }
        }
        None => {
            ui.heading("Thread");
            ui.label("Select a conversation in the inbox.");
        }
    }
    ui.separator();

    let messages: Vec<(bool, String, String)> = snapshot
        .selected_messages()
        .iter()
        .map(|message| {
            (
                message.outbound,
                message.sender.clone(),
                message.body.clone(),
            )
        })
        .collect();

    egui::ScrollArea::vertical()
        .auto_shrink([false, true])
        .max_height(ui.available_height() - 48.0)
        .show(ui, |ui| {
            if messages.is_empty() {
                ui.label(
                    RichText::new("No messages yet. Adapters push placeholders over the channel.")
                        .italics()
                        .color(MUTED),
                );
            }
            for (outbound, sender, body) in messages {
                ui.group(|ui| {
                    let who = if outbound { "you" } else { sender.as_str() };
                    ui.strong(who);
                    ui.label(body);
                });
                ui.add_space(4.0);
            }
        });

    ui.separator();
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut snapshot.compose)
                .desired_width(ui.available_width() - 72.0)
                .hint_text("Message"),
        );
        if ui.button("Send").clicked() {
            snapshot.send_compose_stub();
        }
    });
}

fn support_color(support: SupportClass) -> Color32 {
    match support {
        SupportClass::Supported => SUPPORTED,
        SupportClass::Experimental => EXPERIMENTAL,
        SupportClass::Constrained => CONSTRAINED,
    }
}
