# GUI A: egui + eframe + glow (wgpu off)

**Status:** accepted

## Context
Product-council (2026-09-20) locked the desktop renderer after option B. The shell is a thin native messenger for Windows, macOS, and Linux. eframe 0.36 turns wgpu on by default. Glow is enough for this UI. The council rejected a wgpu-default stack and gpui until it says otherwise.

## Decision
thinwire uses **egui + eframe + glow**. wgpu stays off.

The workspace `eframe` crate sets `default-features = false`. It enables `glow`, `default_fonts`, `accesskit`, `wayland`, `x11`, and `links`. It does not enable `wgpu` or `wgpu_no_default_features`. Cargo does not let a consumer pass `winit/default` as an eframe feature (slash features are private). thinwire depends on `winit` with default features so unification matches that eframe default. If `NativeOptions` must pick a renderer, it picks Glow.

Discord stays bot/OAuth only. README Critic risk bullets stay unchanged.

## Consequences
`Cargo.lock` does not pull the eframe wgpu renderer stack. Linux still needs X11 or Wayland plus OpenGL libraries. A later renderer change needs a new council lock.

Rejected: eframe wgpu default; gpui.
