//! Three-pane shell: accounts, conversations, messages.

use eframe::egui::{self, Color32, RichText};

use crate::protocols::{ProtocolId, SupportClass};

use super::snapshot::{AccountRow, Snapshot};

const SUPPORTED: Color32 = Color32::from_rgb(96, 176, 128);
const EXPERIMENTAL: Color32 = Color32::from_rgb(214, 160, 64);
const CONSTRAINED: Color32 = Color32::from_rgb(196, 148, 88);
const MUTED: Color32 = Color32::from_rgb(160, 160, 168);

pub(crate) fn draw(ctx: &egui::Context, snapshot: &mut Snapshot) {
    egui::TopBottomPanel::top("top").show(ctx, |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading("thinwire");
            ui.separator();
            ui.label("Lightweight multi-protocol messenger");
        });
        ui.label(
            RichText::new(
                "UI thread stays free. Protocol I/O runs on tokio and is polled from a channel.",
            )
            .small()
            .color(MUTED),
        );
        ui.add_space(2.0);
    });

    egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
        ui.add_space(2.0);
        ui.label(
            RichText::new(
                "WhatsApp, Signal, and Discord personal clients are experimental or constrained — not reliable. Telegram (TDLib) and Slack (OAuth) are the supported goals.",
            )
            .small()
            .color(MUTED),
        );
        ui.add_space(2.0);
    });

    accounts_panel(ctx, snapshot);
    conversations_panel(ctx, snapshot);
    messages_panel(ctx, snapshot);
}

fn accounts_panel(ctx: &egui::Context, snapshot: &mut Snapshot) {
    egui::SidePanel::left("accounts")
        .resizable(true)
        .default_width(230.0)
        .width_range(200.0..=320.0)
        .show(ctx, |ui| {
            ui.heading("Accounts");
            ui.label(RichText::new("All five protocols").small().color(MUTED));
            ui.separator();

            let mut clicked: Option<ProtocolId> = None;
            for account in &snapshot.accounts {
                if account_row(ui, account, snapshot.selected_protocol == account.caps.id) {
                    clicked = Some(account.caps.id);
                }
                ui.add_space(6.0);
            }
            if let Some(protocol) = clicked {
                snapshot.select_protocol(protocol);
            }
        });
}

fn account_row(ui: &mut egui::Ui, account: &AccountRow, selected: bool) -> bool {
    let caps = account.caps;
    let response = ui.selectable_label(selected, caps.id.display_name());
    ui.colored_label(support_color(caps.support), caps.short_label);
    ui.label(
        RichText::new(format!("status: {}", account.status.as_str()))
            .small()
            .color(MUTED),
    );
    ui.label(RichText::new(&account.detail).small().weak());
    response.clicked()
}

fn conversations_panel(ctx: &egui::Context, snapshot: &mut Snapshot) {
    egui::SidePanel::left("conversations")
        .resizable(true)
        .default_width(250.0)
        .width_range(200.0..=360.0)
        .show(ctx, |ui| {
            ui.heading("Conversations");
            if let Some(account) = snapshot.selected_account() {
                ui.label(
                    RichText::new(account.caps.short_label)
                        .small()
                        .color(support_color(account.caps.support)),
                );
            }
            ui.separator();

            let rows: Vec<(String, String, String)> = snapshot
                .conversations()
                .iter()
                .map(|row| (row.id.clone(), row.title.clone(), row.preview.clone()))
                .collect();

            if rows.is_empty() {
                ui.label(
                    RichText::new("Waiting for adapter events…")
                        .italics()
                        .color(MUTED),
                );
                return;
            }

            let mut clicked: Option<String> = None;
            for (id, title, preview) in rows {
                let selected = snapshot.selected_conversation.as_deref() == Some(id.as_str());
                if ui.selectable_label(selected, title).clicked() {
                    clicked = Some(id);
                } else {
                    ui.label(RichText::new(preview).small().weak());
                }
            }
            if let Some(id) = clicked {
                snapshot.select_conversation(id);
            }
        });
}

fn messages_panel(ctx: &egui::Context, snapshot: &Snapshot) {
    egui::CentralPanel::default().show(ctx, |ui| {
        match snapshot.selected_conversation_row() {
            Some(conversation) => {
                ui.heading(&conversation.title);
                if let Some(account) = snapshot.selected_account() {
                    ui.label(RichText::new(account.caps.detail).small().color(MUTED));
                }
            }
            None => {
                ui.heading("Messages");
                ui.label("Select a protocol and conversation.");
            }
        }
        ui.separator();

        let messages = snapshot.selected_messages();
        if messages.is_empty() {
            ui.label(
                RichText::new("No messages yet. Adapters push placeholders over the channel.")
                    .italics()
                    .color(MUTED),
            );
            return;
        }

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for message in messages {
                    ui.group(|ui| {
                        let who = if message.outbound {
                            "you"
                        } else {
                            message.sender.as_str()
                        };
                        ui.strong(who);
                        ui.label(&message.body);
                    });
                    ui.add_space(4.0);
                }
            });
    });
}

fn support_color(support: SupportClass) -> Color32 {
    match support {
        SupportClass::Supported => SUPPORTED,
        SupportClass::Experimental => EXPERIMENTAL,
        SupportClass::Constrained => CONSTRAINED,
    }
}
