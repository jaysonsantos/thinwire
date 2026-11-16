//! Name completion after "@" in the compose field of a group chat.
//!
//! While the text before the cursor ends with "@" and the start of a name,
//! a list over the compose field shows the people of the chat who match.
//! Up and Down move the highlight. Tab, Enter, or a click puts the mention
//! text and one space in the place of the typed part. Esc closes the list
//! and keeps the text.
//!
//! The core keeps the people of each chat (`View::chat_members`). This
//! module finds the typed part, filters the people, and draws the list.

use eframe::egui::{self, RichText};
use thinwire_core::View;
use thinwire_protocol::ChatMember;

use super::theme::{self, radius, space};

/// The most rows the list shows. More typed characters narrow the list.
const MAX_ROWS: usize = 6;

/// The width of the list.
const LIST_WIDTH: f32 = 300.0;

/// The height of one row.
const ROW_HEIGHT: f32 = 30.0;

/// The typed part of a mention: the "@" and the characters after it, up to
/// the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    /// The place of the "@", counted in characters.
    pub start: usize,
    /// The characters after the "@".
    pub query: String,
}

/// A character that a typed name can have.
fn is_name_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

/// The mention that the user types at `cursor` (counted in characters).
/// The "@" starts the text or follows white space, and only letters,
/// digits, and "_" are between it and the cursor.
pub(crate) fn token(text: &str, cursor: usize) -> Option<Token> {
    let before: Vec<char> = text.chars().take(cursor).collect();
    let typed = before
        .iter()
        .rev()
        .take_while(|character| is_name_char(**character))
        .count();
    let start = before.len().checked_sub(typed + 1)?;
    if before[start] != '@' {
        return None;
    }
    if start > 0 && !before[start - 1].is_whitespace() {
        return None;
    }
    Some(Token {
        start,
        query: before[start + 1..].iter().collect(),
    })
}

/// How well a person matches the typed part. A smaller value goes first.
/// `None` is no match.
fn rank(member: &ChatMember, query: &str) -> Option<u8> {
    let name = member.name.to_lowercase();
    let handle = member.handle.as_deref().unwrap_or_default().to_lowercase();
    if name.starts_with(query) || handle.starts_with(query) {
        Some(0)
    } else if name.split_whitespace().any(|word| word.starts_with(query)) {
        Some(1)
    } else if name.contains(query) || handle.contains(query) {
        Some(2)
    } else {
        None
    }
}

/// The people who match the typed part, on the name and on the handle,
/// with upper and lower case equal. A name or a handle that starts with the
/// typed part goes first, then a later word of the name, then the rest.
/// Each group keeps the order of the protocol.
pub(crate) fn matches<'a>(members: &'a [ChatMember], query: &str) -> Vec<&'a ChatMember> {
    let query = query.to_lowercase();
    let mut found: Vec<(u8, &ChatMember)> = members
        .iter()
        .filter_map(|member| Some((rank(member, &query)?, member)))
        .collect();
    found.sort_by_key(|(rank, _)| *rank);
    found.into_iter().map(|(_, member)| member).collect()
}

/// The text with `mention` and one space in the place of the typed part,
/// and the new place of the cursor, after that space. A space that already
/// follows the cursor is used, so the text does not get two.
pub(crate) fn complete(text: &str, start: usize, cursor: usize, mention: &str) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let start = start.min(chars.len());
    let cursor = cursor.clamp(start, chars.len());
    let mut done: String = chars[..start].iter().collect();
    done.push_str(mention);
    done.push(' ');
    let rest = &chars[cursor..];
    let rest = if rest.first() == Some(&' ') {
        &rest[1..]
    } else {
        rest
    };
    done.extend(rest);
    (done, start + mention.chars().count() + 1)
}

/// What the list keeps between frames.
#[derive(Debug, Clone, Default)]
struct State {
    /// The thread of the last frame. Another thread starts with no state.
    thread: Option<egui::Id>,
    /// The row with the highlight.
    highlight: usize,
    /// The typed part of the last frame. A change puts the highlight on the
    /// first row.
    query: String,
    /// The "@" of the list that is open, counted in characters.
    open: Option<usize>,
    /// The "@" of a list that Esc closed. It stays closed until that
    /// mention ends.
    dismissed: Option<usize>,
    /// The last place of the cursor while the field had the focus.
    cursor: Option<usize>,
    /// Where the list was in the last frame.
    rect: Option<egui::Rect>,
}

