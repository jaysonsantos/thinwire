//! Design tokens: colors, fonts, the type scale, spacing, and radii for the
//! egui shell.
//!
//! All shell colors come from [`Palette`]. [`install`] builds one egui
//! `Style` for each theme once at startup. `Settings::apply` then only picks
//! System / Light / Dark, so System keeps following the OS live (ADR 0005).
//!
//! Every text pair is at least WCAG AA 4.5:1. Every input boundary is at
//! least 3:1. The tests below check both palettes.

use std::collections::BTreeMap;
use std::sync::Arc;

use eframe::egui::epaint::text::VariationCoords;
use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, FontTweak, Margin,
    Stroke, TextStyle, Theme, Vec2,
};

/// Semantic colors for one theme. UI code reads colors only from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Palette {
    /// Thread / center panel.
    pub bg: Color32,
    /// Account sidebar. One step away from `sidebar`, so the two columns read as layers.
    pub rail: Color32,
    /// Chat list column and the status banner.
    pub sidebar: Color32,
    /// Inbound bubble, cards, buttons.
    pub surface: Color32,
    /// Hover fill on list rows.
    pub hover: Color32,
    /// Text fields.
    pub input: Color32,
    /// Separators only (decorative).
    pub border: Color32,
    /// Input border and other UI boundaries (3:1).
    pub border_strong: Color32,
    /// Primary text.
    pub text: Color32,
    /// Preview and secondary labels.
    pub text2: Color32,
    /// Times, hints, meta, day breaks.
    pub text3: Color32,
    /// Focus ring, the primary and Send buttons, and the brand mark.
    pub accent: Color32,
    /// Underlined links. 4.5:1 on an inbound bubble. An outbound bubble uses
    /// `out_link`.
    pub link: Color32,
    /// Text on the primary button.
    pub on_accent: Color32,
    /// Selected chat row, selected account row, and the pressed control.
    pub selected_row: Color32,
    /// Outbound bubble fill.
    pub out: Color32,
    /// Outbound body text.
    pub on_out: Color32,
    /// Links inside an outbound bubble. Underlined, so color is not the only cue.
    pub out_link: Color32,
    /// Outbound time and "Sending…".
    pub out_meta: Color32,
    /// Unread count pill.
    pub badge: Color32,
    /// Text on the unread pill.
    pub on_badge: Color32,
    /// Notices (keychain, not signed in, experimental).
    pub warn: Color32,
    /// Errors.
    pub error: Color32,
    /// "Not sent." inside an outbound bubble.
    pub out_error: Color32,
    /// Online dot and label.
    pub ok: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

impl Palette {
    pub(crate) const DARK: Self = Self {
        bg: hex(0x1D_1E_23),
        rail: hex(0x11_12_15),
        sidebar: hex(0x17_18_1C),
        surface: hex(0x2A_2C_33),
        hover: hex(0x22_24_2A),
        input: hex(0x13_14_17),
        border: hex(0x2B_2D_34),
        border_strong: hex(0x6B_70_7A),
        text: hex(0xEC_ED_F0),
        text2: hex(0xB6_BA_C3),
        text3: hex(0x9A_A0_AB),
        accent: hex(0x6A_A6_FF),
        link: hex(0xB7_D4_FF),
        on_accent: hex(0x08_17_2B),
        selected_row: hex(0x22_30_4A),
        out: hex(0x2A_52_B8),
        on_out: hex(0xFF_FF_FF),
        out_link: hex(0xFF_FF_FF),
        out_meta: hex(0xC9_D8_FF),
        badge: hex(0x2A_68_CC),
        on_badge: hex(0xFF_FF_FF),
        warn: hex(0xF0_B4_55),
        error: hex(0xFF_7A_7A),
        out_error: hex(0xFF_D0_CC),
        ok: hex(0x5F_D3_9A),
    };

    pub(crate) const LIGHT: Self = Self {
        bg: hex(0xFF_FF_FF),
        rail: hex(0xEC_EE_F2),
        sidebar: hex(0xF6_F7_F9),
        surface: hex(0xEA_ED_F2),
        hover: hex(0xE8_EB_F0),
        input: hex(0xFF_FF_FF),
        border: hex(0xE2_E5_EA),
        border_strong: hex(0x76_7C_87),
        text: hex(0x14_16_1A),
        text2: hex(0x46_4C_56),
        text3: hex(0x5C_62_6C),
        accent: hex(0x1F_5B_DB),
        link: hex(0x0B_3D_91),
        on_accent: hex(0xFF_FF_FF),
        selected_row: hex(0xDF_E9_FC),
        out: hex(0x1F_5B_DB),
        on_out: hex(0xFF_FF_FF),
        out_link: hex(0xFF_FF_FF),
        out_meta: hex(0xDC_E8_FF),
        badge: hex(0x1F_5B_DB),
        on_badge: hex(0xFF_FF_FF),
        warn: hex(0x8A_53_00),
        error: hex(0xB3_26_1E),
        out_error: hex(0xFF_E1_DE),
        ok: hex(0x1B_6E_43),
    };

