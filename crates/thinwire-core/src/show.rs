//! A second launch asks the running instance to show its window.
//!
//! The running instance listens next to [`instance.lock`](super::instance::LOCK_FILE_NAME),
//! on its own thread. A launch that finds the lock held sends one fixed message,
//! [`SHOW_MESSAGE`]. No other bytes are a request. The reply is [`SHOWN_REPLY`]
//! or [`CLOSING_REPLY`].
//!
//! On Linux and macOS the endpoint is a Unix socket, mode `0600`, in the same
//! folder as the lock. On Windows it is a named pipe. Windows has no pipe
//! files in that folder, so the pipe name is derived from the lock path and
//! the pipe allows only its owner. A remote client is rejected.
//!
//! A stale socket file is removed only when this process holds the lock, the
//! file is a socket, and nothing answers a connect. A live listener is left
//! in place. A file that is not a socket is left in place.
//!
//! The listener thread does a bounded read and write for each client, so one
//! silent client cannot hold it.

use std::io::{self, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// Socket file name, next to `instance.lock`.
pub const SHOW_FILE_NAME: &str = "instance.show";

/// The only request. Anything else is ignored.
pub const SHOW_MESSAGE: &[u8] = b"show\n";
/// The running instance will bring its window forward.
pub const SHOWN_REPLY: &[u8] = b"shown\n";
/// The running instance is closing and will not show its window.
pub const CLOSING_REPLY: &[u8] = b"closing\n";

/// How long one exchange may take. A silent peer makes the caller stop.
const EXCHANGE_LIMIT: Duration = Duration::from_millis(200);
/// Bytes kept from one client. The fixed messages are shorter than this.
const MESSAGE_LIMIT: usize = 32;

/// What the running instance answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowReply {
    /// The window will come forward.
    Shown,
    /// The window is already hidden for shutdown.
    Closing,
    /// Nothing accepted the message.
    Unanswered,
}

/// What the listener does with one message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    Shown,
    Closing,
    Ignore,
}

fn answer_for(message: &[u8], closing: bool) -> Answer {
    if message != SHOW_MESSAGE {
        Answer::Ignore
    } else if closing {
        Answer::Closing
    } else {
        Answer::Shown
    }
}

fn answer_bytes(answer: Answer) -> Option<&'static [u8]> {
    match answer {
        Answer::Shown => Some(SHOWN_REPLY),
        Answer::Closing => Some(CLOSING_REPLY),
        Answer::Ignore => None,
    }
}

fn reply_from(message: &[u8]) -> ShowReply {
    match message {
        SHOWN_REPLY => ShowReply::Shown,
        CLOSING_REPLY => ShowReply::Closing,
        _ => ShowReply::Unanswered,
    }
}

/// `<lock dir>/instance.show`.
#[must_use]
pub fn socket_path(lock_path: &Path) -> PathBuf {
    lock_path.with_file_name(SHOW_FILE_NAME)
}

/// Pipe stem for `lock_path`. The same path always gives the same stem.
/// The stem has no separators, so it is one pipe name.
#[cfg(any(windows, test))]
fn pipe_stem(lock_path: &Path) -> String {
    // FNV-1a, 64-bit. Stable across processes. `DefaultHasher` is not.
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in lock_path.as_os_str().as_encoded_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("thinwire-instance-{hash:016x}")
}

