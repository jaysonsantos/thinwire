# System theme by default

**Status:** accepted

## Context
Product-council (2026-09-20) locked thinwire appearance: follow the OS light/dark preference by default.

## Decision
Default theme mode is System. When System is selected, follow the OS light/dark preference and update when the OS changes. User override later may choose System, Light, or Dark. First paint and missing settings both use System. No accent, per-account, or scheduled themes in this decision.

## Consequences
eframe/egui must read the system theme on startup and when the OS theme changes. Settings persistence stores the mode enum. A missing config file means System.
