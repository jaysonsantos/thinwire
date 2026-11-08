#[cfg(target_os = "macos")]
mod host;
#[cfg(any(test, target_os = "macos"))]
mod machine;
#[cfg(any(test, not(target_os = "macos")))]
mod release;

use std::ffi::{OsStr, OsString};
#[cfg(target_os = "macos")]
use std::io::{self, IsTerminal, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
#[cfg(any(target_os = "macos", not(test)))]
use std::time::Instant;

#[cfg(any(test, target_os = "macos"))]
use machine::Update;
#[cfg(target_os = "macos")]
use machine::{Effect, Event, step};

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Prompt {
    Quiet,
    /// A check that the user asked for runs.
    Checking,
    /// A check that the user asked for found no newer build. The text is the
    /// build id of this app.
    UpToDate(String),
    /// macOS: a signed update that the app can install.
    Offer(OfferView),
    /// Linux and Windows: a newer build on the nightly release page.
    Available(OfferView),
    Installing,
    QuitToInstall,
    Failed(PlainError),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OfferView {
    version: String,
    notes: String,
}

impl OfferView {
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    #[must_use]
    pub fn notes(&self) -> &str {
        &self.notes
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlainError {
    pub text: &'static str,
    pub retry: bool,
}

pub(super) const CHECK_FAILED: PlainError = PlainError {
    text: "Couldn't check for updates.",
    retry: true,
};

#[cfg(any(test, target_os = "macos"))]
pub(super) const BAD_SIGNATURE: PlainError = PlainError {
    text: "This download isn't a signed Thinwire update.",
    retry: true,
};

pub(super) const NOT_A_CLIENT: PlainError = PlainError {
    text: "This build can't check for updates. Get Thinwire from the nightly release page.",
    retry: false,
};

/// The page that lists every nightly build.
pub const NIGHTLY_PAGE: &str = "https://github.com/jaysonsantos/thinwire/releases/tag/nightly";

#[cfg(target_os = "macos")]
pub(super) const SWAP_FAILED: PlainError = PlainError {
    text: "Couldn't install the update. Thinwire is unchanged.",
    retry: true,
};

#[cfg(target_os = "macos")]
pub(super) const MOVE_TO_APPLICATIONS: PlainError = PlainError {
    text: "Move Thinwire.app to the Applications folder and open it from there.",
    retry: false,
};

#[cfg(target_os = "macos")]
pub(super) const NOT_WRITABLE: PlainError = PlainError {
    text: "Thinwire couldn't replace the app in its folder. Move it somewhere you can write to, then try again.",
    retry: true,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Install,
    Later,
    Dismiss,
    Retry,
    /// The user asked for a check now: from the menu or from Settings.
    CheckNow,
}

pub struct Updates {
    prompt: Arc<Mutex<Prompt>>,
    tx: Option<mpsc::SyncSender<Action>>,
    /// The build id of this app, for Settings. `None` in a local build.
    build: Option<String>,
}

impl Updates {
    #[must_use]
    pub fn start(wake: egui::Context) -> Self {
        #[cfg(target_os = "macos")]
        {
            spawn(wake)
        }
        #[cfg(not(target_os = "macos"))]
        {
            spawn_release_check(wake, release_build_id())
        }
    }

    #[must_use]
    pub fn prompt(&self) -> Prompt {
        self.prompt
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or(Prompt::Quiet)
    }

    /// The build id of this app, for example `0.1.0.r298.g0be7e95`.
    #[must_use]
    pub fn build(&self) -> Option<&str> {
        self.build.as_deref()
    }

    pub fn act(&self, action: Action) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(action);
            return;
        }
        // No update thread: this build cannot check. A manual check says so.
        let next = match (self.prompt(), action) {
            (_, Action::CheckNow) => Prompt::Failed(NOT_A_CLIENT),
            (Prompt::Failed(_), Action::Dismiss) => Prompt::Quiet,
            (prompt, _) => prompt,
        };
        set_prompt(&self.prompt, next);
    }
}

fn set_prompt(slot: &Mutex<Prompt>, next: Prompt) {
    match slot.lock() {
        Ok(mut guard) => *guard = next,
        Err(poisoned) => *poisoned.into_inner() = next,
    }
}

/// The next time of an automatic check: 2 to 4 hours from now.
#[cfg(any(target_os = "macos", not(test)))]
fn next_recheck() -> Instant {
    Instant::now() + recheck_delay()
}

#[cfg(target_os = "macos")]
fn recheck_delay() -> std::time::Duration {
    machine::recheck_delay(machine::recheck_random())
}

#[cfg(all(not(target_os = "macos"), not(test)))]
fn recheck_delay() -> std::time::Duration {
    use std::hash::{BuildHasher, Hasher};
    const MIN: u64 = 2 * 60 * 60;
    const MAX: u64 = 4 * 60 * 60;
    let random = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    std::time::Duration::from_secs(MIN + random % (MAX - MIN + 1))
}

/// The `BUILD_ID` file next to the program of a Linux or Windows archive.
#[cfg(not(target_os = "macos"))]
fn release_build_id() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let text = std::fs::read_to_string(exe.parent()?.join("BUILD_ID")).ok()?;
    let build = text.trim().to_string();
    release::build_count(&build).map(|_| build)
}

/// Linux and Windows: start the check thread when this build has a build
/// id and the nightly release has an archive for this OS. Else no thread
/// runs, and a manual check says that this build cannot check.
#[cfg(not(target_os = "macos"))]
fn spawn_release_check(wake: egui::Context, build: Option<String>) -> Updates {
    let prompt = Arc::new(Mutex::new(Prompt::Quiet));
    let prefix = release::asset_prefix(std::env::consts::OS, std::env::consts::ARCH);
    let (Some(our), Some(prefix)) = (build.clone(), prefix) else {
        let _ = wake;
        return Updates {
            prompt,
            tx: None,
            build,
        };
    };
    #[cfg(test)]
    {
        let _ = (wake, our, prefix);
        Updates {
            prompt,
            tx: None,
            build,
        }
    }
    #[cfg(not(test))]
    {
        let (tx, rx) = mpsc::sync_channel(8);
        let published = Arc::clone(&prompt);
        match std::thread::Builder::new()
            .name("thinwire-update".to_string())
            .spawn(move || release_worker(&wake, &published, &rx, our, prefix))
        {
            Ok(_) => Updates {
                prompt,
                tx: Some(tx),
                build,
            },
            Err(_) => {
                tracing::warn!(target: "thinwire::update", kind = "check_failed");
                Updates {
                    prompt,
                    tx: None,
                    build,
                }
            }
        }
    }
}

#[cfg(all(not(target_os = "macos"), not(test)))]
fn release_worker(
    wake: &egui::Context,
    prompt: &Mutex<Prompt>,
    rx: &mpsc::Receiver<Action>,
    our: String,
    prefix: &'static str,
) {
    let mut state = release::start(Some(our));
    let mut event = release::Event::Recheck;
    let mut due = next_recheck();
    loop {
        let (next, effect) = release::step(state, event);
        state = next;
        set_prompt(prompt, release::project(&state));
        wake.request_repaint();
        if effect == release::Effect::Fetch {
            event = release::Event::Release(fetch_newest(prefix));
            continue;
        }
        event = match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
            Ok(action) => {
                if action == Action::CheckNow {
                    due = next_recheck();
                }
                release::Event::Action(action)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                due = next_recheck();
                release::Event::Recheck
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
    }
}

#[cfg(all(not(target_os = "macos"), not(test)))]
fn fetch_newest(prefix: &str) -> Result<Option<String>, PlainError> {
    use std::io::Read;
    const RELEASE_API_URL: &str =
        "https://api.github.com/repos/jaysonsantos/thinwire/releases/tags/nightly";
    let agent = ureq::AgentBuilder::new()
        .https_only(true)
        .redirects(0)
        .try_proxy_from_env(false)
        .timeout_connect(std::time::Duration::from_secs(20))
        .timeout(std::time::Duration::from_secs(30))
        .build();
    let response = agent
        .get(RELEASE_API_URL)
        .set("accept", "application/vnd.github+json")
        .set(
            "user-agent",
            concat!("thinwire/", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .map_err(|_| CHECK_FAILED)?;
    if response.status() != 200 {
        return Err(CHECK_FAILED);
    }
    let mut body = Vec::new();
    response
        .into_reader()
        .take(release::MAX_RELEASE_JSON as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|_| CHECK_FAILED)?;
    let result = release::newest_build(&body, prefix);
    if result.is_err() {
        tracing::warn!(target: "thinwire::update", kind = "check_failed");
    }
    result
}

pub struct ApplyJob {
    staged: PathBuf,
    dest: PathBuf,
}

#[must_use]
pub fn apply_from_args() -> Option<ApplyJob> {
    apply_from(std::env::args_os())
}

fn apply_from(mut args: impl Iterator<Item = OsString>) -> Option<ApplyJob> {
    let _argv0 = args.next()?;
    if args.next().as_deref() != Some(OsStr::new("--apply-update")) {
        return None;
    }
    let staged = PathBuf::from(args.next()?);
    let dest = PathBuf::from(args.next()?);
    if args.next().is_some() {
        return None;
    }
    if !is_app_path(&staged) || !is_app_path(&dest) {
        return None;
    }
    Some(ApplyJob { staged, dest })
}

fn is_app_path(path: &Path) -> bool {
    path.is_absolute()
        && path.file_name() == Some(OsStr::new("Thinwire.app"))
        && !path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
}

pub fn apply_and_relaunch(job: ApplyJob) -> ! {
    #[cfg(not(target_os = "macos"))]
    {
        let ApplyJob { staged, dest } = job;
        let _kept = (staged, dest);
        std::process::exit(1);
    }
    #[cfg(target_os = "macos")]
    {
        if io::stdin().is_terminal() {
            std::process::exit(1);
        }
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 256];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => std::process::exit(1),
            }
        }
        #[cfg(target_os = "macos")]
        if host::require_developer_id(&job.staged).is_err() {
            open_bundle(&job.dest);
            std::process::exit(1);
        }
        #[cfg(target_os = "macos")]
        clear_quarantine(&job.staged);

        let Some(parent) = job.dest.parent().map(Path::to_path_buf) else {
            std::process::exit(1);
        };
        let fresh = parent.join("Thinwire.app.new");
        let previous = parent.join("Thinwire.app.previous");
        settle(&job.staged, &fresh, &job.dest, &previous);
        if real_dir(&job.dest) {
            #[cfg(target_os = "macos")]
            open_bundle(&job.dest);
            std::process::exit(0);
        }
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
fn settle(staged: &Path, fresh: &Path, dest: &Path, previous: &Path) {
    for _ in 0..4 {
        let before = present(staged, fresh, dest, previous);
        match machine::converge(staged, fresh, dest, previous) {
            Ok(_) => {
                if present(staged, fresh, dest, previous) == before {
                    break;
                }
            }
            Err(_) => {
                tracing::warn!(target: "thinwire::update", kind = "swap_failed");
                break;
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn present(staged: &Path, fresh: &Path, dest: &Path, previous: &Path) -> [bool; 4] {
    [staged, fresh, dest, previous].map(real_dir)
}

#[cfg(target_os = "macos")]
fn real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| meta.is_dir())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn open_bundle(dest: &Path) {
    let opened = std::process::Command::new("/usr/bin/open")
        .arg(dest)
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !opened {
        tracing::warn!(target: "thinwire::update", kind = "swap_failed");
    }
}

#[cfg(target_os = "macos")]
fn clear_quarantine(bundle: &Path) {
    let _ = std::process::Command::new("/usr/bin/xattr")
        .args(["-dr", "com.apple.quarantine"])
        .arg(bundle)
        .status();
}

#[cfg(target_os = "macos")]
fn spawn(wake: egui::Context) -> Updates {
    let build = host::display_build();
    let prompt = Arc::new(Mutex::new(Prompt::Quiet));
    let (tx, rx) = mpsc::sync_channel(8);
    let published = Arc::clone(&prompt);
    match std::thread::Builder::new()
        .name("thinwire-update".to_string())
        .spawn(move || worker(wake, published, rx))
    {
        Ok(_) => Updates {
            prompt,
            tx: Some(tx),
            build,
        },
        Err(_) => {
            tracing::warn!(target: "thinwire::update", kind = "check_failed");
            Updates {
                prompt,
                tx: None,
                build,
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn worker(wake: egui::Context, prompt: Arc<Mutex<Prompt>>, rx: mpsc::Receiver<Action>) {
    let mut dest = None;
    let mut state = match host::gate() {
        host::Gate::NotClient => Update::Dormant,
        host::Gate::Translocated => Update::Failed {
            our: None,
            error: MOVE_TO_APPLICATIONS,
        },
        host::Gate::Client { our, dest: bundle } => {
            dest = Some(bundle);
            Update::Checking { our }
        }
    };
    publish(&prompt, &state, &wake);
    if let Update::Checking { our } = &state {
        let our = our.clone();
        let event = Event::Feed(host::fetch_feed(&our));
        let (next, effect) = step(state, event);
        state = run_effects(next, effect, dest.as_deref(), &prompt, &wake);
    }
    let mut due = next_recheck();
    loop {
        let event = match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
            Ok(action) => {
                // A manual check starts the 2 to 4 hours again.
                if action == Action::CheckNow {
                    due = next_recheck();
                }
                Event::Action(action)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                due = next_recheck();
                Event::Recheck
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        let before = state.clone();
        let (next, effect) = step(state, event);
        if next == before && matches!(effect, Effect::None) {
            state = next;
            continue;
        }
        state = run_effects(next, effect, dest.as_deref(), &prompt, &wake);
    }
}

#[cfg(target_os = "macos")]
fn run_effects(
    mut state: Update,
    mut effect: Effect,
    dest: Option<&Path>,
    prompt: &Mutex<Prompt>,
    wake: &egui::Context,
) -> Update {
    loop {
        publish(prompt, &state, wake);
        let event = match effect {
            Effect::None => return state,
            Effect::FetchFeed { our, purpose: _ } => Event::Feed(host::fetch_feed(&our)),
            Effect::Download { offer } => Event::Downloaded(host::download(&offer)),
            Effect::VerifyAndArm { offer: _, archive } => match dest {
                Some(dest) => Event::Armed(host::verify_and_arm(&archive, dest)),
                None => Event::Armed(Err(SWAP_FAILED)),
            },
        };
        let (next, next_effect) = step(state, event);
        state = next;
        effect = next_effect;
    }
}

#[cfg(target_os = "macos")]
fn publish(slot: &Mutex<Prompt>, state: &Update, wake: &egui::Context) {
    set_prompt(slot, project(state));
    wake.request_repaint();
}

#[cfg(any(test, target_os = "macos"))]
fn project(state: &Update) -> Prompt {
    match state {
        Update::Quiet {
            our,
            kind: machine::QuietKind::Confirmed,
        } => Prompt::UpToDate(our.display()),
        Update::Dormant | Update::Checking { .. } | Update::Quiet { .. } => Prompt::Quiet,
        Update::CheckingNow { .. } => Prompt::Checking,
        Update::Offer { offer, .. } => Prompt::Offer(offer.view()),
        Update::Installing {
            phase: machine::InstallPhase::Armed,
            ..
        } => Prompt::QuitToInstall,
        Update::Installing { .. } => Prompt::Installing,
        Update::Failed { error, .. } => Prompt::Failed(*error),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::{Action, Prompt, Updates, apply_from};

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn start_does_not_spawn_a_thread() {
        let before = named_threads("thinwire-update");
        let updates = Updates::start(egui::Context::default());
        assert_eq!(updates.prompt(), Prompt::Quiet);
        updates.act(Action::Install);
        updates.act(Action::Retry);
        assert_eq!(updates.prompt(), Prompt::Quiet);
        assert_eq!(named_threads("thinwire-update"), before);
    }

    /// A build with no `BUILD_ID` (a local or test build) cannot compare.
    /// A manual check says so, and Dismiss closes the message. On macOS the
    /// update thread answers, so this test is for Linux and Windows.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_manual_check_of_a_local_build_says_that_it_cannot_check() {
        let updates = Updates::start(egui::Context::default());
        assert_eq!(updates.build(), None);
        updates.act(Action::CheckNow);
        assert_eq!(updates.prompt(), Prompt::Failed(super::NOT_A_CLIENT));
        updates.act(Action::Dismiss);
        assert_eq!(updates.prompt(), Prompt::Quiet);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_linux_or_windows_build_with_a_build_id_reports_it() {
        let updates = super::spawn_release_check(
            egui::Context::default(),
            Some("0.1.0.r298.g0be7e95".to_string()),
        );
        assert_eq!(updates.build(), Some("0.1.0.r298.g0be7e95"));
    }

    #[cfg(not(target_os = "macos"))]
    fn named_threads(name: &str) -> usize {
        let mut count = 0;
        let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
            return 0;
        };
        for entry in entries.flatten() {
            let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else {
                continue;
            };
            if comm.trim() == name {
                count += 1;
            }
        }
        count
    }

    #[test]
    fn apply_args_need_the_flag_and_two_app_paths() {
        let staged = OsString::from("/tmp/stage/Thinwire.app");
        let dest = OsString::from("/Applications/Thinwire.app");
        let job = apply_from(
            [
                OsString::from("thinwire"),
                OsString::from("--apply-update"),
                staged.clone(),
                dest.clone(),
            ]
            .into_iter(),
        )
        .expect("job");
        assert_eq!(job.staged, PathBuf::from(&staged));
        assert_eq!(job.dest, PathBuf::from(&dest));
        assert!(
            apply_from(
                [
                    OsString::from("thinwire"),
                    OsString::from("--apply-update"),
                    OsString::from("Thinwire.app"),
                    dest.clone(),
                ]
                .into_iter(),
            )
            .is_none()
        );
        assert!(
            apply_from(
                [
                    OsString::from("thinwire"),
                    OsString::from("--apply-update"),
                    OsString::from("/tmp/../Thinwire.app"),
                    dest,
                ]
                .into_iter(),
            )
            .is_none()
        );
        assert!(
            apply_from(
                [
                    OsString::from("thinwire"),
                    OsString::from("--apply-update"),
                    staged.clone(),
                    OsString::from("/Applications/Other.app"),
                ]
                .into_iter(),
            )
            .is_none()
        );
        assert!(apply_from([OsString::from("thinwire")].into_iter()).is_none());
    }
}
