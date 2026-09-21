//! Appearance settings. Missing file means System (ADR 0005).
//!
//! Theme changes update in-memory state on the UI thread. Disk writes run on a
//! tokio `spawn_blocking` worker so the egui event loop never waits on
//! `create_dir_all` / `fs::write`. Overlapping jobs share a write lock and
//! re-check the persist epoch under that lock so `settings.toml` always matches
//! the latest theme.

use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use eframe::egui;

/// User override for light/dark. Default and missing config are System.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().trim_matches('"').to_ascii_lowercase().as_str() {
            "system" => Some(Self::System),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }

    #[must_use]
    pub const fn to_egui(self) -> egui::ThemePreference {
        match self {
            Self::System => egui::ThemePreference::System,
            Self::Light => egui::ThemePreference::Light,
            Self::Dark => egui::ThemePreference::Dark,
        }
    }

    #[must_use]
    pub const fn from_egui(preference: egui::ThemePreference) -> Self {
        match preference {
            egui::ThemePreference::System => Self::System,
            egui::ThemePreference::Light => Self::Light,
            egui::ThemePreference::Dark => Self::Dark,
        }
    }
}

impl fmt::Display for ThemeMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Disk write queued after a theme change. Run on a worker, never the UI thread.
pub struct PersistJob {
    path: PathBuf,
    contents: String,
    epoch: u64,
    latest: Arc<AtomicU64>,
    write_lock: Arc<Mutex<()>>,
}

impl PersistJob {
    /// Blocking write. Caller must run this on `spawn_blocking` / a test thread.
    pub fn run(self) {
        self.commit(true);
    }

    fn commit(self, check_before_lock: bool) {
        if check_before_lock && !self.is_current() {
            return;
        }
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.is_current() {
            return;
        }
        if let Some(parent) = self.path.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            tracing::warn!(error = %error, "theme settings directory was not created");
            return;
        }
        if let Err(error) = fs::write(&self.path, self.contents) {
            tracing::warn!(error = %error, "theme settings were not written");
        }
    }

    fn is_current(&self) -> bool {
        self.latest.load(Ordering::Acquire) == self.epoch
    }

    /// Skip the opportunistic pre-lock check so tests can overlap jobs that
    /// already passed a naive epoch load before filesystem I/O.
    #[cfg(test)]
    fn run_after_naive_pre_io_check(self) {
        self.commit(false);
    }
}

/// Persisted appearance. Only the theme mode is stored in this beat.
#[derive(Debug, Clone)]
pub struct Settings {
    theme: ThemeMode,
    path: PathBuf,
    persist_pending: bool,
    persist_epoch: u64,
    latest_persist: Arc<AtomicU64>,
    persist_lock: Arc<Mutex<()>>,
}

impl Settings {
    /// Load from the platform config dir. Missing or unreadable file → System.
    #[must_use]
    pub fn load() -> Self {
        Self::load_from(default_path())
    }

    #[must_use]
    pub fn load_from(path: PathBuf) -> Self {
        let theme = read_theme(&path).unwrap_or(ThemeMode::System);
        Self {
            theme,
            path,
            persist_pending: false,
            persist_epoch: 0,
            latest_persist: Arc::new(AtomicU64::new(0)),
            persist_lock: Arc::new(Mutex::new(())),
        }
    }

    #[must_use]
    pub const fn theme(&self) -> ThemeMode {
        self.theme
    }

    /// Apply the stored mode. System follows the OS and updates when it changes.
    pub fn apply(&self, ctx: &egui::Context) {
        ctx.set_theme(self.theme.to_egui());
    }

    /// Keep System mode in sync with live OS changes (egui-winit `ThemeChanged`).
    ///
    /// Returns true when the OS light/dark preference changed this frame so the
    /// caller can request an immediate repaint.
    pub fn follow_os_live(
        &self,
        ctx: &egui::Context,
        last_os_theme: &mut Option<egui::Theme>,
    ) -> bool {
        self.apply(ctx);
        live_os_theme_changed(self.theme, last_os_theme, ctx.system_theme())
    }

    /// Update the in-memory mode immediately. Disk persist is queued.
    pub fn set_theme(&mut self, theme: ThemeMode) {
        if self.theme == theme && !self.persist_pending {
            return;
        }
        self.theme = theme;
        self.persist_pending = true;
    }

    /// Take the latest queued write. The UI thread must `spawn_blocking` this.
    #[must_use]
    pub fn take_persist_job(&mut self) -> Option<PersistJob> {
        if !std::mem::take(&mut self.persist_pending) {
            return None;
        }
        self.persist_epoch = self.persist_epoch.saturating_add(1);
        self.latest_persist
            .store(self.persist_epoch, Ordering::Release);
        Some(PersistJob {
            path: self.path.clone(),
            contents: self.render(),
            epoch: self.persist_epoch,
            latest: Arc::clone(&self.latest_persist),
            write_lock: Arc::clone(&self.persist_lock),
        })
    }

    #[cfg(test)]
    fn persist_now(&mut self) -> Result<(), std::io::Error> {
        self.persist_pending = false;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.path, self.render())
    }

    fn render(&self) -> String {
        format!(
            "# thinwire appearance. Missing file means System.\ntheme = {}\n",
            self.theme.as_str()
        )
    }
}

