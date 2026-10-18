//! Underlined links in a message, and the confirm step when the label hides
//! the address.
//!
//! Opening a link happens off the UI thread. The address is not logged.
//! [`open_url`] checks the scheme again. A blocked scheme is plain text.
//! `http` and `https` go through `webbrowser` with its `hardened` feature.
//! `mailto` goes to the OS handler as one argument.

use eframe::egui::text::LayoutJob;
use eframe::egui::{self, Color32, FontId, Stroke, TextFormat};

use thinwire_protocol::{MessageLink, openable_url};

/// A link the user has not opened yet, because the label is a different address.
#[derive(Clone)]
struct PendingLink {
    url: String,
    /// Open button from the last frame. Enter activates it only when it has focus.
    open_id: Option<egui::Id>,
}

/// True when the label is not the address the app will open.
///
/// A label that is not a URL hides the host. `docs` can point at
/// `docs.example` or at `evil.example`. The app asks in both cases.
/// An email label that is the same `mailto` address does not ask.
/// `www.a.com` and `https://www.a.com` are the same address.
/// A different scheme, host, or path asks.
#[must_use]
pub(crate) fn needs_confirm(label: &str, target: &str) -> bool {
    if same_mailto(label, target) {
        return false;
    }
    if !label_looks_like_url(label) {
        return true;
    }
    normalize_url(label) != normalize_url(target)
}

/// The label is an email address, and `target` is that same `mailto`.
fn same_mailto(label: &str, target: &str) -> bool {
    match (mail_address(label), mail_address(target)) {
        (Some(label), Some(target)) => label == target,
        _ => false,
    }
}

fn mail_address(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.contains(char::is_whitespace) {
        return None;
    }
    let lower = value.to_ascii_lowercase();
    let address = lower.strip_prefix("mailto:").unwrap_or(&lower);
    if address.contains("://") || !address.contains('@') {
        return None;
    }
    let address = address.split(['?', '#']).next().unwrap_or(address);
    if address.is_empty() || address.starts_with('@') || address.ends_with('@') {
        return None;
    }
    Some(address.to_string())
}

fn label_looks_like_url(label: &str) -> bool {
    let label = label.trim();
    if label.is_empty() || label.contains(char::is_whitespace) {
        return false;
    }
    let lower = label.to_ascii_lowercase();
    if lower.starts_with("mailto:") || (label.contains('@') && !label.contains("://")) {
        return false;
    }
    lower.contains("://") || lower.starts_with("www.") || label.contains('.')
}

/// Trim, add `https` when there is no scheme, lowercase the scheme and the
/// host, and drop one trailing slash.
fn normalize_url(value: &str) -> String {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("mailto:") {
        return lower;
    }
    let with_scheme = if value.contains("://") {
        value.to_string()
    } else {
        format!("https://{value}")
    };
    let Some((scheme, rest)) = with_scheme.split_once("://") else {
        return with_scheme;
    };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let (host, path) = rest
        .split_once('/')
        .map(|(host, path)| (host, format!("/{path}")))
        .unwrap_or((rest, String::new()));
    format!(
        "{}://{}{path}",
        scheme.to_ascii_lowercase(),
        host.to_ascii_lowercase()
    )
}

/// The link whose label contains `index`, if any.
#[must_use]
pub(crate) fn link_at(links: &[MessageLink], index: usize) -> Option<&MessageLink> {
    links
        .iter()
        .find(|link| index >= link.start && index < link.end)
}

