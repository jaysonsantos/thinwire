//! Short transitions for the shell.
//!
//! A transition here never holds input and never moves the layout on its
//! own. The first frame of a widget shows its end state, so a new window and
//! each pixel snapshot start settled. `Style::animation_time` of 0 turns
//! every transition off: they then jump to the end state. OS reduce-motion
//! sets that time to 0.

use eframe::egui::{self, Color32, emath::easing};

/// Hover feedback on rows, chips, and the jump button.
pub(crate) const HOVER: f32 = 0.10;
/// Selection, focus rings, and enabled / disabled colors.
pub(crate) const STATE: f32 = 0.14;
/// Content that replaces other content: a chat, a login step, a busy line.
pub(crate) const FADE: f32 = 0.16;
/// Panels that slide in or out, such as the status strip. egui reads it from
/// `Style::animation_time`.
pub(crate) const PANEL: f32 = 0.18;

/// Motion is on unless the style turns it off. OS reduce-motion sets
/// [`animation_time`] to 0.
pub(crate) fn enabled(ctx: &egui::Context) -> bool {
    ctx.global_style().animation_time > 0.0
}

/// Still text in place of a spinner when the OS asks for less motion.
pub(crate) const WORKING: &str = "Working…";

/// A spinner, or the still text [`WORKING`] when motion is off.
///
/// [`egui::Spinner`] asks for another frame on every paint and turns from
/// the clock. `Style::animation_time` does not stop it, so every loading
/// spinner in the shell goes through here.
pub(crate) fn spinner(ui: &mut egui::Ui) {
    paint_spinner(ui, None);
}

/// The top-bar spinner: a fixed size and color, or [`WORKING`] when motion
/// is off.
pub(crate) fn busy_spinner(ui: &mut egui::Ui, size: f32, color: Color32) {
    paint_spinner(ui, Some((size, color)));
}

fn paint_spinner(ui: &mut egui::Ui, look: Option<(f32, Color32)>) {
    if !enabled(ui.ctx()) {
        ui.label(egui::RichText::new(WORKING).weak());
        return;
    }
    match look {
        Some((size, color)) => {
            ui.add(egui::Spinner::new().size(size).color(color));
        }
        None => {
            ui.spinner();
        }
    }
}

/// Seconds for one egui transition. `0` when the OS asks for less motion.
pub(crate) fn animation_time(reduce_motion: bool) -> f32 {
    if reduce_motion { 0.0 } else { PANEL }
}

/// Write [`animation_time`] into the light style and the dark style.
pub(crate) fn apply(ctx: &egui::Context, reduce_motion: bool) {
    let time = animation_time(reduce_motion);
    if ctx.style_of(egui::Theme::Light).animation_time == time
        && ctx.style_of(egui::Theme::Dark).animation_time == time
    {
        return;
    }
    ctx.all_styles_mut(|style| style.animation_time = time);
}

/// Apply the latest OS value. `true` when it changed since `last`, so the
/// caller can repaint at once. The first value is stored and applied.
pub(crate) fn follow(ctx: &egui::Context, last: &mut Option<bool>, reduce_motion: bool) -> bool {
    apply(ctx, reduce_motion);
    let changed = last.is_some_and(|prev| prev != reduce_motion);
    *last = Some(reduce_motion);
    changed
}

/// 0 → 1 when `on` turns true, 1 → 0 when it turns false. Starts fast and
/// slows down at the end, both ways.
pub(crate) fn toggle(ctx: &egui::Context, id: egui::Id, on: bool, secs: f32) -> f32 {
    if !enabled(ctx) {
        return if on { 1.0 } else { 0.0 };
    }
    ctx.animate_bool_with_time_and_easing(id, on, secs, easing::cubic_out)
}