/// Shared listener state. The UI thread and the listener thread both use it.
struct State {
    closing: AtomicBool,
    pending: AtomicBool,
    /// Wakes the UI. Empty until the window exists.
    wake: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl State {
    fn new() -> Self {
        Self {
            closing: AtomicBool::new(false),
            pending: AtomicBool::new(false),
            wake: Mutex::new(None),
        }
    }

    fn mark_closing(&self) {
        self.closing.store(true, Ordering::Release);
    }

    fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    /// Record a show and wake the UI if it already registered.
    fn signal(&self) {
        self.pending.store(true, Ordering::Release);
        let wake = self.wake.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(wake) = wake.as_ref() {
            wake();
        }
    }

    /// Register the UI wake. A show that arrived earlier is delivered now.
    fn set_wake(&self, wake: impl Fn() + Send + 'static) {
        let mut slot = self.wake.lock().unwrap_or_else(PoisonError::into_inner);
        *slot = Some(Box::new(wake));
        if self.pending.load(Ordering::Acquire)
            && let Some(wake) = slot.as_ref()
        {
            wake();
        }
    }

    fn take_show(&self) -> bool {
        self.pending.swap(false, Ordering::AcqRel)
    }
}

impl std::fmt::Debug for State {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("State")
            .field("closing", &self.is_closing())
            .field("pending", &self.pending.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// The listener for this process. The accept loop runs on its own thread.
#[derive(Debug)]
pub struct ShowListener {
    state: Arc<State>,
}

impl ShowListener {
    /// Bind the endpoint and start the accept loop.
    ///
    /// The socket is listening before this returns. Call it only while this
    /// process holds the instance lock: that is what makes a leftover socket
    /// stale.
    ///
    /// # Errors
    ///
    /// The endpoint is already in use, or it could not be created.
    pub fn spawn(lock_path: &Path) -> io::Result<Self> {
        let listener = transport::bind(lock_path)?;
        let state = Arc::new(State::new());
        let thread_state = Arc::clone(&state);
        let spawned = thread::Builder::new()
            .name("thinwire-instance-show".into())
            .spawn(move || transport::serve(listener, thread_state));
        if let Err(error) = spawned {
            tracing::warn!(%error, "the show listener thread did not start");
            let _ = transport::release_after_failed_start(lock_path);
            return Err(error);
        }
        Ok(Self { state })
    }

    /// The window is hidden for shutdown. Later requests get [`CLOSING_REPLY`].
    pub fn mark_closing(&self) {
        self.state.mark_closing();
    }

    /// Wake the UI thread when a show arrives. The callback must not block
    /// and must not call back into this listener.
    pub fn set_wake(&self, wake: impl Fn() + Send + 'static) {
        self.state.set_wake(wake);
    }

    /// `true` once for each accepted show since the last call.
    #[must_use]
    pub fn take_show(&self) -> bool {
        self.state.take_show()
    }
}

/// Send [`SHOW_MESSAGE`] to the instance that holds `lock_path`.
///
/// A connect failure, a timeout, a closed connection, or any other reply is
/// [`ShowReply::Unanswered`]. The caller then waits for the lock.
#[must_use]
pub fn ask(lock_path: &Path) -> ShowReply {
    ask_within(lock_path, EXCHANGE_LIMIT)
}

fn ask_within(lock_path: &Path, limit: Duration) -> ShowReply {
    match transport::connect(lock_path, limit) {
        Ok(mut stream) => client_exchange(&mut stream, limit),
        Err(error) => {
            tracing::debug!(%error, "show request got no connection");
            ShowReply::Unanswered
        }
    }
}

fn client_exchange(stream: &mut (impl Read + Write), limit: Duration) -> ShowReply {
    let deadline = Instant::now() + limit;
    if write_all_bounded(stream, SHOW_MESSAGE, deadline).is_err() {
        return ShowReply::Unanswered;
    }
    reply_from(&read_message(stream, deadline))
}

fn server_exchange(stream: &mut (impl Read + Write), state: &State) {
    let deadline = Instant::now() + EXCHANGE_LIMIT;
    let message = read_message(stream, deadline);
    // Read the flag immediately before the reply. A close during the read
    // gets `closing` and does not wake the UI. A `shown` reply always wakes.
    let answer = answer_for(&message, state.is_closing());
    let Some(bytes) = answer_bytes(answer) else {
        return;
    };
    if write_all_bounded(stream, bytes, deadline).is_err() {
        return;
    }
    if answer == Answer::Shown {
        state.signal();
    }
}

fn read_message(stream: &mut impl Read, deadline: Instant) -> Vec<u8> {
    let mut buf = [0u8; MESSAGE_LIMIT];
    let mut len = 0;
    while len < buf.len() && Instant::now() < deadline {
        match stream.read(&mut buf[len..]) {
            Ok(0) => break,
            Ok(read) => {
                len += read;
                if buf[..len].contains(&b'\n') {
                    break;
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error)
                if error.kind() == ErrorKind::WouldBlock || error.kind() == ErrorKind::TimedOut =>
            {
                if !pause_until(deadline) {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    buf[..len].to_vec()
}

fn write_all_bounded(stream: &mut impl Write, bytes: &[u8], deadline: Instant) -> io::Result<()> {
    let mut sent = 0;
    while sent < bytes.len() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(ErrorKind::TimedOut, "show exchange"));
        }
        match stream.write(&bytes[sent..]) {
            Ok(0) => return Err(io::Error::new(ErrorKind::WriteZero, "show exchange")),
            Ok(wrote) => sent += wrote,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error)
                if error.kind() == ErrorKind::WouldBlock || error.kind() == ErrorKind::TimedOut =>
            {
                if !pause_until(deadline) {
                    return Err(io::Error::new(ErrorKind::TimedOut, "show exchange"));
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Sleep a short step, and stop when `deadline` has passed.
fn pause_until(deadline: Instant) -> bool {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return false;
    }
    thread::sleep(Duration::from_millis(10).min(left));
    true
}

#[cfg(unix)]
mod transport {
    use std::fs;
    use std::io::{self, ErrorKind};
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use super::{State, server_exchange, socket_path};

    pub(super) fn connect(lock_path: &Path, _limit: Duration) -> io::Result<UnixStream> {
        let stream = UnixStream::connect(socket_path(lock_path))?;
        stream.set_nonblocking(true)?;
        Ok(stream)
    }

    pub(super) fn bind(lock_path: &Path) -> io::Result<UnixListener> {
        let path = socket_path(lock_path);
        clear_stale_socket(&path)?;
        let listener = UnixListener::bind(&path)?;
        if let Err(error) = fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
            drop(listener);
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        Ok(listener)
    }

    pub(super) fn serve(listener: UnixListener, state: Arc<State>) {
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_ok() {
                        let mut stream = stream;
                        server_exchange(&mut stream, &state);
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, "show listener did not accept");
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }

    pub(super) fn release_after_failed_start(lock_path: &Path) -> io::Result<()> {
        clear_stale_socket(&socket_path(lock_path))
    }

    /// Remove a socket that nobody answers on. A live listener, a symlink,
    /// and any other file stay.
    fn clear_stale_socket(path: &Path) -> io::Result<()> {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if !meta.file_type().is_socket() {
            return Err(io::Error::new(
                ErrorKind::AlreadyExists,
                "instance.show exists and is not a socket",
            ));
        }
        match UnixStream::connect(path) {
            Ok(stream) => {
                drop(stream);
                Err(io::Error::new(
                    ErrorKind::AddrInUse,
                    "the show socket is already listening",
                ))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(_) => fs::remove_file(path),
        }
    }
}

#[cfg(windows)]
mod transport {
    use std::io;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use interprocess::ConnectWaitMode;
    use interprocess::local_socket::prelude::*;
    use interprocess::local_socket::{
        ConnectOptions, GenericNamespaced, Listener, ListenerOptions,
    };
    use interprocess::os::windows::local_socket::ListenerOptionsExt;
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;
    use widestring::u16cstr;

    use super::{State, pipe_stem, server_exchange};

    pub(super) fn connect(
        lock_path: &Path,
        limit: Duration,
    ) -> io::Result<interprocess::local_socket::Stream> {
        let stem = pipe_stem(lock_path);
        let name = stem.to_ns_name::<GenericNamespaced>()?;
        let stream = ConnectOptions::new()
            .name(name)
            .wait_mode(ConnectWaitMode::Timeout(limit))
            .connect_sync()?;
        stream.set_nonblocking(true)?;
        Ok(stream)
    }

    pub(super) fn bind(lock_path: &Path) -> io::Result<Listener> {
        let stem = pipe_stem(lock_path);
        let name = stem.to_ns_name::<GenericNamespaced>()?;
        // `OW` is the Owner Rights SID. The allow entry matches the owner
        // of the new pipe, which is this user, and nobody else. `P` keeps
        // the pipe from inheriting a wider list. Interprocess also rejects
        // remote clients.
        let descriptor = SecurityDescriptor::deserialize(u16cstr!("D:P(A;;GA;;;OW)"))?;
        ListenerOptions::new()
            .name(name)
            .security_descriptor(descriptor)
            .create_sync()
    }

    pub(super) fn serve(listener: Listener, state: Arc<State>) {
        loop {
            match listener.accept() {
                Ok(stream) => {
                    if stream.set_nonblocking(true).is_ok() {
                        let mut stream = stream;
                        server_exchange(&mut stream, &state);
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, "show listener did not accept");
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }

    pub(super) fn release_after_failed_start(_lock_path: &Path) -> io::Result<()> {
        // The pipe disappears with the listener handle. There is no file.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::FileTypeExt;
    #[cfg(unix)]
    use std::sync::Arc;
    #[cfg(unix)]
    use std::sync::atomic::AtomicBool;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    use super::super::instance::InstanceLock;
    use super::*;

    #[cfg(unix)]
    fn scratch(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        std::env::temp_dir()
            .join(format!(
                "thinwire-show-{name}-{}-{nanos}",
                std::process::id()
            ))
            .join("thinwire")
            .join("instance.lock")
    }

    #[cfg(unix)]
    fn cleanup(path: &Path) {
        if let Some(root) = path.parent().and_then(Path::parent) {
            let _ = fs::remove_dir_all(root);
        }
    }

    #[test]
    fn only_the_show_message_is_a_request() {
        assert_eq!(answer_for(SHOW_MESSAGE, false), Answer::Shown);
        assert_eq!(answer_for(SHOW_MESSAGE, true), Answer::Closing);
        assert_eq!(answer_for(b"show", false), Answer::Ignore);
        assert_eq!(answer_for(b"open\n", false), Answer::Ignore);
        assert_eq!(answer_for(b"show\nextra", false), Answer::Ignore);
        assert_eq!(answer_bytes(Answer::Shown), Some(SHOWN_REPLY));
        assert_eq!(answer_bytes(Answer::Closing), Some(CLOSING_REPLY));
        assert_eq!(answer_bytes(Answer::Ignore), None);
        assert_eq!(reply_from(SHOWN_REPLY), ShowReply::Shown);
        assert_eq!(reply_from(CLOSING_REPLY), ShowReply::Closing);
        assert_eq!(reply_from(b""), ShowReply::Unanswered);
        assert_eq!(reply_from(b"no\n"), ShowReply::Unanswered);
    }

    #[test]
    fn the_pipe_name_depends_only_on_the_lock_path() {
        let one = Path::new("/home/a/thinwire/instance.lock");
        let two = Path::new("/home/b/thinwire/instance.lock");
        assert_eq!(pipe_stem(one), pipe_stem(one));
        assert_ne!(pipe_stem(one), pipe_stem(two));
        let stem = pipe_stem(one);
        let hash = stem.strip_prefix("thinwire-instance-").expect("prefix");
        assert_eq!(hash.len(), 16, "{stem}");
        assert!(hash.chars().all(|ch| ch.is_ascii_hexdigit()), "{stem}");
        assert!(!stem.contains("home"));
    }

    #[test]
    fn the_socket_sits_next_to_the_lock() {
        let lock = Path::new("/tmp/thinwire/instance.lock");
        assert_eq!(socket_path(lock), Path::new("/tmp/thinwire/instance.show"));
    }

    #[cfg(unix)]
    #[test]
    fn a_show_message_is_accepted_and_wakes_the_ui() {
        let path = scratch("accepted");
        let _lock = InstanceLock::try_acquire(&path).expect("lock");
        let listener = ShowListener::spawn(&path).expect("listen");
        let woke = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&woke);
        listener.set_wake(move || flag.store(true, Ordering::SeqCst));

        assert_eq!(ask(&path), ShowReply::Shown);
        assert!(listener.take_show(), "the UI can see the request");
        assert!(woke.load(Ordering::SeqCst), "the UI thread is woken");

        assert_eq!(ask(&path), ShowReply::Shown, "the listener stays up");
        assert!(listener.take_show());
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn a_show_that_arrives_before_the_window_still_wakes_it() {
        let path = scratch("early");
        let _lock = InstanceLock::try_acquire(&path).expect("lock");
        let listener = ShowListener::spawn(&path).expect("listen");
        assert_eq!(ask(&path), ShowReply::Shown);
        let woke = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&woke);
        listener.set_wake(move || flag.store(true, Ordering::SeqCst));
        assert!(woke.load(Ordering::SeqCst));
        assert!(listener.take_show());
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn a_closing_instance_replies_closing_and_does_not_show() {
        let path = scratch("closing");
        let _lock = InstanceLock::try_acquire(&path).expect("lock");
        let listener = ShowListener::spawn(&path).expect("listen");
        listener.mark_closing();
        let woke = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&woke);
        listener.set_wake(move || flag.store(true, Ordering::SeqCst));

        assert_eq!(ask(&path), ShowReply::Closing);
        assert!(!listener.take_show());
        assert!(!woke.load(Ordering::SeqCst));
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn a_message_that_is_not_show_is_ignored() {
        use std::os::unix::net::UnixStream;

        let path = scratch("ignore");
        let _lock = InstanceLock::try_acquire(&path).expect("lock");
        let listener = ShowListener::spawn(&path).expect("listen");
        let mut client = UnixStream::connect(socket_path(&path)).expect("connect");
        client.set_nonblocking(true).expect("nonblocking");
        write_all_bounded(
            &mut client,
            b"open chat\n",
            Instant::now() + Duration::from_secs(1),
        )
        .expect("write");
        let reply = read_message(&mut client, Instant::now() + Duration::from_millis(500));
        assert_ne!(reply.as_slice(), SHOWN_REPLY);
        assert!(!listener.take_show());
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn a_connection_that_closes_without_a_reply_is_unanswered() {
        use std::os::unix::net::UnixListener;

        let path = scratch("drop");
        let _lock = InstanceLock::try_acquire(&path).expect("lock");
        let socket = socket_path(&path);
        let listener = UnixListener::bind(&socket).expect("bind");
        let accepted = thread::spawn(move || {
            let _client = listener.accept();
        });
        assert_eq!(ask(&path), ShowReply::Unanswered);
        accepted.join().expect("accept thread");
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn a_silent_peer_does_not_hold_the_client_past_the_limit() {
        use std::os::unix::net::UnixListener;

        let path = scratch("silent");
        let _lock = InstanceLock::try_acquire(&path).expect("lock");
        let socket = socket_path(&path);
        let listener = UnixListener::bind(&socket).expect("bind");
        // The peer accepts and then holds the connection open. Dropping the
        // join handle leaves that thread running; the process exit ends it.
        let _accepted = thread::spawn(move || {
            let _client = listener.accept().expect("accept");
            thread::sleep(Duration::from_secs(30));
        });
        let limit = Duration::from_millis(80);
        let start = Instant::now();
        assert_eq!(ask_within(&path, limit), ShowReply::Unanswered);
        assert!(
            start.elapsed() < limit + Duration::from_millis(500),
            "waited {:?}",
            start.elapsed()
        );
        assert!(
            start.elapsed() + Duration::from_millis(30) >= limit,
            "returned too soon: {:?}",
            start.elapsed()
        );
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn a_stale_socket_is_replaced_and_then_answers() {
        use std::os::unix::net::UnixListener;

        let path = scratch("stale");
        let _lock = InstanceLock::try_acquire(&path).expect("lock");
        let socket = socket_path(&path);
        drop(UnixListener::bind(&socket).expect("old listener"));
        assert!(
            socket
                .symlink_metadata()
                .expect("meta")
                .file_type()
                .is_socket(),
            "the socket file outlives the listener"
        );
        assert_eq!(
            ask(&path),
            ShowReply::Unanswered,
            "nobody answers on the stale socket"
        );

        let listener = ShowListener::spawn(&path).expect("replaced the stale socket");
        assert_eq!(ask(&path), ShowReply::Shown);
        assert!(listener.take_show());
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn a_live_socket_is_not_replaced() {
        let path = scratch("live");
        let _lock = InstanceLock::try_acquire(&path).expect("lock");
        let first = ShowListener::spawn(&path).expect("first");
        let error = ShowListener::spawn(&path).expect_err("second");
        assert_eq!(error.kind(), ErrorKind::AddrInUse);
        assert_eq!(ask(&path), ShowReply::Shown);
        assert!(first.take_show());
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_is_not_a_socket_is_left_in_place() {
        let path = scratch("file");
        let _lock = InstanceLock::try_acquire(&path).expect("lock");
        let socket = socket_path(&path);
        fs::write(&socket, b"keep").expect("file");
        assert!(ShowListener::spawn(&path).is_err());
        assert_eq!(fs::read(&socket).expect("still there"), b"keep");
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn the_socket_is_private_to_this_user() {
        use std::os::unix::fs::PermissionsExt;

        let path = scratch("mode");
        let _lock = InstanceLock::try_acquire(&path).expect("lock");
        let _listener = ShowListener::spawn(&path).expect("listen");
        let mode = fs::metadata(socket_path(&path))
            .expect("socket")
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn a_silent_client_does_not_stick_the_listener() {
        use std::os::unix::net::UnixStream;

        let path = scratch("stuck");
        let _lock = InstanceLock::try_acquire(&path).expect("lock");
        let listener = ShowListener::spawn(&path).expect("listen");
        let silent = UnixStream::connect(socket_path(&path)).expect("silent");
        silent.set_nonblocking(true).expect("nonblocking");
        thread::sleep(EXCHANGE_LIMIT + Duration::from_millis(150));
        drop(silent);
        assert_eq!(ask(&path), ShowReply::Shown);
        assert!(listener.take_show());
        cleanup(&path);
    }
}
