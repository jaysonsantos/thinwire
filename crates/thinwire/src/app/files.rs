//! A document on a message. The preview is a thumbnail the protocol already
//! sent. The file downloads only when the user asks, and it opens with the
//! program the computer uses for that file.

use std::path::Path;
use std::process::Command;

use eframe::egui;
use thinwire_core::Intent;
use thinwire_protocol::{AudioPath, MessageFile, ProtocolId};

use super::theme;

pub(crate) fn show(
    ui: &mut egui::Ui,
    protocol: ProtocolId,
    conversation_id: &str,
    message_id: &str,
    file: &MessageFile,
    out: &mut Vec<Intent>,
) -> bool {
    let drew = file.preview().is_some_and(|preview| {
        super::photos::show(
            ui,
            protocol,
            conversation_id,
            message_id,
            preview,
            false,
            out,
        );
        true
    });
    let palette = theme::palette(ui);
    let waiting = is_waiting(ui.ctx(), message_id, file.file_id());
    if file.file().is_some() || file.failed() {
        set_waiting(ui.ctx(), message_id, file.file_id(), false);
    }
    let waiting = waiting && file.file().is_none() && !file.failed();
    ui.vertical(|ui| {
        ui.set_min_width(160.0);
        ui.label(egui::RichText::new(file.name()).color(palette.text));
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(file.kind_label())
                    .small()
                    .color(palette.text2),
            );
            if let Some(size) = file.size_label() {
                ui.label(egui::RichText::new(size).small().color(palette.text2));
            }
        });
        match control(file, waiting) {
            Control::Download | Control::Retry => {
                let label = if file.failed() { "Retry" } else { "Download" };
                if ui
                    .add(egui::Button::new(egui::RichText::new(label).small()).small())
                    .clicked()
                {
                    set_waiting(ui.ctx(), message_id, file.file_id(), true);
                    set_open_failed(ui.ctx(), message_id, false);
                    out.push(Intent::DownloadDocument {
                        protocol,
                        conversation_id: conversation_id.to_owned(),
                        message_id: message_id.to_owned(),
                        file_id: file.file_id(),
                    });
                }
            }
            Control::Downloading => {
                ui.label(
                    egui::RichText::new("Downloading…")
                        .small()
                        .color(palette.text2),
                );
            }
            Control::Open => {
                if ui
                    .add(egui::Button::new(egui::RichText::new("Open").small()).small())
                    .clicked()
                    && let Some(path) = file.file()
                {
                    set_open_failed(ui.ctx(), message_id, !launch(path));
                }
                if open_failed(ui.ctx(), message_id) {
                    ui.label(
                        egui::RichText::new("Couldn't open this file.")
                            .small()
                            .color(palette.error),
                    );
                }
            }
            Control::Plain => {}
        }
    });
    drew
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    Download,
    Downloading,
    Open,
    Retry,
    /// The message names a file and has no download id.
    Plain,
}

fn control(file: &MessageFile, waiting: bool) -> Control {
    if file.file().is_some() {
        Control::Open
    } else if file.file_id() <= 0 {
        Control::Plain
    } else if file.failed() {
        Control::Retry
    } else if waiting {
        Control::Downloading
    } else {
        Control::Download
    }
}

fn ask_id(message_id: &str, file_id: i32) -> egui::Id {
    egui::Id::unique(("thinwire-file-download", message_id, file_id))
}

fn is_waiting(ctx: &egui::Context, message_id: &str, file_id: i32) -> bool {
    ctx.data(|data| data.get_temp(ask_id(message_id, file_id)).unwrap_or(false))
}

fn set_waiting(ctx: &egui::Context, message_id: &str, file_id: i32, waiting: bool) {
    ctx.data_mut(|data| data.insert_temp(ask_id(message_id, file_id), waiting));
}

fn open_failed_id(message_id: &str) -> egui::Id {
    egui::Id::unique(("thinwire-file-open", message_id))
}

fn open_failed(ctx: &egui::Context, message_id: &str) -> bool {
    ctx.data(|data| data.get_temp(open_failed_id(message_id)).unwrap_or(false))
}

fn set_open_failed(ctx: &egui::Context, message_id: &str, failed: bool) {
    ctx.data_mut(|data| data.insert_temp(open_failed_id(message_id), failed));
}

fn launch(path: &AudioPath) -> bool {
    let Some(mut command) = open_command(path.as_path()) else {
        return false;
    };
    command.spawn().is_ok()
}

#[must_use]
pub(crate) fn open_command(path: &Path) -> Option<Command> {
    if !path.is_absolute() {
        return None;
    }
    #[cfg(windows)]
    {
        return windows_open_command(path);
    }
    #[cfg(not(windows))]
    {
        let program = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let mut command = Command::new(program);
        command.arg(path);
        Some(command)
    }
}