    #[must_use]
    pub(crate) const fn of(theme: Theme) -> &'static Self {
        match theme {
            Theme::Dark => &Self::DARK,
            Theme::Light => &Self::LIGHT,
        }
    }
}

/// Color behind an inbox row. A normal row is transparent and shows the
/// sidebar. Hover paints `hover`. Selected paints `selected_row`, including
/// when the pointer is also over that row.
#[cfg(test)]
#[must_use]
pub(crate) const fn inbox_row_backdrop(
    palette: &Palette,
    selected: bool,
    hovered: bool,
) -> Color32 {
    if selected {
        palette.selected_row
    } else if hovered {
        palette.hover
    } else {
        palette.sidebar
    }
}

/// Row highlight during a transition. `selected` and `hovered` run from 0 to
/// one. At the ends it is the row backdrop of that state: `base`, then
/// `hover`, then `selected_row`. `base` is the panel the row sits on:
/// `sidebar` for chats, `rail` for accounts.
#[must_use]
pub(crate) fn row_fill(base: Color32, palette: &Palette, selected: f32, hovered: f32) -> Color32 {
    let hover = super::motion::mix(base, palette.hover, hovered);
    super::motion::mix(hover, palette.selected_row, selected)
}

/// [`row_fill`] on the chat list.
#[cfg(test)]
#[must_use]
pub(crate) fn inbox_row_fill(palette: &Palette, selected: f32, hovered: f32) -> Color32 {
    row_fill(palette.sidebar, palette, selected, hovered)
}

/// Outline of a muted unread count (#206).
///
/// WCAG 1.4.11 compares colors, so a thicker stroke of `border_strong` stays
/// about 2.53:1 on the dark selected row. The row title uses `text`, which
/// clears 3:1 on every [`inbox_row_backdrop`].
#[must_use]
pub(crate) const fn muted_badge_ring(palette: &Palette) -> Color32 {
    palette.text
}

/// Circle behind inbox initials, and the initials color on that circle. The
/// protocol tile of the account sidebar uses the same pairs.
///
/// One pair per `accent_color_id` in 0–6. The initials use the accent ink.
/// Each pair is at least 4.5:1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AvatarInk {
    pub fill: Color32,
    pub text: Color32,
}

const LIGHT_AVATAR: [AvatarInk; 7] = [
    AvatarInk {
        fill: hex(0xF3_B8_BE),
        text: hex(0x8B_1E_2D),
    },
    AvatarInk {
        fill: hex(0xF6_C9_A0),
        text: hex(0x7A_32_00),
    },
    AvatarInk {
        fill: hex(0xD4_C4_F5),
        text: hex(0x54_28_87),
    },
    AvatarInk {
        fill: hex(0xB7_E3_B8),
        text: hex(0x0F_5C_2E),
    },
    AvatarInk {
        fill: hex(0xB5_E3_E6),
        text: hex(0x0C_59_62),
    },
    AvatarInk {
        fill: hex(0xC5_D7_F6),
        text: hex(0x0D_3C_88),
    },
    AvatarInk {
        fill: hex(0xF5_C0_D6),
        text: hex(0x8A_21_54),
    },
];

const DARK_AVATAR: [AvatarInk; 7] = [
    AvatarInk {
        fill: hex(0x5C_2E_36),
        text: hex(0xFF_D0_D4),
    },
    AvatarInk {
        fill: hex(0x5C_3C_22),
        text: hex(0xFF_E0_C2),
    },
    AvatarInk {
        fill: hex(0x3E_34_58),
        text: hex(0xE6_D8_FF),
    },
    AvatarInk {
        fill: hex(0x24_50_3A),
        text: hex(0xC8_F5_D4),
    },
    AvatarInk {
        fill: hex(0x1E_4A_50),
        text: hex(0xC6_F4_F8),
    },
    AvatarInk {
        fill: hex(0x243C64),
        text: hex(0xD4_E6_FF),
    },
    AvatarInk {
        fill: hex(0x5C_2C_44),
        text: hex(0xFF_D0_E4),
    },
];

/// Initials colors for palette index `accent` (0–6). Other values use 0.
#[must_use]
pub(crate) const fn avatar_ink(accent: u8, dark: bool) -> AvatarInk {
    let index = if accent <= 6 { accent as usize } else { 0 };
    if dark {
        DARK_AVATAR[index]
    } else {
        LIGHT_AVATAR[index]
    }
}

