//! Full-screen experimental WhatsApp gate.
//!
//! Compiled only with `whatsapp-web`. The QR and pair screen is unreachable
//! until the ban acknowledgement. This is not a supported messenger. The
//! client runs in the WhatsApp helper process (ADR 0013), which starts only
//! after that acknowledgement.

use eframe::egui::{self, RichText};
use thinwire_protocol::CRITIC_RISK_BULLETS;

use super::pairing_qr;
use super::theme;
use super::ui::edited;
use thinwire_core::state::{Snapshot, WhatsAppScreen};
use thinwire_core::{Intent, ProtocolId, SecretText, View, WhatsAppIntent};

/// Start of the link flow, in the middle column. The full notice is the next
/// step. With no helper program there is no button: the account row already
/// says to reinstall (ADR 0013 decision 6).
pub(crate) fn link_offer(ui: &mut egui::Ui, snapshot: &Snapshot, out: &mut Vec<Intent>) {
    if snapshot.selected_protocol != ProtocolId::WhatsApp || !snapshot.whatsapp_pairing_available()
    {
        return;
    }
    let linked = snapshot
        .accounts
        .iter()
        .any(|row| row.caps.id == ProtocolId::WhatsApp && row.linked());
    if linked || snapshot.helper_missing(ProtocolId::WhatsApp) {
        return;
    }
    ui.add_space(8.0);
    if ui.button("Link").clicked() {
        out.push(Intent::WhatsApp(WhatsAppIntent::OpenRiskGate));
    }
}

/// Why the pair screen cannot start when the helper program is missing.
const HELPER_MISSING_REASON: &str = "Needs the WhatsApp helper. Reinstall thinwire.";

/// Ban-risk heading from the gate. The pair screen repeats it under the QR.
const BAN_HEADING: &str = "WhatsApp: unofficial, ban risk";

/// Ban-risk line from the gate. The pair screen repeats it under the QR.
const BAN_WARN: &str = "Unofficial linked-device pairing can get a personal account banned. This is not a supported messenger.";

/// Shown directly under the QR image. The payload itself is not drawn.
const QR_WARNING: &str = "Do not share a screenshot of this code. Anyone who scans it can link a device to your WhatsApp.";

/// Opens the phone pair-code field. The ban gate stays accepted.
const PHONE_LINK: &str = "Link with phone number instead";

/// Step line under which the QR image is centered.
const SCAN_STEP: &str =
    "On your phone, open WhatsApp > Linked devices > Link a device, then scan this code.";

/// Replaces the QR image after `PairingQrCodesExhausted`.
const QR_EXPIRED: &str = "This code expired.";

/// Starts a new pairing after the codes are used up.
const QR_NEW_CODE: &str = "Get new code";

pub(crate) fn draw(ui: &mut egui::Ui, snapshot: &View<'_>, out: &mut Vec<Intent>) {
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| match snapshot.whatsapp_screen {
            WhatsAppScreen::Hidden => {
                pairing_qr::forget_pairing_qr(ui.ctx(), pairing_qr::WHATSAPP_QR);
            }
            WhatsAppScreen::RiskGate => {
                // The QR stays off this screen. The user confirms the risk first.
                pairing_qr::forget_pairing_qr(ui.ctx(), pairing_qr::WHATSAPP_QR);
                risk_gate(ui, out);
            }
            WhatsAppScreen::Pair => pair_screen(ui, snapshot, out),
        });
    });
}

fn risk_gate(ui: &mut egui::Ui, out: &mut Vec<Intent>) {
    ui.heading(BAN_HEADING);
    ui.colored_label(theme::palette(ui).warn, BAN_WARN);
    ui.add_space(8.0);
    for bullet in CRITIC_RISK_BULLETS {
        ui.label(bullet);
        ui.add_space(6.0);
    }
    ui.label("No QR code and no pair code are shown on this screen.");
    ui.add_space(12.0);
    if ui
        .button("I understand — this can ban my account")
        .clicked()
    {
        out.push(Intent::WhatsApp(WhatsAppIntent::AcknowledgeRisk));
    }
    if ui.button("Back").clicked() {
        out.push(Intent::WhatsApp(WhatsAppIntent::CloseGate));
    }
}