fn state_id(field: egui::Id) -> egui::Id {
    field.with("mention")
}

fn load(ctx: &egui::Context, field: egui::Id) -> State {
    ctx.data(|data| data.get_temp(state_id(field)))
        .unwrap_or_default()
}

fn store(ctx: &egui::Context, field: egui::Id, state: State) {
    ctx.data_mut(|data| data.insert_temp(state_id(field), state));
}

/// Close the list of the compose field with the focus, as Esc does. `true`
/// when it was open.
pub(crate) fn take_escape(ctx: &egui::Context) -> bool {
    let field = egui::Id::unique("thread-compose");
    let mut state = load(ctx, field);
    let Some(start) = state.open.take() else {
        return false;
    };
    state.dismissed = Some(start);
    store(ctx, field, state);
    true
}

/// What one frame of the list did.
#[derive(Debug, Default)]
pub(crate) struct Outcome {
    /// The new text of the field: the user picked a person.
    pub text: Option<String>,
    /// The list is open: Tab and Esc stay with the field.
    pub open: bool,
}

impl Outcome {
    /// The keys that do not move the focus away from the field. With the
    /// list open, Tab and Esc are keys of the list.
    pub(crate) fn event_filter(&self) -> egui::EventFilter {
        egui::EventFilter {
            horizontal_arrows: true,
            vertical_arrows: true,
            tab: self.open,
            escape: self.open,
        }
    }

    /// Call after the field is added: a click on a row took the focus away
    /// from the field, and this gives it back.
    pub(crate) fn after_field(&self, ui: &egui::Ui, field: egui::Id) {
        if self.text.is_some() {
            ui.memory_mut(|memory| memory.request_focus(field));
        }
    }
}

/// The place of the cursor in the field `field`, counted in characters.
/// `None` while a range is selected.
fn field_cursor(ctx: &egui::Context, field: egui::Id) -> Option<usize> {
    let range = egui::TextEdit::load_state(ctx, field)?
        .cursor
        .char_range()?;
    (range.primary.index == range.secondary.index).then_some(range.primary.index.0)
}

fn set_field_cursor(ctx: &egui::Context, field: egui::Id, cursor: usize) {
    let mut state = egui::TextEdit::load_state(ctx, field).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::one(
            egui::text::CCursor::new(cursor),
        )));
    egui::TextEdit::store_state(ctx, field, state);
}

/// One frame of the list for the compose field `field`. Call it before the
/// field is added, so the field does not see a key of the list. `anchor` is
/// the bottom left corner of the list.
pub(crate) fn show(
    ui: &egui::Ui,
    snapshot: &View<'_>,
    field: egui::Id,
    anchor: egui::Pos2,
) -> Outcome {
    let ctx = ui.ctx();
    let mut state = load(ctx, field);
    // A closed list of one chat does not stay closed in another chat.
    let thread = snapshot
        .pane
        .conversation
        .as_ref()
        .map(|chat| egui::Id::unique((snapshot.pane.protocol, chat, snapshot.pane.topic)));
    if state.thread != thread {
        state = State {
            thread,
            ..State::default()
        };
    }
    let outcome = frame(ui, snapshot, field, anchor, &mut state);
    if !outcome.open {
        state.open = None;
        state.rect = None;
    }
    store(ctx, field, state);
    outcome
}