/// Color of a sender's name in a group chat. The same name always gets the
/// same color. It is the initials color of an [`avatar_ink`] pair, so it keeps
/// 4.5:1 on the inbound bubble in both themes.
#[must_use]
pub(crate) fn sender_ink(sender: &str, dark: bool) -> Color32 {
    let sum = sender.bytes().fold(0_u32, |sum, byte| {
        sum.wrapping_mul(31).wrapping_add(u32::from(byte))
    });
    avatar_ink((sum % 7) as u8, dark).text
}

/// Ink for a disabled "Muted in {protocol}" control (#206).
///
/// Title and reason are full palette colors on `bg` (the menu popup and the
/// thread). egui's 0.5 disabled fade is not part of this ink: in light that
/// blend falls under 4.5:1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProtocolMuteInk {
    pub title: Color32,
    pub reason: Color32,
    pub on: Color32,
}

#[must_use]
pub(crate) const fn protocol_mute_ink(palette: &Palette) -> ProtocolMuteInk {
    ProtocolMuteInk {
        title: palette.text,
        reason: palette.text2,
        on: palette.bg,
    }
}

/// Center a card: `bg` fill, a hairline, and a soft shadow that lifts it off
/// the page in both themes.
pub(crate) fn show_centered_card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let palette = palette(ui);
    let dark = ui.visuals().dark_mode;
    let width = ui.available_width().min(LOGIN_CARD_WIDTH);
    let inset = ((ui.available_width() - width) * 0.5).max(0.0);
    let frame = egui::Frame::new()
        .fill(if dark { palette.surface } else { palette.bg })
        .stroke(egui::Stroke::new(1.0, palette.border))
        .shadow(egui::Shadow {
            offset: [0, 8],
            blur: 24,
            spread: 0,
            color: Color32::from_black_alpha(if dark { 90 } else { 22 }),
        })
        .corner_radius(egui::CornerRadius::same(radius::CARD))
        .inner_margin(egui::Margin::same(space::XL as i8));
    ui.horizontal(|ui| {
        ui.add_space(inset);
        ui.vertical(|ui| {
            ui.set_max_width(width);
            frame.show(ui, add);
        });
    });
}

/// Frame of a text field. `focus` goes from 0 to 1: the hairline becomes
/// the accent ring and grows from 1 to 2 px. The hairline is `border_strong`,
/// which clears 3:1 on every panel (WCAG 1.4.11).
pub(crate) fn field_frame(palette: &Palette, focus: f32, margin: Margin) -> egui::Frame {
    let width = egui::lerp(1.0..=FIELD_RING, focus);
    egui::Frame::new()
        .fill(palette.input)
        .stroke(Stroke::new(
            width,
            super::motion::mix(palette.border_strong, palette.accent, focus),
        ))
        .corner_radius(CornerRadius::same(radius::FIELD_BOX))
        // The ring grows inward, so the text does not move when it appears.
        .inner_margin(margin - Margin::from(width))
}

/// Widest focused field ring.
pub(crate) const FIELD_RING: f32 = 2.0;

/// Tint for the tile of a protocol in the account sidebar. An index into
/// [`avatar_ink`], so the pair keeps its 4.5:1 initials contrast.
#[must_use]
pub(crate) const fn protocol_tile(protocol: thinwire_protocol::ProtocolId) -> u8 {
    use thinwire_protocol::ProtocolId;
    match protocol {
        ProtocolId::Telegram => 5,
        ProtocolId::WhatsApp => 3,
        ProtocolId::Discord => 2,
        ProtocolId::Slack => 6,
        ProtocolId::Signal => 4,
    }
}

/// Fill and mark for a protocol badge on a chat avatar. Approximate brand
/// hues, not the official assets.
#[must_use]
pub(crate) const fn protocol_badge_ink(
    protocol: thinwire_protocol::ProtocolId,
) -> (Color32, Color32) {
    use thinwire_protocol::ProtocolId;
    let mark = Color32::from_rgb(255, 255, 255);
    let fill = match protocol {
        ProtocolId::Telegram => hex(0x2A_AB_EE),
        ProtocolId::WhatsApp => hex(0x25_D3_66),
        ProtocolId::Signal => hex(0x3A_76_F0),
        ProtocolId::Discord => hex(0x58_65_F2),
        ProtocolId::Slack => hex(0xE0_1E_5A),
    };
    (fill, mark)
}

/// Palette for the theme that `ui` draws with now.
#[must_use]
pub(crate) fn palette(ui: &egui::Ui) -> &'static Palette {
    Palette::of(if ui.visuals().dark_mode {
        Theme::Dark
    } else {
        Theme::Light
    })
}