/// Body text with underlined links. Plain text stays a normal label.
pub(crate) fn layout_job(
    body: &str,
    links: &[MessageLink],
    text: Color32,
    link: Color32,
    font: FontId,
) -> LayoutJob {
    let plain = TextFormat {
        font_id: font.clone(),
        color: text,
        ..Default::default()
    };
    let linked = TextFormat {
        font_id: font,
        color: link,
        underline: Stroke::new(1.0, link),
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    let mut cursor = 0usize;
    for span in links {
        if openable_url(&span.url).is_none() {
            continue;
        }
        let start = span.start.min(body.len());
        let end = span.end.min(body.len());
        if start < cursor
            || start >= end
            || !body.is_char_boundary(start)
            || !body.is_char_boundary(end)
        {
            continue;
        }
        if cursor < start {
            job.append(&body[cursor..start], 0.0, plain.clone());
        }
        job.append(&body[start..end], 0.0, linked.clone());
        cursor = end;
    }
    if cursor < body.len() {
        job.append(&body[cursor..], 0.0, plain);
    }
    job
}

pub(crate) fn remember_confirm(ctx: &egui::Context, url: &str) {
    let same = ctx
        .data(|data| data.get_temp::<PendingLink>(confirm_id()))
        .is_some_and(|pending| pending.url == url);
    if same {
        return;
    }
    ctx.data_mut(|data| {
        data.insert_temp(
            confirm_id(),
            PendingLink {
                url: url.to_owned(),
                open_id: None,
            },
        );
    });
}

/// Close the confirm dialog. `true` when it was open.
pub(crate) fn take_escape(ctx: &egui::Context) -> bool {
    let open = ctx.data(|data| data.get_temp::<PendingLink>(confirm_id()).is_some());
    if open {
        let _ = ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        ctx.data_mut(|data| data.remove::<PendingLink>(confirm_id()));
    }
    open
}

/// Enter while the confirm dialog is open.
///
/// `true` when the dialog takes the key, so the center screen does not also
/// act on it. The key cancels, unless focus is already on the Open button.
pub(crate) fn take_enter(ctx: &egui::Context) -> bool {
    let Some(pending) = ctx.data(|data| data.get_temp::<PendingLink>(confirm_id())) else {
        return false;
    };
    let open_focused = pending
        .open_id
        .is_some_and(|id| ctx.memory(|memory| memory.focused() == Some(id)));
    if open_focused {
        return true;
    }
    let _ = ctx.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
    ctx.data_mut(|data| data.remove::<PendingLink>(confirm_id()));
    true
}

/// The host the dialog shows.
///
/// [`url::Url::host_str`] is the punycode form of an international name.
/// A `mailto` address has no host. The domain after `@` is read the same way.
fn confirm_host(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    if let Some(host) = parsed.host_str() {
        return Some(host.to_owned());
    }
    if !parsed.scheme().eq_ignore_ascii_case("mailto") {
        return None;
    }
    let domain = parsed.path().rsplit_once('@')?.1;
    let domain = domain.split(['?', '#']).next().unwrap_or(domain);
    let domain = percent_decode(domain)?;
    if domain.is_empty() {
        return None;
    }
    url::Url::parse(&format!("https://{domain}"))
        .ok()
        .and_then(|probe| probe.host_str().map(ToOwned::to_owned))
}

/// Decode `%HH` bytes. `None` when an escape is cut off or the bytes are not UTF-8.
fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return None;
            }
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn open_button_label(url: &str) -> String {
    match confirm_host(url) {
        Some(host) => format!("Open {host}"),
        None => "Open link".to_owned(),
    }
}

/// Semibold is the bold weight of the UI font.
fn host_font() -> egui::FontId {
    egui::FontId::new(super::theme::size::BODY, super::theme::semibold())
}

/// The confirm dialog. It shows the real host and does not log the address.
pub(crate) fn confirm(ui: &mut egui::Ui) {
    let Some(pending) = ui
        .ctx()
        .data(|data| data.get_temp::<PendingLink>(confirm_id()))
    else {
        return;
    };
    let url = pending.url.clone();
    let host = confirm_host(&url);
    let open_label = open_button_label(&url);
    let screen = ui.ctx().content_rect();
    let mut dismiss = false;
    let mut open_id = None;
    egui::Area::new(egui::Id::new("link-confirm"))
        .order(egui::Order::Foreground)
        .fixed_pos(screen.min)
        .show(ui.ctx(), |ui| {
            let _ = ui.allocate_rect(screen, egui::Sense::click());
            ui.painter()
                .rect_filled(screen, 0.0, egui::Color32::from_black_alpha(120));
            egui::Window::new("Open link?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ui.ctx(), |ui| {
                    ui.set_max_width(420.0);
                    ui.label("This link text does not match the address.");
                    if let Some(host) = &host {
                        ui.label(
                            egui::RichText::new(host)
                                .font(host_font())
                                .color(super::theme::palette(ui).text),
                        );
                    }
                    ui.label(url.as_str());
                    ui.horizontal(|ui| {
                        let open_button = ui.button(&open_label);
                        open_id = Some(open_button.id);
                        if open_button.clicked() {
                            open_url(&url);
                            dismiss = true;
                        }
                        let cancel = ui.button("Cancel");
                        let focused = ui.memory(|memory| memory.focused());
                        if focused != Some(open_button.id) && focused != Some(cancel.id) {
                            cancel.request_focus();
                        }
                        if cancel.clicked() {
                            dismiss = true;
                        }
                        // The center panel also handles these keys. This covers
                        // a frame where that panel did not run.
                        let (enter, escape) = ui.input(|input| {
                            (
                                input.key_pressed(egui::Key::Enter),
                                input.key_pressed(egui::Key::Escape),
                            )
                        });
                        if escape || (enter && focused != Some(open_button.id)) {
                            dismiss = true;
                        }
                    });
                });
        });
    let ctx = ui.ctx();
    if dismiss {
        ctx.data_mut(|data| data.remove::<PendingLink>(confirm_id()));
    } else if let Some(id) = open_id {
        ctx.data_mut(|data| {
            data.insert_temp(
                confirm_id(),
                PendingLink {
                    url,
                    open_id: Some(id),
                },
            );
        });
    }
}

