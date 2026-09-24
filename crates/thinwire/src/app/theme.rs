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
use thinwire_protocol::SupportClass;

/// Semantic colors for one theme. UI code reads colors only from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Palette {
    /// Thread / center panel.
    pub bg: Color32,
    /// Left panel and top bar.
    pub sidebar: Color32,
    /// Inbound bubble, cards, buttons.
    pub surface: Color32,
    /// Hover fill on rows and buttons.
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
    /// Links, focus, primary button.
    pub accent: Color32,
    /// Text on the primary button.
    pub on_accent: Color32,
    /// Selected chat row.
    pub selected_row: Color32,
    /// Outbound bubble fill.
    pub out: Color32,
    /// Outbound body text.
    pub on_out: Color32,
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
    /// "Not sent" inside an outbound bubble.
    pub out_error: Color32,
    /// Online dot and label.
    pub ok: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

impl Palette {
    pub(crate) const DARK: Self = Self {
        bg: hex(0x16_17_1A),
        sidebar: hex(0x1D_1E_22),
        surface: hex(0x2A_2C_31),
        hover: hex(0x33_36_3C),
        input: hex(0x10_11_14),
        border: hex(0x3A_3D_44),
        border_strong: hex(0x6B_70_7A),
        text: hex(0xE8_E9_EC),
        text2: hex(0xB3_B7_BF),
        text3: hex(0x9C_A1_AB),
        accent: hex(0x5A_A2_FF),
        on_accent: hex(0x0B_1A_2E),
        selected_row: hex(0x26_34_4A),
        out: hex(0x2B_52_78),
        on_out: hex(0xF2_F6_FB),
        out_meta: hex(0xB9_CC_E3),
        badge: hex(0x2A_68_CC),
        on_badge: hex(0xFF_FF_FF),
        warn: hex(0xF0_B4_55),
        error: hex(0xFF_7A_7A),
        out_error: hex(0xFF_C2_C2),
        ok: hex(0x5F_D3_9A),
    };

    pub(crate) const LIGHT: Self = Self {
        bg: hex(0xFF_FF_FF),
        sidebar: hex(0xF3_F4_F6),
        surface: hex(0xEE_F0_F3),
        hover: hex(0xE3_E6_EA),
        input: hex(0xFF_FF_FF),
        border: hex(0xC4_C8_CF),
        border_strong: hex(0x76_7C_87),
        text: hex(0x17_18_1B),
        text2: hex(0x45_4A_53),
        text3: hex(0x5E_63_6D),
        accent: hex(0x1A_63_D6),
        on_accent: hex(0xFF_FF_FF),
        selected_row: hex(0xDD_E8_FA),
        out: hex(0xD6_E8_FF),
        on_out: hex(0x17_18_1B),
        out_meta: hex(0x3E_57_75),
        badge: hex(0x1A_63_D6),
        on_badge: hex(0xFF_FF_FF),
        warn: hex(0x8A_53_00),
        error: hex(0xB3_26_1E),
        out_error: hex(0xB3_26_1E),
        ok: hex(0x1B_6E_43),
    };

    #[must_use]
    pub(crate) const fn of(theme: Theme) -> &'static Self {
        match theme {
            Theme::Dark => &Self::DARK,
            Theme::Light => &Self::LIGHT,
        }
    }

    /// Label color for a protocol support class.
    #[must_use]
    pub(crate) const fn support(&self, support: SupportClass) -> Color32 {
        match support {
            SupportClass::Supported => self.ok,
            SupportClass::Experimental => self.warn,
            SupportClass::Constrained => self.text2,
        }
    }
}

/// Center a card. Dark uses `surface`. Light uses `bg` and a `border`.
pub(crate) fn show_centered_card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let palette = palette(ui);
    let dark = ui.visuals().dark_mode;
    let width = ui.available_width().min(LOGIN_CARD_WIDTH);
    let inset = ((ui.available_width() - width) * 0.5).max(0.0);
    let mut frame = egui::Frame::new()
        .fill(if dark { palette.surface } else { palette.bg })
        .corner_radius(egui::CornerRadius::same(radius::BUBBLE))
        .inner_margin(egui::Margin::same(space::XL as i8));
    if !dark {
        frame = frame.stroke(egui::Stroke::new(1.0, palette.border));
    }
    ui.horizontal(|ui| {
        ui.add_space(inset);
        ui.vertical(|ui| {
            ui.set_max_width(width);
            frame.show(ui, add);
        });
    });
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
    /// Login card padding.
    pub(crate) const XL: f32 = 24.0;
}

/// Max width of the login and first-run card.
pub(crate) const LOGIN_CARD_WIDTH: f32 = 400.0;

/// Corner radii.
pub(crate) mod radius {
    /// Buttons, inputs, rows.
    pub(crate) const CONTROL: u8 = 6;
    /// Compose field. Tall enough to read as a pill at 40 px.
    pub(crate) const FIELD: u8 = 20;
    /// Bubbles and cards.
    pub(crate) const BUBBLE: u8 = 12;
    /// Sender-side bottom corner on the last bubble of a run.
    pub(crate) const BUBBLE_TAIL: u8 = 4;
    /// Badges and pills. The spec value 999 does not fit in egui's `u8`
    /// radius. This value rounds any badge into a pill.
    pub(crate) const PILL: u8 = u8::MAX;
}

/// Minimum height of a click target.
pub(crate) const MIN_TARGET: f32 = 32.0;
/// Inner margin of side and top panels.
pub(crate) const PANEL_MARGIN: i8 = 12;
/// Focus ring width.
const FOCUS_WIDTH: f32 = 2.0;

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
    /// Thread header, section titles.
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
    ctx.set_fonts(fonts());
    for theme in [Theme::Dark, Theme::Light] {
        ctx.set_style_of(theme, style(Palette::of(theme), theme));
    }
}

fn style(palette: &Palette, theme: Theme) -> egui::Style {
    let mut style = egui::Style {
        visuals: visuals(palette, theme),
        text_styles: text_styles(),
        ..egui::Style::default()
    };
    let spacing = &mut style.spacing;
    spacing.item_spacing = Vec2::new(space::S, 6.0);
    spacing.button_padding = Vec2::new(space::M, 6.0);
    spacing.interact_size.y = MIN_TARGET;
    spacing.window_margin = Margin::same(PANEL_MARGIN);
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

    visuals.widgets.noninteractive = widget(
        palette.sidebar,
        Stroke::new(1.0, palette.border),
        palette.text,
    );
    visuals.widgets.inactive = widget(
        palette.surface,
        Stroke::new(1.0, palette.border_strong),
        palette.text,
    );
    visuals.widgets.hovered = widget(
        palette.hover,
        Stroke::new(1.0, palette.accent),
        palette.text,
    );
    visuals.widgets.active = widget(
        palette.selected_row,
        Stroke::new(1.0, palette.accent),
        palette.text,
    );
    visuals.widgets.open = widget(
        palette.hover,
        Stroke::new(1.0, palette.border_strong),
        palette.text,
    );

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
    visuals.window_corner_radius = CornerRadius::same(radius::BUBBLE);
    visuals.menu_corner_radius = control;
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
    fn support_labels_meet_aa_on_the_sidebar() {
        for palette in [Palette::DARK, Palette::LIGHT] {
            for support in [
                SupportClass::Supported,
                SupportClass::Experimental,
                SupportClass::Constrained,
            ] {
                assert!(contrast(palette.support(support), palette.sidebar) >= TEXT_AA);
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
