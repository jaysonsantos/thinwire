//! Finds a helper program and starts it as a child process.
//!
//! The helper is its own program (ADR 0013). The app never links it. The
//! only connection is the child's stdin, stdout, and stderr.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::oneshot;

/// The pipe ends of a helper are boxed, so a test can give in-memory pipes.
pub type HelperInput = Box<dyn AsyncWrite + Send + Unpin>;
pub type HelperOutput = Box<dyn AsyncRead + Send + Unpin>;

/// A helper that runs.
pub struct HelperProcess {
    /// The helper reads wire lines from it. Dropping it closes the pipe, and
    /// the helper then shuts down.
    pub stdin: HelperInput,
    /// The helper writes wire lines to it.
    pub stdout: HelperOutput,
    /// The helper writes its logs to it.
    pub stderr: Option<HelperOutput>,
    /// Resolves when the helper ended.
    pub exited: oneshot::Receiver<()>,
    /// Send a value, or drop it, to kill the helper.
    pub kill: oneshot::Sender<()>,
    /// The OS id of the process, if it has one.
    pub pid: Option<u32>,
}

/// Starts the helper of one protocol.
pub trait HelperLauncher: Send + Sync + 'static {
    /// The helper program exists. A file check only: it starts nothing.
    fn installed(&self) -> bool;

    /// Start the helper. Call it on a tokio runtime.
    fn launch(&self) -> std::io::Result<HelperProcess>;
}

/// Starts a helper program from disk.
///
/// It looks next to the app's own binary first (`Contents/MacOS/` in
/// `Thinwire.app`), then at the path from the settings (ADR 0013).
#[derive(Debug, Clone)]
pub struct ProcessLauncher {
    program: &'static str,
    configured: Option<PathBuf>,
    /// Extra arguments. The app passes none. Tests name a session folder.
    args: Vec<String>,
}

impl ProcessLauncher {
    /// `program` is the file name of the helper without `.exe`.
    /// `configured` is the path from the settings, if the user set one.
    #[must_use]
    pub const fn new(program: &'static str, configured: Option<PathBuf>) -> Self {
        Self {
            program,
            configured,
            args: Vec::new(),
        }
    }

    /// Start the helper with these arguments.
    #[must_use]
    pub fn with_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// The helper program on disk, if one exists.
    #[must_use]
    pub fn path(&self) -> Option<PathBuf> {
        let beside = std::env::current_exe()
            .ok()
            .and_then(|exe| beside(&exe, self.program));
        first_file(beside.into_iter().chain(self.configured.clone()))
    }
}

/// The path of `program` in the folder of the app binary `exe`.
fn beside(exe: &Path, program: &str) -> Option<PathBuf> {
    let name = format!("{program}{}", std::env::consts::EXE_SUFFIX);
    Some(exe.parent()?.join(name))
}

fn first_file(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|path| path.is_file())
}

/// Windows process creation flag: no console window for the helper.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

impl HelperLauncher for ProcessLauncher {
    fn installed(&self) -> bool {
        self.path().is_some()
    }

    fn launch(&self) -> std::io::Result<HelperProcess> {
        let path = self
            .path()
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
        let mut command = tokio::process::Command::new(path);
        command
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // The helper never outlives the app's handle to it.
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);
        let mut child = command.spawn()?;
        let pid = child.id();
        let missing = || std::io::Error::from(std::io::ErrorKind::BrokenPipe);
        let stdin = child.stdin.take().ok_or_else(missing)?;
        let stdout = child.stdout.take().ok_or_else(missing)?;
        let stderr = child.stderr.take();
        let (exited_tx, exited) = oneshot::channel();
        let (kill, kill_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            tokio::select! {
                _ = child.wait() => {}
                // A value or a dropped sender: both mean kill.
                _ = kill_rx => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                }
            }
            let _ = exited_tx.send(());
        });
        Ok(HelperProcess {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            stderr: stderr.map(|stderr| Box::new(stderr) as HelperOutput),
            exited,
            kill,
            pid,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "thinwire-helper-launch-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn the_helper_is_found_next_to_the_app_binary() {
        let exe = Path::new("/opt/thinwire/thinwire");
        let found = beside(exe, "thinwire-whatsapp-helper").expect("a parent folder");
        let name = format!("thinwire-whatsapp-helper{}", std::env::consts::EXE_SUFFIX);
        assert_eq!(found, Path::new("/opt/thinwire").join(name));
        // macOS: the helper sits in Contents/MacOS, next to the app binary.
        let bundle = Path::new("/Applications/Thinwire.app/Contents/MacOS/thinwire");
        assert!(
            beside(bundle, "thinwire-whatsapp-helper")
                .expect("a parent folder")
                .starts_with("/Applications/Thinwire.app/Contents/MacOS")
        );
    }

    /// ADR 0013: the folder of the app binary comes first, then the path
    /// from the settings. A path that is not a file is skipped.
    #[test]
    fn the_first_candidate_that_is_a_file_wins() {
        let dir = temp_dir("order");
        let bundled = dir.join("bundled-helper");
        let configured = dir.join("configured-helper");
        std::fs::write(&configured, b"").expect("configured file");
        assert_eq!(
            first_file([bundled.clone(), configured.clone()]),
            Some(configured.clone()),
            "the bundled helper is missing"
        );
        std::fs::write(&bundled, b"").expect("bundled file");
        assert_eq!(
            first_file([bundled.clone(), configured]),
            Some(bundled),
            "the bundled helper wins"
        );
        assert_eq!(first_file([dir.clone()]), None, "a folder is not a helper");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_helper_is_not_installed_and_does_not_launch() {
        let launcher = ProcessLauncher::new("thinwire-no-such-helper", None);
        assert!(!launcher.installed());
        assert_eq!(
            launcher.launch().err().map(|error| error.kind()),
            Some(std::io::ErrorKind::NotFound)
        );
    }
}