pub(crate) fn interact(
    ui: &egui::Ui,
    response: &egui::Response,
    galley_pos: egui::Pos2,
    galley: &egui::Galley,
    body: &str,
    links: &[MessageLink],
) {
    let Some(pointer) = response.hover_pos().or(response.interact_pointer_pos()) else {
        return;
    };
    let index = galley_index(galley, galley_pos, pointer, body);
    let Some(link) = link_at(links, index) else {
        return;
    };
    if openable_url(&link.url).is_none() {
        return;
    }
    let label = body.get(link.start..link.end).unwrap_or("");
    response.clone().on_hover_text(&link.url);
    if response.clicked() {
        if needs_confirm(label, &link.url) {
            remember_confirm(ui.ctx(), &link.url);
        } else {
            open_url(&link.url);
        }
    }
    let url = link.url.clone();
    response.clone().context_menu(|ui| {
        if ui.button("Copy link").clicked() {
            ui.ctx().copy_text(url.clone());
            ui.close();
        }
    });
}

pub(crate) fn open_url(url: &str) {
    // `openable_url` is the gate. A link that skipped `links_in` stops here.
    let Some(url) = openable_url(url) else {
        return;
    };
    dispatch_open(url);
}

#[cfg(test)]
thread_local! {
    static OPENED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn dispatch_open(url: String) {
    #[cfg(test)]
    {
        OPENED.with(|opened| opened.borrow_mut().push(url));
    }
    #[cfg(not(test))]
    {
        // The UI thread only queues the open. The worker talks to the OS.
        std::thread::spawn(move || {
            if !open_on_worker(&url) {
                tracing::debug!("link did not open");
            }
        });
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Opener {
    /// `webbrowser` with `hardened`. Only `http` and `https` succeed there.
    Browser,
    /// The OS handler. `hardened` rejects `mailto`.
    Mail,
}

fn opener(url: &str) -> Option<Opener> {
    let url = openable_url(url)?;
    if is_mailto(&url) {
        Some(Opener::Mail)
    } else {
        Some(Opener::Browser)
    }
}

fn is_mailto(url: &str) -> bool {
    url.split_once(':')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("mailto"))
}

#[cfg_attr(test, allow(dead_code))]
fn open_on_worker(url: &str) -> bool {
    match opener(url) {
        Some(Opener::Mail) => open_mailto(url).is_ok(),
        Some(Opener::Browser) => webbrowser::open(url).is_ok(),
        None => false,
    }
}

#[cfg_attr(test, allow(dead_code))]
fn open_mailto(url: &str) -> std::io::Result<()> {
    let mut command = mailto_command(url)?;
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("mailto handler failed"))
    }
}

/// Start the OS mail handler.
///
/// The address is one argument. It is not a shell string.
/// On Windows, `ShellExecuteW` needs `unsafe`, which this workspace forbids.
/// `rundll32 url.dll,FileProtocolHandler` passes the address with no `cmd`.
/// `cmd /c` is command-line mode: `%%` stays as two percent signs there,
/// and `%name%` expands inside quotes.
fn mailto_command(url: &str) -> std::io::Result<std::process::Command> {
    #[cfg(windows)]
    {
        windows_mailto_command(url)
    }
    #[cfg(not(windows))]
    {
        let program = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let mut command = std::process::Command::new(program);
        command.arg(url);
        Ok(command)
    }
}

/// `rundll32` in System32, so a `rundll32` earlier on `PATH` is not used.
#[cfg(any(windows, test))]
fn rundll32_program() -> String {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
    let root = root.trim_end_matches(['\\', '/']);
    format!(r"{root}\System32\rundll32.exe")
}

/// One `rundll32` argument.
///
/// A quote, a space, or a control character is refused.
/// `Command` would quote a space, and `rundll32` keeps those quotes in the address.
/// `%` stays as written. This path does not run `cmd`.
/// `url.dll` exports `FileProtocolHandler` as an ANSI entry.
/// A percent-encoded address is ASCII, so that conversion keeps `%25`.
#[cfg(any(windows, test))]
fn windows_mailto_arg(url: &str) -> bool {
    is_mailto(url)
        && !url
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || ch == '"')
}