fn frame(
    ui: &egui::Ui,
    snapshot: &View<'_>,
    field: egui::Id,
    anchor: egui::Pos2,
    state: &mut State,
) -> Outcome {
    let ctx = ui.ctx();
    let closed = Outcome::default();
    let Some(chat) = snapshot.pane.conversation.as_deref() else {
        return closed;
    };
    let members = snapshot.chat_members(snapshot.pane.protocol, chat);
    if members.is_empty() {
        return closed;
    }
    let focused = ui.memory(|memory| memory.has_focus(field));
    // A press on a row takes the focus away from the field. The list stays
    // while the pointer is on it, so the click arrives.
    let on_list = state.rect.is_some_and(|rect| {
        ctx.input(|input| input.pointer.interact_pos())
            .is_some_and(|pos| rect.contains(pos))
    });
    let cursor = if focused {
        state.cursor = field_cursor(ctx, field);
        state.cursor
    } else if on_list {
        state.cursor
    } else {
        state.cursor = None;
        None
    };
    let text = snapshot.pane.compose.as_str();
    let Some(token) = cursor.and_then(|cursor| token(text, cursor)) else {
        state.dismissed = None;
        return closed;
    };
    let cursor = cursor.unwrap_or_default();
    if state.dismissed == Some(token.start) {
        return closed;
    }
    state.dismissed = None;
    let found = matches(members, &token.query);
    let rows = &found[..found.len().min(MAX_ROWS)];
    if rows.is_empty() {
        return closed;
    }
    if state.query != token.query {
        state.query.clone_from(&token.query);
        state.highlight = 0;
    }
    state.highlight = state.highlight.min(rows.len() - 1);

    let mut chosen = None;
    if focused {
        let none = egui::Modifiers::NONE;
        let (down, up, tab, enter, escape) = ctx.input_mut(|input| {
            (
                input.consume_key(none, egui::Key::ArrowDown),
                input.consume_key(none, egui::Key::ArrowUp),
                input.consume_key(none, egui::Key::Tab),
                input.consume_key(none, egui::Key::Enter),
                input.consume_key(none, egui::Key::Escape),
            )
        });
        if escape {
            state.dismissed = Some(token.start);
            return closed;
        }
        if down {
            state.highlight = (state.highlight + 1) % rows.len();
        }
        if up {
            state.highlight = (state.highlight + rows.len() - 1) % rows.len();
        }
        if tab || enter {
            chosen = Some(state.highlight);
        }
    }

    let area = egui::Area::new(state_id(field).with("list"))
        .order(egui::Order::Foreground)
        .pivot(egui::Align2::LEFT_BOTTOM)
        .fixed_pos(anchor)
        .show(ctx, |ui| {
            let palette = theme::palette(ui);
            egui::Frame::new()
                .fill(palette.surface)
                .stroke(egui::Stroke::new(1.0, palette.border_strong))
                .corner_radius(radius::CONTROL)
                .inner_margin(egui::Margin::same(space::XS as i8))
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.set_width(LIST_WIDTH);
                    for (index, member) in rows.iter().enumerate() {
                        let response = row(ui, member, index == state.highlight);
                        if response.hovered() && ui.input(|input| input.pointer.is_moving()) {
                            state.highlight = index;
                        }
                        if response.clicked() {
                            chosen = Some(index);
                        }
                    }
                });
        });
    state.rect = Some(area.response.rect);
    state.open = Some(token.start);

    let Some(member) = chosen.and_then(|index| rows.get(index)) else {
        return Outcome {
            text: None,
            open: true,
        };
    };
    let (done, cursor) = complete(text, token.start, cursor, &member.mention());
    set_field_cursor(ctx, field, cursor);
    state.cursor = Some(cursor);
    state.highlight = 0;
    Outcome {
        text: Some(done),
        open: false,
    }
}

/// The name of a row for a screen reader: "Mention Ada Park".
fn row_name(member: &ChatMember) -> String {
    format!("Mention {}", member.name)
}