fn pair_screen(ui: &mut egui::Ui, snapshot: &Snapshot, out: &mut Vec<Intent>) {
    ui.heading("Experimental pairing");
    ui.label(
        "The worker stays idle until you start it. A phone number stays in memory and is not placed on the command channel.",
    );
    ui.add_space(8.0);
    ui.label("Optional phone for a pair code (digits). Leave blank for a QR payload only.");
    if let Some(value) = edited(ui, &snapshot.whatsapp_phone, |text| {
        egui::TextEdit::singleline(text)
            .hint_text("15551234567")
            .desired_width(240.0)
    }) {
        out.push(Intent::WhatsApp(WhatsAppIntent::SetPhone(SecretText::new(
            value,
        ))));
    }
    if let Some(note) = snapshot.whatsapp_phone_note {
        ui.colored_label(theme::palette(ui).warn, note);
    }
    // With no helper program no pairing can start. The core refuses it
    // too. Show the reason next to the disabled button.
    let missing = snapshot.helper_missing(ProtocolId::WhatsApp);
    if missing {
        ui.colored_label(theme::palette(ui).warn, HELPER_MISSING_REASON);
    }
    if ui
        .add_enabled(
            !snapshot.whatsapp_started && !missing,
            egui::Button::new("Start experimental pairing"),
        )
        .clicked()
    {
        out.push(Intent::WhatsApp(WhatsAppIntent::BeginLink));
    }
    // This screen draws no account row and no error block. Show the helper
    // state here: a start that takes long, a restart, or a helper that
    // stopped, with what to do.
    if let Some(notice) = snapshot.helper_notice(ProtocolId::WhatsApp) {
        ui.add_space(8.0);
        ui.colored_label(theme::palette(ui).warn, notice.label);
        if let Some(error) = notice.error {
            ui.label(format!("What happened: {}", error.happened));
            ui.label(format!("Why: {}", error.why));
            ui.label(format!("What to do: {}", error.next));
            ui.label("You can also start the pairing again here.");
        }
    }
    // The payload can wrap as several `2@` lines. It is one string. The image
    // encodes that whole string. The text is not drawn and not logged.
    if snapshot.whatsapp_qr_exhausted {
        ui.add_space(8.0);
        ui.label(QR_EXPIRED);
        if ui.button(QR_NEW_CODE).clicked() {
            out.push(Intent::WhatsApp(WhatsAppIntent::BeginLink));
        }
        pairing_qr::forget_pairing_qr(ui.ctx(), pairing_qr::WHATSAPP_QR);
    } else if let Some(qr) = snapshot.whatsapp_qr.as_deref() {
        ui.add_space(8.0);
        ui.label(SCAN_STEP);
        pairing_qr::show_pairing_qr(
            ui,
            pairing_qr::WHATSAPP_QR,
            snapshot.whatsapp_pairing_generation(),
            qr,
        );
        ui.add_space(8.0);
        ui.colored_label(theme::palette(ui).warn, QR_WARNING);
        ui.heading(BAN_HEADING);
        ui.colored_label(theme::palette(ui).warn, BAN_WARN);
        if ui
            .add(egui::Button::new(RichText::new(PHONE_LINK).small().weak()).frame(false))
            .clicked()
        {
            out.push(Intent::WhatsApp(WhatsAppIntent::LinkWithPhone));
        }
    } else {
        pairing_qr::forget_pairing_qr(ui.ctx(), pairing_qr::WHATSAPP_QR);
    }
    if let Some(code) = &snapshot.whatsapp_pair_code {
        ui.add_space(8.0);
        ui.label("Pair code from the worker. Do not commit it or log it.");
        ui.monospace(code);
    }
    ui.add_space(12.0);
    ui.label(
        RichText::new("Cancel returns to the shell and stops the worker task.")
            .small()
            .weak(),
    );
    if ui.button("Cancel pairing").clicked() {
        out.push(Intent::WhatsApp(WhatsAppIntent::CancelLink));
    }
}

#[cfg(test)]
mod tests {
    use eframe::egui::accesskit::Role;
    use egui_kittest::Harness;
    use egui_kittest::kittest::{By, Queryable};
    use thinwire_core::secrets::SecretStore;
    use thinwire_core::settings::Settings;
    use thinwire_core::state::Snapshot;
    use thinwire_core::{Intent, View, WhatsAppIntent};
    use thinwire_protocol::{
        AccountState, AdapterEvent, AdapterStatus, ProtocolId, RedactedPairingSecret,
        WHATSAPP_QR_EXHAUSTED, WhatsAppPhoneVault,
    };

    use super::{
        BAN_HEADING, BAN_WARN, PHONE_LINK, QR_EXPIRED, QR_NEW_CODE, QR_WARNING, SCAN_STEP,
    };
    use crate::app::pairing_qr::{self, QR_IMAGE_LABEL};
    use crate::app::theme;

