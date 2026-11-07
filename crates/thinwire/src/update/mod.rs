//! macOS update check. The frame reads a [`Prompt`] and sends an [`Action`].

#[cfg(target_os = "macos")]
mod host;
mod machine;

use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};

use machine::Update;
#[cfg(target_os = "macos")]
use machine::{Effect, Event, step};

/// What the frame may draw. No URL, path, signature, or feed type is in here.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Prompt {
    Quiet,
    Offer(OfferView),
    Installing,
    /// `Installing` reached `Armed`. `logic` sends `ViewportCommand::Close`.
    QuitToInstall,
    Failed(PlainError),
}

/// Version line and notes for the modal. Built only by [`project`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OfferView {
    pub version: String,
    pub notes: String,
}

/// A sentence the modal may show. The UI does not format errors itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlainError {
    pub text: &'static str,
    pub retry: bool,
}

pub(super) const CHECK_FAILED: PlainError = PlainError {
    text: "Couldn't check for updates.",
    retry: true,
};

pub(super) const BAD_SIGNATURE: PlainError = PlainError {
    text: "This download isn't a signed Thinwire update.",
    retry: true,
};

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) const SWAP_FAILED: PlainError = PlainError {
    text: "Couldn't install the update. Thinwire is unchanged.",
    retry: true,
};

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(super) const MOVE_TO_APPLICATIONS: PlainError = PlainError {
    text: "Move Thinwire.app to the Applications folder and open it from there.",
    retry: false,
};

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
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
}

/// Handle the frame holds. The worker owns the [`Update`] machine.
///
/// `prompt` copies the last snapshot. `act` sends and returns. Neither waits
/// on the network, the disk, or `Core`.
pub struct Updates {
    prompt: Arc<Mutex<Prompt>>,
    tx: Option<mpsc::SyncSender<Action>>,
}

impl Updates {
    /// Spawn `thinwire-update` on macOS. On Linux and Windows the handle stays
    /// `Quiet` and no thread starts.
    #[must_use]
    pub fn start(wake: egui::Context) -> Self {
        #[cfg(target_os = "macos")]
        {
            return spawn(wake);
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = wake;
            Self {
                prompt: Arc::new(Mutex::new(Prompt::Quiet)),
                tx: None,
            }
        }
    }

    #[must_use]
    pub fn prompt(&self) -> Prompt {
        self.prompt
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or(Prompt::Quiet)
    }

    pub fn act(&self, action: Action) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(action);
        }
    }
}

/// Set when `main` sees `--apply-update`. Fields stay private so the frame
/// cannot build a job and skip the feed.
pub struct ApplyJob {
    staged: PathBuf,
    dest: PathBuf,
}

/// `None` unless argv is `--apply-update`, two absolute paths, and nothing else.
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

/// Wait until the parent dies, then [`machine::converge`], then open `dest`
/// when it exists. Does not return. Does not take `instance.lock`.
///
/// A terminal stdin exits before any rename.
pub fn apply_and_relaunch(job: ApplyJob) -> ! {
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

fn present(staged: &Path, fresh: &Path, dest: &Path, previous: &Path) -> [bool; 4] {
    [staged, fresh, dest, previous].map(real_dir)
}

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
        },
        Err(_) => {
            tracing::warn!(target: "thinwire::update", kind = "check_failed");
            Updates { prompt, tx: None }
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
    while let Ok(action) = rx.recv() {
        let (next, effect) = step(state, Event::Action(action));
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

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn publish(slot: &Mutex<Prompt>, state: &Update, wake: &egui::Context) {
    let next = project(state);
    match slot.lock() {
        Ok(mut guard) => *guard = next,
        Err(poisoned) => {
            *poisoned.into_inner() = Prompt::Quiet;
        }
    }
    wake.request_repaint();
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn project(state: &Update) -> Prompt {
    match state {
        Update::Dormant | Update::Checking { .. } | Update::Quiet { .. } => Prompt::Quiet,
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