/// One person: the name, and the handle after it. The row is one click
/// target.
fn row(ui: &mut egui::Ui, member: &ChatMember, highlighted: bool) -> egui::Response {
    let palette = theme::palette(ui);
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), ROW_HEIGHT),
        egui::Sense::click(),
    );
    let name = row_name(member);
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &name));
    if ui.is_rect_visible(rect) {
        let fill = if highlighted {
            palette.selected_row
        } else if response.hovered() {
            palette.hover
        } else {
            palette.surface
        };
        ui.painter().rect_filled(rect, radius::CONTROL, fill);
        let inner = rect.shrink2(egui::vec2(space::M, 0.0));
        let wrap = Some(egui::TextWrapMode::Truncate);
        let title = egui::WidgetText::from(RichText::new(&member.name).color(palette.text))
            .into_galley(ui, wrap, inner.width(), egui::TextStyle::Body);
        let title_width = title.size().x;
        ui.painter().galley(
            egui::pos2(inner.left(), inner.center().y - title.size().y / 2.0),
            title,
            palette.text,
        );
        let left = inner.left() + title_width + space::M;
        if let Some(handle) = member.handle.as_deref()
            && inner.right() - left > space::XL
        {
            let handle = egui::WidgetText::from(
                RichText::new(format!("@{handle}"))
                    .text_style(theme::secondary())
                    .color(palette.text2),
            )
            .into_galley(ui, wrap, inner.right() - left, theme::secondary());
            ui.painter().galley(
                egui::pos2(left, inner.center().y - handle.size().y / 2.0),
                handle,
                palette.text2,
            );
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(name: &str, handle: Option<&str>) -> ChatMember {
        ChatMember {
            name: name.to_owned(),
            handle: handle.map(str::to_owned),
        }
    }

    fn found(text: &str, cursor: usize) -> Option<(usize, String)> {
        token(text, cursor).map(|token| (token.start, token.query))
    }

    #[test]
    fn a_token_is_an_at_sign_and_name_characters_before_the_cursor() {
        assert_eq!(found("@", 1), Some((0, String::new())));
        assert_eq!(found("@ad", 3), Some((0, "ad".into())));
        assert_eq!(found("hi @ad", 6), Some((3, "ad".into())));
        assert_eq!(found("hi\n@a_1", 7), Some((3, "a_1".into())));
        // Only the part before the cursor counts.
        assert_eq!(found("hi @ada there", 5), Some((3, "a".into())));
        assert_eq!(found("hi @ada there", 4), Some((3, String::new())));
        // A cursor after the end is the end.
        assert_eq!(found("@ad", 9), Some((0, "ad".into())));
        // Characters are counted, not bytes.
        assert_eq!(found("é @zoë", 6), Some((2, "zoë".into())));
    }

    #[test]
    fn no_token_without_an_at_sign_at_a_word_start() {
        assert_eq!(found("", 0), None);
        assert_eq!(found("hello", 5), None);
        assert_eq!(found("mail@ad", 7), None, "an address is no mention");
        assert_eq!(found("@ada ", 5), None, "a space ends the mention");
        assert_eq!(found("@ada!", 5), None);
        assert_eq!(found("hi @ada", 3), None, "the cursor is before the @");
        assert_eq!(found("@@a", 3), None);
    }

    #[test]
    fn matches_read_the_name_and_the_handle_in_any_case() {
        let people = [
            member("Milo Chen", Some("milo")),
            member("Ada Park", Some("ada_park")),
            member("Nora Silva", None),
            member("Quiz Bot", Some("book_quiz_bot")),
            member("Noel Brandt", Some("adams")),
        ];
        let names = |query: &str| -> Vec<&str> {
            matches(&people, query)
                .into_iter()
                .map(|member| member.name.as_str())
                .collect()
        };
        assert_eq!(names("").len(), 5, "no typed part shows everyone");
        assert_eq!(names("")[0], "Milo Chen", "in the order of the protocol");
        assert_eq!(names("MI"), ["Milo Chen"]);
        assert_eq!(names("no"), ["Nora Silva", "Noel Brandt"]);
        // A handle counts as the name does.
        assert_eq!(names("ada"), ["Ada Park", "Noel Brandt"]);
        // The start of a name goes before a later word, then the rest.
        assert_eq!(names("b"), ["Quiz Bot", "Noel Brandt"]);
        assert_eq!(names("quiz"), ["Quiz Bot"]);
        assert_eq!(names("il"), ["Milo Chen", "Nora Silva"]);
        assert!(names("zz").is_empty());
    }

    #[test]
    fn a_pick_puts_the_mention_and_one_space_in_the_text() {
        assert_eq!(complete("@mi", 0, 3, "@milo"), ("@milo ".into(), 6));
        assert_eq!(
            complete("hi @mi there", 3, 6, "@milo"),
            ("hi @milo there".into(), 9),
            "the space after the cursor is used"
        );
        assert_eq!(
            complete("hi @mi, ok", 3, 6, "@milo"),
            ("hi @milo , ok".into(), 9)
        );
        // A person with no handle is named.
        assert_eq!(
            complete("é @no", 2, 5, "@Nora Silva"),
            ("é @Nora Silva ".into(), 14)
        );
    }

    #[test]
    fn a_row_says_what_a_click_does() {
        assert_eq!(row_name(&member("Ada Park", None)), "Mention Ada Park");
    }
}
