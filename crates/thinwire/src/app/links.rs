//! Underlined links in a message, and the confirm step when the label hides
//! the address.
//!
//! The confirm dialog shows the link text, the real host, and the full
//! address, so the user sees what the text said and where it goes.
//!
//! Opening a link happens off the UI thread. The address and the link text
//! are not logged.
//! [`open_url`] checks the scheme again. A blocked scheme is plain text.
//! `http` and `https` go through `webbrowser` with its `hardened` feature.
//! `mailto` goes to the OS handler as one argument.
//!
//! A mention of a person has the link color too. A click on it opens the
//! one-to-one chat with that person, when the chat list has that chat.

use eframe::egui::text::LayoutJob;
use eframe::egui::{self, Color32, FontId, Stroke, TextFormat};

use thinwire_core::Intent;
use thinwire_protocol::{MessageLink, openable_url};

/// A link the user has not opened yet, because the label is a different address.
#[derive(Clone)]
struct PendingLink {
    url: String,
    /// The text of the link in the message. The dialog shows it, and no log has it.
    label: String,
    /// Open button from the last frame. Enter activates it only when it has focus.
    open_id: Option<egui::Id>,
}

/// True when the label is not the address the app will open.
///
/// A label that is not a URL hides the host. `docs` can point at
/// `docs.example` or at `evil.example`. The app asks in both cases.
/// An email label that is the same `mailto` address does not ask.
/// `www.a.com` and `https://www.a.com` are the same address.
/// A label with only a host shows only the host. The app does not ask when
/// the target has the same host and an `http` or `https` scheme. A leading
/// `www.` does not count. A label with a scheme also needs the same scheme.
/// A label with a path, a query, or a fragment needs the same address.
/// A different scheme, host, or path asks.
#[must_use]
pub(crate) fn needs_confirm(label: &str, target: &str) -> bool {
    if same_mailto(label, target) {
        return false;
    }
    if !label_looks_like_url(label) {
        return true;
    }
    if same_bare_host(label, target) {
        return false;
    }
    normalize_url(label) != normalize_url(target)
}

/// The label is only a host, and `target` opens a page on that host.
///
/// The `url` crate reads both sides, so an `@` in the authority, a lookalike
/// host, or a punycode name cannot pass as the host of the label.
fn same_bare_host(label: &str, target: &str) -> bool {
    let label = label.trim();
    let has_scheme = label.contains("://");
    let with_scheme = if has_scheme {
        label.to_string()
    } else {
        format!("https://{label}")
    };
    let (Ok(shown), Ok(opened)) = (
        url::Url::parse(&with_scheme),
        url::Url::parse(target.trim()),
    ) else {
        return false;
    };
    let bare = matches!(shown.path(), "" | "/")
        && shown.query().is_none()
        && shown.fragment().is_none()
        && shown.username().is_empty()
        && shown.password().is_none()
        && shown.port().is_none();
    if !bare || !matches!(opened.scheme(), "http" | "https") {
        return false;
    }
    if has_scheme && shown.scheme() != opened.scheme() {
        return false;
    }
    if !opened.username().is_empty() || opened.password().is_some() || opened.port().is_some() {
        return false;
    }
    match (shown.host_str(), opened.host_str()) {
        (Some(shown), Some(opened)) => without_www(shown) == without_www(opened),
        _ => false,
    }
}