#[cfg(any(windows, test))]
fn windows_mailto_command(url: &str) -> std::io::Result<std::process::Command> {
    if !windows_mailto_arg(url) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "mailto address is not safe to open",
        ));
    }
    let mut command = std::process::Command::new(rundll32_program());
    command.arg("url.dll,FileProtocolHandler");
    command.arg(url);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // `CREATE_NO_WINDOW`. A console must not flash over the app.
        command.creation_flags(0x0800_0000);
    }
    Ok(command)
}

fn confirm_id() -> egui::Id {
    egui::Id::new("thinwire-link-confirm")
}

/// Byte index of a pointer position in `body`.
///
/// The galley cursor is a character index. Link ranges are byte indexes.
pub(crate) fn galley_index(
    galley: &egui::Galley,
    galley_pos: egui::Pos2,
    pointer: egui::Pos2,
    body: &str,
) -> usize {
    let characters = galley.cursor_from_pos(pointer - galley_pos).index.0;
    byte_at(body, characters)
}

/// Byte index of character `char_index`. The end of `text` when it is past the last character.
fn byte_at(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirm_when_the_label_hides_the_host_or_the_address_differs() {
        assert!(needs_confirm("docs", "https://evil.example"));
        assert!(needs_confirm("docs", "https://docs.example/guide"));
        assert!(needs_confirm("see the doc", "https://evil.example"));
        assert!(needs_confirm("ada@b.test", "https://evil.example"));
        assert!(!needs_confirm("ada@b.test", "mailto:ada@b.test"));
        assert!(!needs_confirm("Ada@B.test", "mailto:ada@b.test"));
        assert!(!needs_confirm("ada@b.test", "mailto:ada@b.test?subject=Hi"));
        assert!(needs_confirm("ada@b.test", "mailto:other@b.test"));
        assert!(!needs_confirm("https://a.com", "https://a.com"));
        assert!(!needs_confirm("www.a.com", "https://www.a.com"));
        assert!(!needs_confirm("https://a.com/", "https://a.com"));
        assert!(needs_confirm("https://a.com", "https://b.com"));
        assert!(needs_confirm("https://a.com", "http://a.com"));
        assert!(needs_confirm("https://a.com/one", "https://a.com/two"));
        assert!(needs_confirm("www.a.com", "https://b.com"));
        assert!(needs_confirm("https://a.com", "https://a.com@evil.com"));
    }

    #[test]
    fn a_blocked_scheme_is_plain_text_and_the_open_gate_rejects_it() {
        let text = Color32::WHITE;
        let link = Color32::BLUE;
        let font = FontId::monospace(14.0);
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "smb://server/share",
            "custom://open",
        ] {
            // The blocked address is the visible text, so a selection can copy it.
            let body = url;
            let links = [MessageLink {
                start: 0,
                end: body.len(),
                url: url.to_string(),
            }];
            let job = layout_job(body, &links, text, link, font.clone());
            assert_eq!(job.text, body, "{url}");
            assert_eq!(job.sections.len(), 1, "{url}");
            assert_eq!(job.sections[0].format.color, text, "{url}");
            assert_eq!(job.sections[0].format.underline.width, 0.0, "{url}");
            assert!(openable_url(url).is_none(), "{url}");
        }
        let body = "https://a.test";
        let links = [MessageLink {
            start: 0,
            end: body.len(),
            url: body.to_string(),
        }];
        let job = layout_job(body, &links, text, link, font);
        assert!(
            job.sections
                .iter()
                .any(|section| section.format.underline.width > 0.0)
        );
        assert_eq!(openable_url(body).as_deref(), Some(body));
    }

    #[test]
    fn a_lookalike_idn_host_asks_and_the_dialog_shows_punycode() {
        use eframe::egui::accesskit::Role;
        use egui_kittest::Harness;
        use egui_kittest::kittest::{NodeT, Queryable};

        // Cyrillic 'а' (U+0430) in place of Latin 'a'.
        let target = "https://\u{0430}pple.com/account";
        let label = "https://apple.com/account";
        assert!(needs_confirm(label, target));
        let host = confirm_host(target).expect("host");
        assert_eq!(host, "xn--pple-43d.com");
        assert_eq!(open_button_label(target), "Open xn--pple-43d.com");
        assert_eq!(host_font().family, crate::app::theme::semibold());
        // The real host is the part after `@` in the authority, not the lookalike.
        assert_eq!(
            confirm_host("https://apple.com@evil.example/x").as_deref(),
            Some("evil.example")
        );
        assert_eq!(
            confirm_host("mailto:ada@\u{0430}pple.com").as_deref(),
            Some("xn--pple-43d.com")
        );

        let show = |target: &'static str| {
            let remember = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let fonts = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            Harness::builder()
                .with_size(egui::vec2(640.0, 400.0))
                .build_ui(move |ui| {
                    // `install` lands on the next pass. The host label needs that font.
                    if !fonts.load(std::sync::atomic::Ordering::Relaxed) {
                        crate::app::theme::install(ui.ctx());
                        if !ui
                            .style()
                            .text_styles
                            .contains_key(&crate::app::theme::secondary())
                        {
                            return;
                        }
                        fonts.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    // One shot. A later frame must not open the dialog again after Cancel.
                    if !remember.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        remember_confirm(ui.ctx(), target);
                    }
                    confirm(ui);
                })
        };
        let mut harness = show(target);
        harness.run();
        harness.get_by_label("xn--pple-43d.com");
        harness.get_by_role_and_label(Role::Button, "Open xn--pple-43d.com");
        let cancel = harness.get_by_role_and_label(Role::Button, "Cancel");
        assert!(
            cancel.accesskit_node().is_focused(),
            "focus starts on Cancel"
        );

        OPENED.with(|opened| opened.borrow_mut().clear());
        harness.key_press(egui::Key::Enter);
        harness.step();
        assert!(harness.query_by_label("Cancel").is_none());
        OPENED.with(|opened| assert!(opened.borrow().is_empty(), "Enter cancels"));

        let mut harness = show(target);
        harness.run();
        harness.key_press(egui::Key::Escape);
        harness.step();
        assert!(harness.query_by_label("Cancel").is_none());
        OPENED.with(|opened| assert!(opened.borrow().is_empty(), "Escape cancels"));
    }

    #[test]
    fn mailto_is_one_argument_and_does_not_use_the_browser() {
        assert_eq!(opener("https://a.test"), Some(Opener::Browser));
        assert_eq!(opener("mailto:ada@b.test"), Some(Opener::Mail));
        assert_eq!(opener("MAILTO:ada@b.test"), Some(Opener::Mail));
        assert_eq!(opener("file:///etc/passwd"), None);
        assert_eq!(opener("javascript:alert(1)"), None);

        let url = "mailto:ada@b.test?subject=Hi&body=a%20b";
        let command = mailto_command(url).expect("command");
        let args = command_args(&command);
        if cfg!(target_os = "macos") {
            assert_eq!(command.get_program(), "open");
            assert_eq!(args, [url]);
        } else if cfg!(windows) {
            assert_windows_mailto(&command, url);
        } else {
            assert_eq!(command.get_program(), "xdg-open");
            assert_eq!(args, [url]);
        }

        // `cmd /c` keeps `%%` as two percent signs. The handler must see `%25`.
        let encoded = "mailto:a@b.test?subject=100%25";
        let command = windows_mailto_command(encoded).expect("windows command");
        assert_windows_mailto(&command, encoded);
        assert!(windows_mailto_command("mailto:a\"@b.test&calc").is_err());
        assert!(windows_mailto_command("mailto:a@b.test\ncalc").is_err());
        assert!(windows_mailto_command("mailto:a@b.test?subject=hello world").is_err());
        assert!(windows_mailto_command("https://a.test").is_err());
    }

    fn command_args(command: &std::process::Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn assert_windows_mailto(command: &std::process::Command, url: &str) {
        let program = command.get_program().to_string_lossy();
        assert!(
            program.ends_with(r"\System32\rundll32.exe"),
            "mailto goes through rundll32: {program}"
        );
        let args = command_args(command);
        assert_eq!(
            args,
            ["url.dll,FileProtocolHandler".to_string(), url.to_string()],
            "the address is one argument and keeps each percent: {args:?}"
        );
        assert!(
            !url.contains("%%") && args.iter().all(|arg| !arg.contains("%%")),
            "a doubled percent would survive cmd /c: {args:?}"
        );
    }

    #[test]
    fn a_character_index_lands_on_the_byte_after_an_emoji() {
        let text = "hi 👍 go";
        assert_eq!(byte_at(text, 0), 0);
        assert_eq!(byte_at(text, 3), "hi ".len());
        assert_eq!(byte_at(text, 4), "hi 👍".len());
        assert_eq!(&text[byte_at(text, 4)..], " go");
        assert_eq!(byte_at(text, 99), text.len());
    }
}