/// True when preference is System and the polled OS theme changed.
#[must_use]
pub fn live_os_theme_changed(
    mode: ThemeMode,
    last: &mut Option<egui::Theme>,
    current: Option<egui::Theme>,
) -> bool {
    if mode != ThemeMode::System {
        return false;
    }
    let changed = last.zip(current).is_some_and(|(prev, now)| prev != now);
    if current.is_some() {
        *last = current;
    }
    changed
}

fn default_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("thinwire")
        .join("settings.toml")
}

fn read_theme(path: &std::path::Path) -> Option<ThemeMode> {
    let contents = fs::read_to_string(path).ok()?;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=')?;
        if key.trim() == "theme" {
            return ThemeMode::parse(value);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_settings_path() -> PathBuf {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join("thinwire-theme-tests")
            .join(format!("{}-{n}", std::process::id()))
            .join("settings.toml")
    }

    #[test]
    fn missing_file_is_system() {
        let path = temp_settings_path();
        assert!(!path.exists());
        let settings = Settings::load_from(path);
        assert_eq!(settings.theme(), ThemeMode::System);
    }

    #[test]
    fn corrupt_or_unknown_theme_is_system() {
        let path = temp_settings_path();
        fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        fs::write(&path, "theme = rainbow\n").expect("write");
        assert_eq!(Settings::load_from(path).theme(), ThemeMode::System);
    }

    #[test]
    fn persist_round_trip() {
        let path = temp_settings_path();
        let mut settings = Settings::load_from(path.clone());
        assert_eq!(settings.theme(), ThemeMode::System);
        settings.set_theme(ThemeMode::Dark);
        settings.persist_now().expect("save dark");
        assert_eq!(Settings::load_from(path.clone()).theme(), ThemeMode::Dark);
        settings.set_theme(ThemeMode::Light);
        settings.persist_now().expect("save light");
        assert_eq!(Settings::load_from(path.clone()).theme(), ThemeMode::Light);
        settings.set_theme(ThemeMode::System);
        settings.persist_now().expect("save system");
        assert_eq!(Settings::load_from(path).theme(), ThemeMode::System);
    }

    #[test]
    fn set_theme_is_memory_only_until_persist_job_runs() {
        let path = temp_settings_path();
        let mut settings = Settings::load_from(path.clone());
        settings.set_theme(ThemeMode::Dark);
        assert_eq!(settings.theme(), ThemeMode::Dark);
        assert!(!path.exists());
        settings.take_persist_job().expect("queued job").run();
        assert_eq!(Settings::load_from(path).theme(), ThemeMode::Dark);
    }

    #[test]
    fn stale_persist_job_does_not_overwrite_newer_theme() {
        let path = temp_settings_path();
        let mut settings = Settings::load_from(path.clone());
        settings.set_theme(ThemeMode::Dark);
        let stale = settings.take_persist_job().expect("dark job");
        settings.set_theme(ThemeMode::Light);
        let latest = settings.take_persist_job().expect("light job");
        stale.run();
        assert!(
            !path.exists() || Settings::load_from(path.clone()).theme() != ThemeMode::Dark,
            "stale Dark write must not win"
        );
        latest.run();
        assert_eq!(Settings::load_from(path).theme(), ThemeMode::Light);
    }

    #[test]
    fn overlapping_persist_jobs_that_pass_pre_io_check_commit_latest() {
        let path = temp_settings_path();
        let mut settings = Settings::load_from(path.clone());
        settings.set_theme(ThemeMode::Dark);
        let stale = settings.take_persist_job().expect("dark job");
        settings.set_theme(ThemeMode::Light);
        let latest = settings.take_persist_job().expect("light job");
        std::thread::scope(|scope| {
            scope.spawn(|| stale.run_after_naive_pre_io_check());
            scope.spawn(|| latest.run_after_naive_pre_io_check());
        });
        assert_eq!(Settings::load_from(path).theme(), ThemeMode::Light);
    }

    #[test]
    fn parse_accepts_quoted_and_mixed_case() {
        assert_eq!(ThemeMode::parse(" System "), Some(ThemeMode::System));
        assert_eq!(ThemeMode::parse("\"DARK\""), Some(ThemeMode::Dark));
        assert_eq!(ThemeMode::parse("light"), Some(ThemeMode::Light));
        assert_eq!(ThemeMode::parse("nope"), None);
    }

    #[test]
    fn egui_mapping_covers_all_modes() {
        for mode in [ThemeMode::System, ThemeMode::Light, ThemeMode::Dark] {
            assert_eq!(ThemeMode::from_egui(mode.to_egui()), mode);
        }
    }

    #[test]
    fn system_mode_follows_live_os_theme_changes() {
        let mut last = None;
        assert!(!live_os_theme_changed(
            ThemeMode::System,
            &mut last,
            Some(egui::Theme::Dark)
        ));
        assert_eq!(last, Some(egui::Theme::Dark));
        assert!(live_os_theme_changed(
            ThemeMode::System,
            &mut last,
            Some(egui::Theme::Light)
        ));
        assert_eq!(last, Some(egui::Theme::Light));
        assert!(!live_os_theme_changed(
            ThemeMode::System,
            &mut last,
            Some(egui::Theme::Light)
        ));
    }

    #[test]
    fn light_or_dark_override_ignores_os_changes() {
        let mut last = Some(egui::Theme::Dark);
        assert!(!live_os_theme_changed(
            ThemeMode::Light,
            &mut last,
            Some(egui::Theme::Light)
        ));
        assert_eq!(last, Some(egui::Theme::Dark));
    }
}