/// Spacing scale in logical px (4, 8, 12, 16, 24, 32). Later items add
/// the steps they use.
pub(crate) mod space {
    /// Badge padding and the gap between the two lines of an inbox row.
    pub(crate) const XS: f32 = 4.0;
    pub(crate) const S: f32 = 8.0;
    pub(crate) const M: f32 = 12.0;
    /// Panel padding and the gap between a header and its content.
    pub(crate) const L: f32 = 16.0;
    /// Login card padding.
    pub(crate) const XL: f32 = 24.0;
}

/// Max width of the login and first-run card.
pub(crate) const LOGIN_CARD_WIDTH: f32 = 400.0;

/// Corner radii.
pub(crate) mod radius {
    /// Buttons and menu items.
    pub(crate) const CONTROL: u8 = 8;
    /// Inbox rows, account rows, and their highlights.
    pub(crate) const ROW: u8 = 10;
    /// Search field and other single-line boxes.
    pub(crate) const FIELD_BOX: u8 = 10;
    /// Compose field. A full pill at one line.
    pub(crate) const FIELD: u8 = 22;
    /// Message bubbles.
    pub(crate) const BUBBLE: u8 = 16;
    /// Sender-side bottom corner on the last bubble of a run.
    pub(crate) const BUBBLE_TAIL: u8 = 5;
    /// Cards and banners.
    pub(crate) const CARD: u8 = 16;
    /// Badges and pills. The spec value 999 does not fit in egui's `u8`
    /// radius. This value rounds any badge into a pill.
    pub(crate) const PILL: u8 = u8::MAX;
}

/// Minimum height of a click target.
pub(crate) const MIN_TARGET: f32 = 32.0;
/// Focus ring width.
const FOCUS_WIDTH: f32 = 2.0;
/// Content inset inside a full-width row highlight.
///
/// The list headers, account rows, and inbox rows share the x so the title,
/// the protocol tile, and the chat avatar start on one line. The highlight
/// fill stays the full row; only the content moves in.
pub(crate) const ROW_INSET: Vec2 = Vec2::new(space::M, space::S);

/// Content rect of a row highlight.
///
/// `right_gutter` is extra space on the right. Inbox rows pass the scroll
/// bar width. Account chips pass `0`.
#[must_use]
pub(crate) fn row_content_rect(row: egui::Rect, right_gutter: f32) -> egui::Rect {
    egui::Rect::from_min_max(
        row.min + ROW_INSET,
        row.max - ROW_INSET - egui::vec2(right_gutter, 0.0),
    )
}

/// Frame of the account sidebar: the `rail` fill with a little air.
pub(crate) fn rail_frame(palette: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(palette.rail)
        .inner_margin(Margin::symmetric(space::S as i8, space::M as i8))
}

/// Frame of the chat list column. The rows inset themselves, so there is no
/// margin at the bottom and the scroll bar sits at the panel edge.
pub(crate) fn list_frame(palette: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(palette.sidebar)
        .inner_margin(Margin {
            left: space::S as i8,
            right: space::S as i8,
            top: space::M as i8,
            bottom: 0,
        })
}

/// Side of the protocol tile in the account sidebar.
pub(crate) const TILE_SIDE: f32 = 36.0;

/// Height of the search field in the chat list.
pub(crate) const SEARCH_HEIGHT: f32 = 38.0;

/// Padding inside the search field: room for the glass, then the hint.
pub(crate) fn search_field_margin() -> Margin {
    Margin::symmetric(space::M as i8, 8)
}

/// Inter variable font (SIL OFL 1.1). Notice: `assets/fonts/OFL.txt`.
const INTER: &[u8] = include_bytes!("../../assets/fonts/InterVariable.ttf");

/// One Inter weight: font key, `wght` axis value, and the family that uses it.
struct Weight {
    key: &'static str,
    wght: f32,
    family: fn() -> FontFamily,
}

const WEIGHTS: [Weight; 3] = [
    Weight {
        key: "inter-regular",
        wght: 400.0,
        family: || FontFamily::Proportional,
    },
    Weight {
        key: "inter-medium",
        wght: 500.0,
        family: medium,
    },
    Weight {
        key: "inter-semibold",
        wght: 600.0,
        family: semibold,
    },
];

/// Inter at weight 500 (buttons, chat names).
#[must_use]
pub(crate) fn medium() -> FontFamily {
    FontFamily::Name("medium".into())
}

/// Inter at weight 600 (headings, unread chat names).
#[must_use]
pub(crate) fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