/// Opacity for content that `key` names: 0 → 1 over `secs` after `key`
/// changes. The first key that `id` sees shows at once.
///
/// The frame of the change draws at 0. That frame can show a state that the
/// next frame moves, for example a thread before `stick_to_bottom` reaches
/// the newest message.
pub(crate) fn fade_in_on_change(
    ctx: &egui::Context,
    id: egui::Id,
    key: egui::Id,
    secs: f32,
) -> f32 {
    let now = ctx.input(|input| input.time);
    let start = ctx.data_mut(|data| {
        let seen = data.get_temp_mut_or_insert_with(id, || (key, f64::NEG_INFINITY));
        if seen.0 != key {
            *seen = (key, now);
        }
        seen.1
    });
    if !enabled(ctx) || secs <= 0.0 {
        return 1.0;
    }
    let t = ((now - start) / f64::from(secs)) as f32;
    if t >= 1.0 {
        return 1.0;
    }
    ctx.request_repaint();
    easing::cubic_out(t.max(0.0))
}

/// Color between `from` (t = 0) and `to` (t = 1). Both ends are exact.
#[must_use]
pub(crate) fn mix(from: Color32, to: Color32, t: f32) -> Color32 {
    from.lerp_to_gamma(to, t.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One pass at `time`. True when it asked for the next frame at once.
    fn frame(ctx: &egui::Context, time: f64, mut run: impl FnMut(&egui::Context)) -> bool {
        let input = egui::RawInput {
            time: Some(time),
            predicted_dt: 1.0 / 60.0,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| run(ui.ctx()));
        output.textures_delta.clear();
        output
            .viewport_output
            .values()
            .any(|viewport| viewport.repaint_delay.is_zero())
    }

    #[test]
    fn mix_keeps_both_ends() {
        let a = Color32::from_rgb(10, 20, 30);
        let b = Color32::from_rgb(200, 150, 100);
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
        assert_eq!(mix(a, b, -1.0), a);
        assert_eq!(mix(a, b, 2.0), b);
        assert_eq!(mix(Color32::TRANSPARENT, b, 1.0), b);
    }

    #[test]
    fn a_new_key_fades_in_and_the_first_key_shows_at_once() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("fade");
        let mut seen = 0.0;
        let key = |name: &str| egui::Id::new(name);
        frame(&ctx, 1.0, |ctx| {
            seen = fade_in_on_change(ctx, id, key("a"), FADE)
        });
        assert_eq!(seen, 1.0, "first key: no fade");
        let repaint = frame(&ctx, 2.0, |ctx| {
            seen = fade_in_on_change(ctx, id, key("b"), FADE);
        });
        assert_eq!(seen, 0.0, "the frame of the change draws at 0");
        assert!(repaint, "the fade asks for the next frame");
        frame(&ctx, 2.0 + f64::from(FADE) / 2.0, |ctx| {
            seen = fade_in_on_change(ctx, id, key("b"), FADE);
        });
        assert!(seen > 0.5 && seen < 1.0, "half way, eased: {seen}");
        frame(&ctx, 2.0 + f64::from(FADE), |ctx| {
            seen = fade_in_on_change(ctx, id, key("b"), FADE);
        });
        assert_eq!(seen, 1.0);
        // egui repaints one more frame after the last request.
        let repaint = frame(&ctx, 3.0, |ctx| {
            seen = fade_in_on_change(ctx, id, key("b"), FADE);
        });
        assert!(!repaint, "a settled fade does not keep the window busy");
    }

    #[test]
    fn zero_animation_time_turns_motion_off() {
        let ctx = egui::Context::default();
        ctx.all_styles_mut(|style| style.animation_time = 0.0);
        let id = egui::Id::new("fade");
        let mut seen = (0.0, 0.0);
        frame(&ctx, 1.0, |ctx| {
            seen.0 = fade_in_on_change(ctx, id, egui::Id::new("a"), FADE);
            seen.1 = toggle(ctx, id.with("t"), false, HOVER);
        });
        frame(&ctx, 1.01, |ctx| {
            seen.0 = fade_in_on_change(ctx, id, egui::Id::new("b"), FADE);
            seen.1 = toggle(ctx, id.with("t"), true, HOVER);
        });
        assert_eq!(seen, (1.0, 1.0));
        let repaint = frame(&ctx, 1.02, |ctx| {
            seen.0 = fade_in_on_change(ctx, id, egui::Id::new("b"), FADE);
            seen.1 = toggle(ctx, id.with("t"), true, HOVER);
        });
        assert!(!repaint, "no motion, no extra frames");
    }

    #[test]
    fn reduce_motion_sets_animation_time_to_zero_on_both_themes() {
        let ctx = egui::Context::default();
        apply(&ctx, false);
        assert_eq!(ctx.style_of(egui::Theme::Light).animation_time, PANEL);
        assert_eq!(ctx.style_of(egui::Theme::Dark).animation_time, PANEL);
        apply(&ctx, true);
        assert_eq!(ctx.style_of(egui::Theme::Light).animation_time, 0.0);
        assert_eq!(ctx.style_of(egui::Theme::Dark).animation_time, 0.0);
        assert!(!enabled(&ctx));
    }

    /// The OS value can change while the window is open. The next follow
    /// applies it.
    #[test]
    fn a_later_reduce_motion_value_applies_without_a_restart() {
        let ctx = egui::Context::default();
        let mut last = None;
        assert!(!follow(&ctx, &mut last, false));
        assert_eq!(ctx.global_style().animation_time, PANEL);
        assert!(follow(&ctx, &mut last, true));
        assert_eq!(ctx.global_style().animation_time, 0.0);
        assert!(!follow(&ctx, &mut last, true));
        assert!(follow(&ctx, &mut last, false));
        assert_eq!(ctx.global_style().animation_time, PANEL);
    }

    /// Every spinner in the shell is this helper. A direct call in any
    /// source file would keep turning when the OS asks for less motion.
    #[test]
    fn every_spinner_goes_through_the_reduce_motion_helper() {
        let call = concat!("ui.", "spinner()");
        let constructed = concat!("Spinner::", "new");
        let defaulted = concat!("Spinner::", "default");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rust_sources(&root, &mut files);
        let helper = std::path::Path::new("app").join("motion.rs");
        let mut saw_helper = false;
        for path in &files {
            let src = std::fs::read_to_string(path).expect("shell source");
            assert!(!imports_spinner(&src), "{}", path.display());
            if path.ends_with(&helper) {
                saw_helper = true;
                assert_eq!(src.matches(call).count(), 1, "{}", path.display());
                assert_eq!(src.matches(constructed).count(), 1, "{}", path.display());
                assert_eq!(src.matches(defaulted).count(), 0, "{}", path.display());
            } else {
                assert_eq!(src.matches(call).count(), 0, "{}", path.display());
                assert_eq!(src.matches(constructed).count(), 0, "{}", path.display());
                assert_eq!(src.matches(defaulted).count(), 0, "{}", path.display());
            }
        }
        assert!(saw_helper, "the scan includes the motion helper");
        assert!(files.len() > 1, "the scan includes every shell source");
    }

    /// A `use` of `Spinner`, including `use egui::Spinner`.
    fn imports_spinner(src: &str) -> bool {
        let name = concat!("::", "Spinner");
        src.lines().any(|line| {
            let code = line.split("//").next().unwrap_or(line);
            code.contains("use ") && code.contains(name)
        })
    }

    fn collect_rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let entries = std::fs::read_dir(dir).unwrap_or_else(|error| {
            panic!("read {}: {error}", dir.display());
        });
        for entry in entries {
            let entry = entry.unwrap_or_else(|error| panic!("read {}: {error}", dir.display()));
            let path = entry.path();
            if path.is_dir() {
                collect_rust_sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    #[test]
    fn a_toggle_moves_toward_its_target_and_settles() {
        let ctx = egui::Context::default();
        let id = egui::Id::new("hover");
        let mut value = 0.0;
        frame(&ctx, 1.0, |ctx| value = toggle(ctx, id, false, HOVER));
        assert_eq!(value, 0.0);
        frame(&ctx, 1.0 + 1.0 / 60.0, |ctx| {
            value = toggle(ctx, id, true, HOVER)
        });
        assert!(value > 0.0 && value < 1.0, "{value}");
        let mut time = 1.0 + 1.0 / 60.0;
        for _ in 0..30 {
            time += 1.0 / 60.0;
            frame(&ctx, time, |ctx| value = toggle(ctx, id, true, HOVER));
        }
        assert_eq!(value, 1.0);
    }
}