    /// One payload that wraps as two `2@` lines. The screen encodes all of it.
    const PAYLOAD: &str = "2@alpha-line-of-the-same-pairing-payload-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n2@beta-line-of-the-same-pairing-payload-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    struct PairUi {
        snapshot: Snapshot,
        phone: WhatsAppPhoneVault,
        store: SecretStore,
        settings: Settings,
        intents: Vec<Intent>,
    }

    impl PairUi {
        fn risk_gate() -> Self {
            let mut snapshot = Snapshot::new();
            snapshot.open_whatsapp_risk_gate();
            Self {
                snapshot,
                phone: WhatsAppPhoneVault::new(),
                store: SecretStore::memory(),
                settings: Settings::in_memory(),
                intents: Vec::new(),
            }
        }

        fn show_payload(&mut self, payload: &str) {
            self.snapshot.acknowledge_whatsapp_risk();
            self.snapshot.begin_whatsapp_link(&self.phone);
            let generation = self.snapshot.whatsapp_pairing_generation();
            self.snapshot.apply(AdapterEvent::WhatsAppQr {
                code: RedactedPairingSecret::new(payload),
                generation,
            });
        }

        fn exhaust(&mut self) {
            // The helper reports Linking, then the used-up codes, then Unlinked.
            self.snapshot.apply(AdapterEvent::Account {
                protocol: ProtocolId::WhatsApp,
                state: AccountState::Linking,
            });
            self.snapshot.apply(AdapterEvent::Status {
                protocol: ProtocolId::WhatsApp,
                status: AdapterStatus::Error,
                detail: WHATSAPP_QR_EXHAUSTED.into(),
            });
            self.snapshot.apply(AdapterEvent::Account {
                protocol: ProtocolId::WhatsApp,
                state: AccountState::Unlinked,
            });
        }

        fn restart(&mut self) -> u64 {
            let before = self.snapshot.whatsapp_pairing_generation();
            self.snapshot.begin_whatsapp_link(&self.phone);
            let generation = self.snapshot.whatsapp_pairing_generation();
            assert_ne!(generation, before, "Get new code starts a new pairing");
            assert!(!self.snapshot.whatsapp_qr_exhausted);
            generation
        }

        fn open_phone_link(&mut self) {
            let Self {
                snapshot, phone, ..
            } = self;
            snapshot.open_whatsapp_phone_link(phone);
        }

        fn begin(&mut self) {
            let Self {
                snapshot, phone, ..
            } = self;
            snapshot.begin_whatsapp_link(phone);
        }
    }

    fn draw_gate(ui: &mut egui::Ui, state: &mut PairUi) {
        theme::install(ui.ctx());
        let view = View::from_parts(&state.snapshot, &state.store, &state.settings);
        super::draw(ui, &view, &mut state.intents);
    }

    fn image_rects(harness: &Harness<'_, PairUi>) -> Vec<egui::Rect> {
        harness
            .query_all(By::new().role(Role::Image).label(QR_IMAGE_LABEL))
            .map(|node| node.rect())
            .collect()
    }

    /// The pair screen draws one QR image for the whole payload, never the
    /// payload text. After the codes are used up, the image is replaced by
    /// "This code expired." and "Get new code".
    #[test]
    fn the_pair_screen_shows_one_qr_and_replaces_it_when_the_code_expires() {
        let mut harness = Harness::builder()
            .with_size(egui::vec2(720.0, 1100.0))
            .build_ui_state(draw_gate, PairUi::risk_gate());
        harness.run_steps(2);
        harness.get_by_label("No QR code and no pair code are shown on this screen.");
        harness.get_by_label_contains("can get a personal account banned");
        assert!(
            image_rects(&harness).is_empty(),
            "the risk gate has no QR image"
        );
        assert!(harness.query_by_label_contains("2@").is_none());

        harness.state_mut().show_payload(PAYLOAD);
        harness.run_steps(2);
        let step = harness.get_by_label(SCAN_STEP).rect();
        let images = image_rects(&harness);
        assert_eq!(images.len(), 1, "one image for the whole payload");
        let image = images[0];
        assert!(
            image.top() + 1.0 >= step.bottom(),
            "the QR is under the step line"
        );
        let warning = harness.get_by_label(QR_WARNING).rect();
        assert!(
            warning.top() + 1.0 >= image.bottom(),
            "the warning is directly under the QR"
        );
        let ban = harness.get_by_label(BAN_WARN).rect();
        assert!(
            ban.top() + 1.0 >= warning.bottom(),
            "the ban-risk line is under the QR"
        );
        harness.get_by_label(BAN_HEADING);
        let mid = harness.ctx.content_rect().center().x;
        assert!(
            (image.center().x - mid).abs() < 8.0,
            "the QR is centered, image {} window {mid}",
            image.center().x
        );
        assert!(
            (image.width() - pairing_qr::DISPLAY_SIDE).abs() < 1.0
                && (image.height() - pairing_qr::DISPLAY_SIDE).abs() < 1.0,
            "the QR uses the Signal size"
        );
        assert!(harness.query_by_label(PAYLOAD).is_none());
        assert!(harness.query_by_label_contains("2@alpha-line").is_none());
        assert!(harness.query_by_label_contains("2@beta-line").is_none());
        assert!(harness.query_by_label(QR_EXPIRED).is_none());

        harness.run_steps(3);
        assert_eq!(
            image_rects(&harness).len(),
            1,
            "later frames keep the one image"
        );

        harness.state_mut().exhaust();
        harness.run_steps(2);
        assert!(
            image_rects(&harness).is_empty(),
            "the expired code is not an image"
        );
        harness.get_by_label(QR_EXPIRED);
        assert!(
            harness.query_by_label(QR_WARNING).is_none(),
            "the expired state has no warning"
        );
        assert!(harness.query_by_label(SCAN_STEP).is_none());
        assert!(harness.query_by_label_contains("2@").is_none());
        assert!(harness.query_by_label(PAYLOAD).is_none());

        harness.state_mut().intents.clear();
        harness
            .get_by_role_and_label(Role::Button, QR_NEW_CODE)
            .click();
        harness.run_steps(1);
        assert_eq!(
            harness.state().intents,
            vec![Intent::WhatsApp(WhatsAppIntent::BeginLink)]
        );
        let generation = harness.state_mut().restart();
        harness
            .state_mut()
            .snapshot
            .apply(AdapterEvent::WhatsAppQr {
                code: RedactedPairingSecret::new("2@fresh-code"),
                generation,
            });
        harness.run_steps(2);
        assert_eq!(image_rects(&harness).len(), 1, "the new code is one image");
        assert!(harness.query_by_label(QR_EXPIRED).is_none());
        assert!(harness.query_by_label_contains("2@fresh-code").is_none());
        harness.get_by_label(SCAN_STEP);
    }

    /// "Link with phone number instead" leaves the QR, keeps the accepted ban
    /// gate, and the existing phone field can start a pair-code pairing.
    #[test]
    fn the_phone_link_keeps_the_risk_gate_and_opens_the_pair_code_field() {
        use thinwire_core::state::WhatsAppScreen;
        use thinwire_protocol::AdapterCommand;

        let mut harness = Harness::builder()
            .with_size(egui::vec2(720.0, 1200.0))
            .build_ui_state(draw_gate, PairUi::risk_gate());
        harness.state_mut().show_payload(PAYLOAD);
        harness.run_steps(2);
        let image = image_rects(&harness)[0];
        let link = harness
            .get_by_role_and_label(Role::Button, PHONE_LINK)
            .rect();
        assert!(
            link.top() + 1.0 >= image.bottom(),
            "the phone link is under the QR"
        );
        harness.state_mut().intents.clear();
        harness
            .get_by_role_and_label(Role::Button, PHONE_LINK)
            .click();
        harness.run_steps(1);
        assert_eq!(
            harness.state().intents,
            vec![Intent::WhatsApp(WhatsAppIntent::LinkWithPhone)]
        );

        {
            let state = harness.state_mut();
            state.open_phone_link();
            assert_eq!(state.snapshot.whatsapp_screen, WhatsAppScreen::Pair);
            assert!(state.snapshot.whatsapp_qr.is_none());
            assert!(!state.snapshot.whatsapp_started);
            assert!(
                state
                    .snapshot
                    .take_commands()
                    .iter()
                    .any(|command| matches!(command, AdapterCommand::WhatsAppCancelLink)),
                "the QR pairing stops"
            );
            state.snapshot.set_whatsapp_phone("15551234567".into());
            state.begin();
            assert_eq!(
                state.phone.phone().as_deref(),
                Some("15551234567"),
                "the pair-code path uses the phone field"
            );
            assert!(
                state
                    .snapshot
                    .take_commands()
                    .iter()
                    .any(|command| matches!(command, AdapterCommand::WhatsAppBeginLink { .. })),
                "the accepted ban gate still allows a pairing"
            );
            let generation = state.snapshot.whatsapp_pairing_generation();
            state.snapshot.apply(AdapterEvent::WhatsAppPairCode {
                code: RedactedPairingSecret::new("1234-5678"),
                generation,
            });
        }
        harness.run_steps(2);
        assert!(image_rects(&harness).is_empty(), "the QR stays off");
        harness.get_by_label(
            "Optional phone for a pair code (digits). Leave blank for a QR payload only.",
        );
        harness.get_by_label("1234-5678");
        assert!(harness.query_by_label(PAYLOAD).is_none());
        assert!(harness.query_by_label_contains("2@").is_none());
    }
}