/// Type scale in logical px. No text is smaller than [`size::CAPTION`].
pub(crate) mod size {
    /// Times, day breaks, meta, badges (egui `Small`).
    pub(crate) const CAPTION: f32 = 12.0;
    /// Chat preview, field help.
    pub(crate) const SECONDARY: f32 = 13.0;
    /// Buttons.
    pub(crate) const BUTTON: f32 = 14.0;
    /// Code.
    pub(crate) const MONO: f32 = 14.0;
    /// Message body, compose, form fields.
    pub(crate) const BODY: f32 = 15.0;
    /// The brand name, the chat list header, the thread title, and empty-state
    /// titles.
    pub(crate) const HEADING: f32 = 18.0;
    /// First-run and login step titles.
    pub(crate) const DISPLAY: f32 = 24.0;
}

/// Chat preview and field help: 13 px Regular.
#[must_use]
pub(crate) fn secondary() -> TextStyle {
    TextStyle::Name("secondary".into())
}

/// First-run and login titles: 24 px SemiBold.
#[must_use]
pub(crate) fn display() -> TextStyle {
    TextStyle::Name("display".into())
}

/// Chat name in the inbox: 15 px Medium. Unread rows use [`semibold`].
#[must_use]
pub(crate) fn row_title() -> TextStyle {
    TextStyle::Name("row_title".into())
}

fn text_styles() -> BTreeMap<TextStyle, FontId> {
    [
        (
            TextStyle::Small,
            FontId::new(size::CAPTION, FontFamily::Proportional),
        ),
        (
            secondary(),
            FontId::new(size::SECONDARY, FontFamily::Proportional),
        ),
        (
            TextStyle::Body,
            FontId::new(size::BODY, FontFamily::Proportional),
        ),
        (TextStyle::Button, FontId::new(size::BUTTON, medium())),
        (row_title(), FontId::new(size::BODY, medium())),
        (TextStyle::Heading, FontId::new(size::HEADING, semibold())),
        (display(), FontId::new(size::DISPLAY, semibold())),
        (
            TextStyle::Monospace,
            FontId::new(size::MONO, FontFamily::Monospace),
        ),
    ]
    .into()
}

/// egui default fonts with Inter first. The egui fonts stay as fallback
/// for emoji and symbols. Monospace stays Hack.
#[must_use]
fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let fallback = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    for weight in &WEIGHTS {
        let tweak = FontTweak {
            coords: VariationCoords::new([(*b"wght", weight.wght)]),
            ..FontTweak::default()
        };
        fonts.font_data.insert(
            weight.key.to_owned(),
            Arc::new(FontData::from_static(INTER).tweak(tweak)),
        );
        let mut family = vec![weight.key.to_owned()];
        family.extend(fallback.iter().cloned());
        fonts.families.insert((weight.family)(), family);
    }
    fonts
}

/// Register the fonts and the light and dark styles. Call once when the
/// app starts. The font bytes are static, so this does no file I/O.
pub fn install(ctx: &egui::Context) {
    // Still images only. The loader decodes off the UI thread.
    egui_extras::install_image_loaders(ctx);
    ctx.set_fonts(fonts());
    for theme in [Theme::Dark, Theme::Light] {
        ctx.set_style_of(theme, style(Palette::of(theme), theme));
    }
}

fn style(palette: &Palette, theme: Theme) -> egui::Style {
    let mut style = egui::Style {
        visuals: visuals(palette, theme),
        text_styles: text_styles(),
        animation_time: super::motion::PANEL,
        ..egui::Style::default()
    };
    let spacing = &mut style.spacing;
    spacing.item_spacing = Vec2::new(space::S, 6.0);
    spacing.button_padding = Vec2::new(space::M + 2.0, 7.0);
    spacing.interact_size.y = MIN_TARGET;
    spacing.window_margin = Margin::same(space::M as i8);
    spacing.menu_margin = Margin::same(space::S as i8);
    style
}

