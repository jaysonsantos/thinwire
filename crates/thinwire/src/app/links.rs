//! Underlined links in a message, and the confirm step when the label hides
//! the address.
//!
//! Opening a link happens off the UI thread. The address is not logged.
//! [`open_url`] checks the scheme again. A blocked scheme is plain text.

use eframe::egui::text::LayoutJob;
use eframe::egui::{self, Color32, FontId, Stroke, TextFormat};

use thinwire_protocol::{MessageLink, openable_url};

/// A link the user has not opened yet, because the label is a different address.
#[derive(Clone)]
struct PendingLink {
    url: String,
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
    ctx.data_mut(|data| {
        data.insert_temp(
            confirm_id(),
            PendingLink {
                url: url.to_owned(),
            },
        );
    });
}

/// Close the confirm dialog. `true` when it was open.
pub(crate) fn take_escape(ctx: &egui::Context) -> bool {
    let open = ctx.data(|data| data.get_temp::<PendingLink>(confirm_id()).is_some());
    if open {
        ctx.data_mut(|data| data.remove::<PendingLink>(confirm_id()));
    }
    open
}

/// The confirm dialog. It shows the real address and does not log it.
pub(crate) fn confirm(ui: &mut egui::Ui) {
    let Some(pending) = ui
        .ctx()
        .data(|data| data.get_temp::<PendingLink>(confirm_id()))
    else {
        return;
    };
    let screen = ui.ctx().content_rect();
    let mut open = true;
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
                    ui.set_max_width(360.0);
                    ui.label("This link text does not match the address.");
                    ui.label(pending.url.as_str());
                    ui.horizontal(|ui| {
                        if ui.button("Open link").clicked() {
                            open_url(&pending.url);
                            open = false;
                        }
                        if ui.button("Cancel").clicked() {
                            open = false;
                        }
                    });
                });
        });
    if !open {
        ui.ctx()
            .data_mut(|data| data.remove::<PendingLink>(confirm_id()));
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
    std::thread::spawn(move || {
        if webbrowser::open(&url).is_err() {
            tracing::debug!("link did not open");
        }
    });
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
        for url in ["file:///etc/passwd", "javascript:alert(1)", "custom://open"] {
            let body = "see the doc";
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
    fn a_character_index_lands_on_the_byte_after_an_emoji() {
        let text = "hi 👍 go";
        assert_eq!(byte_at(text, 0), 0);
        assert_eq!(byte_at(text, 3), "hi ".len());
        assert_eq!(byte_at(text, 4), "hi 👍".len());
        assert_eq!(&text[byte_at(text, 4)..], " go");
        assert_eq!(byte_at(text, 99), text.len());
    }
}