#[cfg(any(windows, test))]
fn windows_open_command(path: &Path) -> Option<Command> {
    let mut command = Command::new("cmd");
    command.arg("/C");
    command.arg("start");
    command.arg("");
    command.arg(path);
    Some(command)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use thinwire_protocol::{ImageBytes, MessagePhoto, PhotoSize};

    use super::*;

    struct Chip {
        file: MessageFile,
        intents: Vec<Intent>,
    }

    fn draw(ui: &mut egui::Ui, chip: &mut Chip) {
        super::super::theme::install(ui.ctx());
        show(
            ui,
            ProtocolId::Telegram,
            "telegram:1",
            "telegram:1:3",
            &chip.file,
            &mut chip.intents,
        );
    }

    fn harness(file: MessageFile) -> Harness<'static, Chip> {
        Harness::builder()
            .with_size(egui::vec2(420.0, 320.0))
            .build_ui_state(
                draw,
                Chip {
                    file,
                    intents: Vec::new(),
                },
            )
    }

    fn pdf() -> MessageFile {
        MessageFile::new(
            "notes.pdf",
            "application/pdf",
            2048,
            11,
            None,
            Some(MessagePhoto {
                width: 40,
                height: 52,
                mini: Some(ImageBytes::new(vec![0xff, 0xd8, 0xff])),
                sizes: vec![PhotoSize {
                    width: 40,
                    height: 52,
                    file_id: Some(32),
                    path: None,
                    bytes: None,
                }],
                has_spoiler: false,
                is_secret: false,
                failed: false,
            }),
        )
    }

    #[test]
    fn a_pdf_preview_is_shown_and_a_plain_file_still_downloads() {
        let mut pdf = harness(pdf());
        pdf.run_steps(2);
        pdf.get_by_label("notes.pdf");
        pdf.get_by_label("PDF");
        assert!(
            pdf.state()
                .intents
                .iter()
                .any(|intent| matches!(intent, Intent::LoadPhoto { file_id: 32, .. })),
            "the pdf preview was not requested: {:?}",
            pdf.state().intents
        );
        pdf.get_by_role_and_label(egui::accesskit::Role::Button, "Download")
            .click();
        pdf.run_steps(1);
        assert!(
            pdf.state()
                .intents
                .iter()
                .any(|intent| matches!(intent, Intent::DownloadDocument { file_id: 11, .. })),
            "the pdf was not downloaded: {:?}",
            pdf.state().intents
        );

        let plain = MessageFile::new("notes.txt", "text/plain", 12, 12, None, None);
        let mut plain = harness(plain);
        plain.run_steps(2);
        plain.get_by_label("notes.txt");
        assert!(
            plain
                .state()
                .intents
                .iter()
                .all(|intent| !matches!(intent, Intent::LoadPhoto { .. })),
            "a file with no preview asked for a photo: {:?}",
            plain.state().intents
        );
        plain
            .get_by_role_and_label(egui::accesskit::Role::Button, "Download")
            .click();
        plain.run_steps(1);
        assert!(
            plain
                .state()
                .intents
                .iter()
                .any(|intent| matches!(intent, Intent::DownloadDocument { file_id: 12, .. })),
            "the plain file was not downloaded: {:?}",
            plain.state().intents
        );
    }

    #[test]
    fn a_saved_file_opens_and_a_new_file_downloads() {
        let fresh = MessageFile::new("notes.pdf", "application/pdf", 2048, 11, None, None);
        assert_eq!(control(&fresh, false), Control::Download);
        assert_eq!(control(&fresh, true), Control::Downloading);
        let mut failed = fresh.clone();
        failed.mark_failed();
        assert_eq!(control(&failed, true), Control::Retry);
        let saved = AudioPath::new("/tmp/hidden-document.bin").expect("path");
        let ready = MessageFile::new("notes.pdf", "application/pdf", 2048, 11, Some(saved), None);
        assert_eq!(control(&ready, false), Control::Open);
        let nameless = MessageFile::new("notes.txt", "text/plain", 0, 0, None, None);
        assert_eq!(control(&nameless, false), Control::Plain);
    }

    #[test]
    fn open_command_takes_an_absolute_path() {
        assert!(open_command(Path::new("notes.pdf")).is_none());
        assert!(open_command(Path::new("")).is_none());
        let path = Path::new("/tmp/notes.pdf");
        let command = open_command(path).expect("command");
        let program = command.get_program().to_string_lossy();
        let expected = if cfg!(target_os = "windows") {
            "cmd"
        } else if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        assert_eq!(program, expected);
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.last().map(String::as_str), Some("/tmp/notes.pdf"));
    }

    #[test]
    fn a_program_file_does_not_open() {
        for extension in [
            "exe", "bat", "cmd", "com", "scr", "pif", "lnk", "app", "desktop", "sh", "EXE",
        ] {
            let path = Path::new("/tmp").join(format!("payload.{extension}"));
            assert!(open_command(&path).is_none(), "{extension} must not open");
        }
        assert!(open_command(Path::new("/tmp/notes.pdf")).is_some());
    }

    #[test]
    fn an_ampersand_in_the_name_is_not_a_cmd_separator() {
        let path = Path::new(r"C:\Users\a\x&calc.pdf");
        let command = windows_open_command(path).expect("command");
        let program = command.get_program().to_string_lossy();
        assert!(
            program.ends_with(r"\System32\rundll32.exe"),
            "open must not go through cmd: {program}"
        );
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.iter()
                .all(|arg| arg != "/C" && !arg.eq_ignore_ascii_case("start")),
            "a cmd start line splits on &: {args:?}"
        );
        assert_eq!(
            args,
            vec![
                "url.dll,FileProtocolHandler".to_string(),
                path.display().to_string(),
            ]
        );
        assert!(windows_open_command(Path::new(r"C:\Users\a\x&calc.exe")).is_none());
    }
}