/// egui visuals built from `palette`.
#[must_use]
pub(crate) fn visuals(palette: &Palette, theme: Theme) -> egui::Visuals {
    let mut visuals = match theme {
        Theme::Dark => egui::Visuals::dark(),
        Theme::Light => egui::Visuals::light(),
    };
    let control = CornerRadius::same(radius::CONTROL);
    let widget = |fill: Color32, stroke: Stroke, fg: Color32| egui::style::WidgetVisuals {
        bg_fill: fill,
        weak_bg_fill: fill,
        bg_stroke: stroke,
        corner_radius: control,
        fg_stroke: Stroke::new(1.0, fg),
        expansion: 0.0,
    };

    // Flat controls: a soft fill and no outline. Text fields draw their own
    // hairline through `field_frame`, so the 3:1 boundary stays where it is
    // needed. Keyboard focus and press show the accent ring on `active`.
    let hover_fill = super::motion::mix(palette.surface, palette.text, 0.08);
    visuals.widgets.noninteractive = widget(
        palette.sidebar,
        Stroke::new(1.0, palette.border),
        palette.text,
    );
    visuals.widgets.inactive = widget(palette.surface, Stroke::NONE, palette.text);
    visuals.widgets.hovered = widget(hover_fill, Stroke::NONE, palette.text);
    visuals.widgets.active = widget(
        palette.selected_row,
        Stroke::new(1.5, palette.accent),
        palette.text,
    );
    visuals.widgets.open = widget(hover_fill, Stroke::NONE, palette.text);

    visuals.selection.bg_fill = palette.selected_row;
    // Selected text and the focused text-field ring. `accent` on
    // `selected_row` is under 4.5:1 in light, so this stays `text`.
    visuals.selection.stroke = Stroke::new(FOCUS_WIDTH, palette.text);
    visuals.text_cursor.stroke = Stroke::new(FOCUS_WIDTH, palette.accent);

    visuals.weak_text_color = Some(palette.text3);
    visuals.hyperlink_color = palette.accent;
    visuals.warn_fg_color = palette.warn;
    visuals.error_fg_color = palette.error;
    visuals.panel_fill = palette.sidebar;
    visuals.window_fill = palette.bg;
    visuals.window_stroke = Stroke::new(1.0, palette.border);
    visuals.window_corner_radius = CornerRadius::same(radius::CARD);
    visuals.menu_corner_radius = CornerRadius::same(radius::ROW);
    let shadow = |blur: u8, alpha: u8| egui::Shadow {
        offset: [0, 6],
        blur,
        spread: 0,
        color: Color32::from_black_alpha(alpha),
    };
    let dark = theme == Theme::Dark;
    visuals.popup_shadow = shadow(20, if dark { 110 } else { 38 });
    visuals.window_shadow = shadow(28, if dark { 130 } else { 46 });
    visuals.faint_bg_color = palette.surface;
    visuals.extreme_bg_color = palette.input;
    visuals.text_edit_bg_color = Some(palette.input);
    visuals.code_bg_color = palette.surface;
    visuals
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 2.x relative luminance.
    fn luminance(color: Color32) -> f64 {
        let channel = |value: u8| {
            let c = f64::from(value) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
    }

    fn contrast(a: Color32, b: Color32) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    const TEXT_AA: f64 = 4.5;
    const UI_AA: f64 = 3.0;

    fn text_pairs(p: &Palette) -> Vec<(&'static str, Color32, Color32)> {
        vec![
            ("text/bg", p.text, p.bg),
            ("text/sidebar", p.text, p.sidebar),
            ("text/rail", p.text, p.rail),
            ("text2/rail", p.text2, p.rail),
            ("text3/rail", p.text3, p.rail),
            ("ok/rail", p.ok, p.rail),
            ("warn/rail", p.warn, p.rail),
            ("error/rail", p.error, p.rail),
            ("accent/rail", p.accent, p.rail),
            ("accent/hover", p.accent, p.hover),
            ("text/surface", p.text, p.surface),
            ("text/hover", p.text, p.hover),
            ("text/input", p.text, p.input),
            ("text/selected_row", p.text, p.selected_row),
            ("text2/bg", p.text2, p.bg),
            ("text2/sidebar", p.text2, p.sidebar),
            ("text2/selected_row", p.text2, p.selected_row),
            ("text2/hover", p.text2, p.hover),
            ("text2/surface", p.text2, p.surface),
            ("text3/bg", p.text3, p.bg),
            ("text3/sidebar", p.text3, p.sidebar),
            ("text3/surface", p.text3, p.surface),
            ("text3/hover", p.text3, p.hover),
            ("text3/selected_row", p.text3, p.selected_row),
            ("text3/input", p.text3, p.input),
            ("on_out/out", p.on_out, p.out),
            ("out_meta/out", p.out_meta, p.out),
            ("link/surface", p.link, p.surface),
            ("out_link/out", p.out_link, p.out),
            ("accent/bg", p.accent, p.bg),
            ("accent/sidebar", p.accent, p.sidebar),
            ("accent/input", p.accent, p.input),
            ("on_accent/accent", p.on_accent, p.accent),
            ("on_badge/badge", p.on_badge, p.badge),
            ("warn/bg", p.warn, p.bg),
            ("warn/sidebar", p.warn, p.sidebar),
            ("error/bg", p.error, p.bg),
            ("error/sidebar", p.error, p.sidebar),
            ("error/surface", p.error, p.surface),
            ("out_error/out", p.out_error, p.out),
            ("ok/bg", p.ok, p.bg),
            ("ok/sidebar", p.ok, p.sidebar),
        ]
    }

    fn ui_pairs(p: &Palette) -> Vec<(&'static str, Color32, Color32)> {
        vec![
            ("border_strong/input", p.border_strong, p.input),
            ("border_strong/sidebar", p.border_strong, p.sidebar),
            ("border_strong/rail", p.border_strong, p.rail),
            ("border_strong/bg", p.border_strong, p.bg),
            ("focus text/input", p.text, p.input),
            ("accent/surface", p.accent, p.surface),
        ]
    }

    #[test]
    fn contrast_function_matches_wcag_reference() {
        let black_white = contrast(Color32::BLACK, Color32::WHITE);
        assert!((black_white - 21.0).abs() < 0.01, "{black_white}");
        assert!((contrast(hex(0x77_77_77), Color32::WHITE) - 4.48).abs() < 0.01);
    }

    #[test]
    fn muted_unread_ring_clears_non_text_contrast_on_every_row_fill() {
        for (theme, palette) in [("dark", Palette::DARK), ("light", Palette::LIGHT)] {
            let ring = muted_badge_ring(&palette);
            assert_eq!(ring, palette.text, "{theme} ring follows the row title");
            for (state, selected, hovered) in [
                ("normal", false, false),
                ("hover", false, true),
                ("selected", true, false),
                ("selected hover", true, true),
            ] {
                let fill = inbox_row_backdrop(&palette, selected, hovered);
                let ratio = contrast(ring, fill);
                assert!(ratio >= UI_AA, "{theme} {state}: {ratio:.2} < {UI_AA}");
            }
        }
    }

    #[test]
    fn a_settled_row_fill_is_the_row_backdrop() {
        for palette in [Palette::DARK, Palette::LIGHT] {
            for selected in [false, true] {
                for hovered in [false, true] {
                    assert_eq!(
                        inbox_row_fill(
                            &palette,
                            f32::from(u8::from(selected)),
                            f32::from(u8::from(hovered))
                        ),
                        inbox_row_backdrop(&palette, selected, hovered),
                        "selected {selected} hovered {hovered}"
                    );
                }
            }
        }
    }

    #[test]
    fn protocol_mute_text_stays_readable_on_the_menu_fill() {
        for (theme, palette) in [("dark", Palette::DARK), ("light", Palette::LIGHT)] {
            let ink = protocol_mute_ink(&palette);
            assert_eq!(ink.on, palette.bg, "{theme} menu fill");
            for (part, color) in [("title", ink.title), ("reason", ink.reason)] {
                let ratio = contrast(color, ink.on);
                assert!(ratio >= TEXT_AA, "{theme} {part}: {ratio:.2} < {TEXT_AA}");
            }
        }
    }

    #[test]
    fn every_protocol_badge_has_a_fill_and_a_mark() {
        for protocol in thinwire_protocol::ProtocolId::ALL {
            let (fill, mark) = protocol_badge_ink(protocol);
            assert_ne!(fill, mark, "{protocol}");
            assert_eq!(mark, Color32::WHITE, "{protocol}");
        }
    }

    #[test]
    fn every_palette_color_gives_initials_at_least_4_5_contrast() {
        for accent in 0..=6 {
            for (theme, dark) in [("light", false), ("dark", true)] {
                let ink = avatar_ink(accent, dark);
                assert_ne!(ink.text, Color32::WHITE, "{theme} accent {accent} text");
                let ratio = contrast(ink.text, ink.fill);
                assert!(
                    ratio >= TEXT_AA,
                    "{theme} accent {accent}: {ratio:.2} < {TEXT_AA}"
                );
            }
        }
        assert_eq!(avatar_ink(9, false).text, avatar_ink(0, false).text);
    }

    #[test]
    fn every_text_pair_meets_aa_in_both_themes() {
        for (name, palette) in [("dark", Palette::DARK), ("light", Palette::LIGHT)] {
            for (pair, fg, bg) in text_pairs(&palette) {
                let ratio = contrast(fg, bg);
                assert!(ratio >= TEXT_AA, "{name} {pair}: {ratio:.2} < {TEXT_AA}");
            }
            for (pair, fg, bg) in ui_pairs(&palette) {
                let ratio = contrast(fg, bg);
                assert!(ratio >= UI_AA, "{name} {pair}: {ratio:.2} < {UI_AA}");
            }
        }
    }

    #[test]
    fn visuals_use_palette_text_and_keep_dark_flag() {
        for theme in [Theme::Dark, Theme::Light] {
            let palette = Palette::of(theme);
            let visuals = visuals(palette, theme);
            assert_eq!(visuals.dark_mode, theme == Theme::Dark);
            assert_eq!(visuals.text_color(), palette.text);
            assert_eq!(visuals.weak_text_color(), palette.text3);
            assert_eq!(visuals.panel_fill, palette.sidebar);
            assert_eq!(visuals.text_edit_bg_color(), palette.input);
            assert_eq!(visuals.selection.bg_fill, palette.selected_row);
            assert_ne!(visuals.selection.bg_fill, palette.out);
        }
    }

    #[test]
    fn install_sets_a_style_for_each_theme() {
        let ctx = egui::Context::default();
        install(&ctx);
        for theme in [Theme::Dark, Theme::Light] {
            let style = ctx.style_of(theme);
            assert_eq!(style.visuals.panel_fill, Palette::of(theme).sidebar);
            assert!(style.spacing.interact_size.y >= MIN_TARGET);
        }
    }

    #[test]
    fn account_and_inbox_content_share_a_left_edge() {
        let panel_left = 8.0;
        let row = egui::Rect::from_min_size(egui::pos2(panel_left, 20.0), egui::vec2(264.0, 52.0));
        let account = row_content_rect(row, 0.0);
        let inbox = row_content_rect(row, 10.0);
        assert_eq!(account.min.x, inbox.min.x);
        assert_eq!(account.min.y - row.min.y, space::S);
        assert_eq!(inbox.min.y - row.min.y, space::S);
        assert_eq!(row.max.x - account.max.x, space::M);
        assert_eq!(row.max.x - inbox.max.x, space::M + 10.0);
    }

    #[test]
    fn sidebar_frames_fill_with_their_own_panel_color() {
        for palette in [Palette::DARK, Palette::LIGHT] {
            assert_eq!(rail_frame(&palette).fill, palette.rail);
            assert_eq!(list_frame(&palette).fill, palette.sidebar);
            assert_ne!(palette.rail, palette.sidebar, "two layers, not one");
        }
    }

    #[test]
    fn a_sender_keeps_one_color_and_it_is_readable_on_the_inbound_bubble() {
        for (dark, palette) in [(true, Palette::DARK), (false, Palette::LIGHT)] {
            for sender in ["Ada", "Nora", "Milo", "Ada Park", ""] {
                let ink = sender_ink(sender, dark);
                assert_eq!(ink, sender_ink(sender, dark));
                let ratio = contrast(ink, palette.surface);
                assert!(ratio >= TEXT_AA, "{sender:?} dark={dark}: {ratio:.2}");
            }
        }
    }

    #[test]
    fn search_field_margin_insets_the_icon_and_the_hint() {
        let margin = search_field_margin();
        assert_eq!(margin.left, space::M as i8);
        assert_eq!(margin.right, space::M as i8);
        assert_eq!(margin.top, 8);
        assert_eq!(margin.bottom, 8);
    }

    #[test]
    fn no_text_style_is_below_caption_size() {
        for (style, font) in text_styles() {
            assert!(font.size >= size::CAPTION, "{style:?} is {}", font.size);
        }
    }

    #[test]
    fn named_styles_and_families_are_registered() {
        let styles = text_styles();
        assert!(styles.contains_key(&secondary()));
        assert!(styles.contains_key(&display()));
        assert_eq!(
            styles.get(&row_title()),
            Some(&FontId::new(size::BODY, medium()))
        );
        let fonts = fonts();
        for family in [FontFamily::Proportional, medium(), semibold()] {
            let first = fonts.families.get(&family).and_then(|list| list.first());
            assert!(
                first.is_some_and(|key| key.starts_with("inter-")),
                "{family:?}"
            );
        }
        assert_eq!(
            fonts.families.get(&FontFamily::Monospace),
            FontDefinitions::default()
                .families
                .get(&FontFamily::Monospace)
        );
    }

    #[test]
    fn inter_is_a_variable_font_with_a_weight_axis() {
        let axes = FontData::from_static(INTER).variation_axes();
        let wght = axes
            .iter()
            .find(|axis| axis.tag == *b"wght")
            .expect("wght axis");
        for weight in &WEIGHTS {
            assert!(wght.range.contains(weight.wght));
        }
    }

    #[test]
    fn installed_fonts_cover_the_shell_glyphs() {
        let ctx = egui::Context::default();
        install(&ctx);
        // The first pass builds the font atlas. No renderer runs here.
        ctx.run_ui(egui::RawInput::default(), |_| {})
            .textures_delta
            .clear();
        let text = "●○…·—ãçéñü";
        for family in [FontFamily::Proportional, medium(), semibold()] {
            let font = FontId::new(size::BODY, family);
            assert!(ctx.fonts_mut(|fonts| fonts.has_glyphs(&font, text)));
        }
    }

    /// UI files must read colors from the palette, never from literals.
    #[test]
    fn shell_files_have_no_color_literals() {
        for (file, source) in [
            ("ui.rs", include_str!("ui.rs")),
            ("auth.rs", include_str!("auth.rs")),
            ("whatsapp_gate.rs", include_str!("whatsapp_gate.rs")),
        ] {
            for needle in ["Color32::from_rgb(", "Color32::from_gray("] {
                assert!(!source.contains(needle), "{file} has {needle}");
            }
        }
    }
}