fn without_www(host: &str) -> &str {
    host.strip_prefix("www.").unwrap_or(host)
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
        if !span.is_mention() && span.url().and_then(openable_url).is_none() {
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

/// Keep `url` and its link text `label` for the confirm dialog.
pub(crate) fn remember_confirm(ctx: &egui::Context, url: &str, label: &str) {
    let same = ctx
        .data(|data| data.get_temp::<PendingLink>(confirm_id()))
        .is_some_and(|pending| pending.url == url && pending.label == label);
    if same {
        return;
    }
    ctx.data_mut(|data| {
        data.insert_temp(
            confirm_id(),
            PendingLink {
                url: url.to_owned(),
                label: label.to_owned(),
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

/// Most characters of the link text that the dialog shows.
const LABEL_MAX_CHARS: usize = 80;

/// The link text as the dialog shows it: in quotes, on one line, and cut
/// with `…` after [`LABEL_MAX_CHARS`] characters.
fn shown_label(label: &str) -> String {
    let line = label.split_whitespace().collect::<Vec<_>>().join(" ");
    let shown = if line.chars().count() > LABEL_MAX_CHARS {
        let mut cut: String = line.chars().take(LABEL_MAX_CHARS - 1).collect();
        cut.push('…');
        cut
    } else {
        line
    };
    format!("\u{201c}{shown}\u{201d}")
}

/// A plain label whose accessible name is its text, also where AT-SPI reads
/// the value. The text is never a link.
fn dialog_label(ui: &mut egui::Ui, text: impl Into<egui::WidgetText>) -> egui::Response {
    let text = text.into();
    let name = text.text().to_owned();
    let response = ui.add(egui::Label::new(text));
    super::a11y::name_label(ui.ctx(), response.id, &name);
    response
}

/// Semibold is the bold weight of the UI font.
fn host_font() -> egui::FontId {
    egui::FontId::new(super::theme::size::BODY, super::theme::semibold())
}

/// The confirm dialog. It shows the link text, the real host, and the full
/// address. It does not log the address or the link text.
pub(crate) fn confirm(ui: &mut egui::Ui) {
    let Some(pending) = ui
        .ctx()
        .data(|data| data.get_temp::<PendingLink>(confirm_id()))
    else {
        return;
    };
    let url = pending.url.clone();
    let label = pending.label.clone();
    let shown = shown_label(&label);
    let host = confirm_host(&url);
    let open_label = open_button_label(&url);
    let screen = ui.ctx().content_rect();
    let mut dismiss = false;
    let mut open_id = None;
    egui::Area::new(egui::Id::unique("link-confirm"))
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
                    dialog_label(ui, "The link text is not the address that opens.");
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        dialog_label(ui, "Link text:");
                        let response = ui.add(egui::Label::new(shown.as_str()).truncate());
                        super::a11y::name_label(ui.ctx(), response.id, &shown);
                    });
                    ui.horizontal(|ui| {
                        dialog_label(ui, "Opens:");
                        if let Some(host) = &host {
                            dialog_label(
                                ui,
                                egui::RichText::new(host)
                                    .font(host_font())
                                    .color(super::theme::palette(ui).text),
                            );
                        }
                    });
                    dialog_label(ui, url.as_str());
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
                    label,
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
    out: &mut Vec<Intent>,
) {
    let Some(pointer) = response.hover_pos().or(response.interact_pointer_pos()) else {
        return;
    };
    let index = galley_index(galley, galley_pos, pointer, body);
    let Some(link) = link_at(links, index) else {
        return;
    };
    if link.is_mention() {
        // A mention with no known chat is only a colored name.
        let Some(chat) = link.chat() else {
            return;
        };
        response
            .clone()
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if response.clicked() {
            out.push(Intent::SelectConversation {
                id: chat.to_owned(),
            });
        }
        return;
    }
    let Some(url) = link.url().filter(|url| openable_url(url).is_some()) else {
        return;
    };
    let label = body.get(link.start..link.end).unwrap_or("");
    response
        .clone()
        .on_hover_text(url)
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() {
        if needs_confirm(label, url) {
            remember_confirm(ui.ctx(), url, label);
        } else {
            open_url(url);
        }
    }
    let url = url.to_owned();
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
pub(crate) fn rundll32_program() -> String {
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
    egui::Id::unique("thinwire-link-confirm")
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
    fn a_bare_host_label_opens_a_page_on_that_host_without_a_question() {
        let deep = "https://fenixstore.de/Fenix-E12-V30-LED-Taschenlampe";
        assert!(!needs_confirm("fenixstore.de", deep));
        assert!(!needs_confirm("FenixStore.de", deep));
        assert!(!needs_confirm("https://fenixstore.de/", deep));
        assert!(!needs_confirm("fenixstore.de", "http://fenixstore.de/x"));
        assert!(!needs_confirm("www.fenixstore.de", deep));
        assert!(!needs_confirm(
            "fenixstore.de",
            "https://www.fenixstore.de/x"
        ));
        assert!(needs_confirm(
            "https://fenixstore.de",
            "http://fenixstore.de/x"
        ));
        assert!(needs_confirm(
            "fenixstore.de",
            "https://fenixstore.de.evil.example/x"
        ));
        assert!(needs_confirm(
            "fenixstore.de",
            "https://evil.example/fenixstore.de"
        ));
        assert!(needs_confirm(
            "fenixstore.de",
            "https://shop.fenixstore.de/x"
        ));
        assert!(needs_confirm(
            "fenixstore.de",
            "https://fenixstore.de@evil.example/x"
        ));
        assert!(needs_confirm(
            "fenixstore.de",
            "https://fenixstore.de:8443/x"
        ));
        assert!(needs_confirm("fenixstore.de", "javascript:alert(1)"));
        assert!(needs_confirm("fenixstore.de", "file:///fenixstore.de"));
        assert!(needs_confirm("fenixstore.de/a", "https://fenixstore.de/b"));
        assert!(needs_confirm(
            "fenixstore.de?x=1",
            "https://fenixstore.de/b"
        ));
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
            let links = [MessageLink::web(0, body.len(), url)];
            let job = layout_job(body, &links, text, link, font.clone());
            assert_eq!(job.text, body, "{url}");
            assert_eq!(job.sections.len(), 1, "{url}");
            assert_eq!(job.sections[0].format.color, text, "{url}");
            assert_eq!(job.sections[0].format.underline.width, 0.0, "{url}");
            assert!(openable_url(url).is_none(), "{url}");
        }
        let body = "https://a.test";
        let links = [MessageLink::web(0, body.len(), body)];
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

        let show = |target: &'static str, label: &'static str| {
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
                        remember_confirm(ui.ctx(), target, label);
                    }
                    confirm(ui);
                })
        };
        let mut harness = show(target, label);
        harness.run();
        harness.get_by_label("The link text is not the address that opens.");
        harness.get_by_label("Link text:");
        let text = harness.get_by_label("\u{201c}https://apple.com/account\u{201d}");
        assert_eq!(
            text.accesskit_node().value().as_deref(),
            Some("\u{201c}https://apple.com/account\u{201d}"),
            "AT-SPI reads the link text"
        );
        harness.get_by_label("Opens:");
        harness.get_by_label("xn--pple-43d.com");
        harness.get_by_label(target);
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

        let mut harness = show(target, label);
        harness.run();
        harness.key_press(egui::Key::Escape);
        harness.step();
        assert!(harness.query_by_label("Cancel").is_none());
        OPENED.with(|opened| assert!(opened.borrow().is_empty(), "Escape cancels"));

        // Words that point at a post: the dialog shows the words and the host.
        let post = "https://x.com/poteto/status/2119521036660314339";
        assert!(needs_confirm("read the thread", post));
        let mut harness = show(post, "read the thread");
        harness.run();
        harness.get_by_label("\u{201c}read the thread\u{201d}");
        harness.get_by_label("x.com");
        harness.get_by_label(post);
        harness.get_by_role_and_label(Role::Button, "Open x.com");
    }

    #[test]
    fn the_pending_link_keeps_the_link_text_for_the_dialog() {
        let ctx = egui::Context::default();
        remember_confirm(&ctx, "https://x.com/poteto/status/1", "read this thread");
        let pending = ctx
            .data(|data| data.get_temp::<PendingLink>(confirm_id()))
            .expect("pending link");
        assert_eq!(pending.url, "https://x.com/poteto/status/1");
        assert_eq!(pending.label, "read this thread");
        // The same address with another text replaces the text.
        remember_confirm(&ctx, "https://x.com/poteto/status/1", "x.com");
        let pending = ctx
            .data(|data| data.get_temp::<PendingLink>(confirm_id()))
            .expect("pending link");
        assert_eq!(pending.label, "x.com");
    }

    #[test]
    fn the_dialog_shows_the_link_text_quoted_on_one_line_and_cut() {
        assert_eq!(shown_label("read this"), "\u{201c}read this\u{201d}");
        assert_eq!(shown_label(" two\nlines "), "\u{201c}two lines\u{201d}");
        let long = "a".repeat(LABEL_MAX_CHARS + 5);
        let shown = shown_label(&long);
        let inner = shown
            .trim_start_matches('\u{201c}')
            .trim_end_matches('\u{201d}');
        assert_eq!(inner.chars().count(), LABEL_MAX_CHARS);
        assert!(inner.ends_with('…'));
        let exact = "é".repeat(LABEL_MAX_CHARS);
        assert_eq!(shown_label(&exact), format!("\u{201c}{exact}\u{201d}"));
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
