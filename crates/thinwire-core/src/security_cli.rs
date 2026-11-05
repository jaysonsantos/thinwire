//! macOS Keychain through the `security` command. For local builds only.
//!
//! The native Keychain store asks for permission every time the binary
//! changes, and a local build changes it on every compile. Apple signs
//! `/usr/bin/security` and its path never changes, so the item that this
//! store creates trusts `security` and asks once. This store runs that tool
//! instead of calling the Keychain API. It plugs into `keyring_core`, so
//! every secret of the app and of the helper processes goes through it.
//!
//! Feature `keychain-security-cli`. It is on in `scripts/local-build.sh` and
//! never in CI or in a release: `build.rs` stops a build when `CI` or
//! `GITHUB_ACTIONS` is set, and `scripts/check-local-only-features.sh` keeps
//! the name out of the workflows.
//!
//! A secret never shows in a process list. A read prints it on the stdout of
//! `security`. A write sends the command to `security -i` on its stdin, with
//! the value as hex, so there is no quoting to get wrong and nothing on a
//! command line. Neither a value nor the output of `security` goes to a log
//! or an error text.
//!
//! Limits, from the behavior of `security` (checked against the real tool):
//! - A value is printable ASCII and at most [`MAX_SECRET`] bytes. `-w` prints
//!   any other data as hex, which a reader cannot tell from text. `security -i`
//!   cuts a command at about 4 KiB, and a cut command stores a cut value.
//! - A name is letters, digits, `.`, `_` and `-`, so a name cannot add an
//!   option to the command.

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;

use keyring_core::api::{Credential, CredentialApi, CredentialPersistence, CredentialStoreApi};
use keyring_core::{Entry, Error, Result};

/// Full path: a `PATH` lookup could find another program.
const SECURITY: &str = "/usr/bin/security";

/// Longest value the store writes. The command for it is under 2.2 KiB.
const MAX_SECRET: usize = 1024;

/// Longest service or account name.
const MAX_NAME: usize = 128;

/// Exit status of `security` for an item that is not in the keychain.
const EXIT_NOT_FOUND: i32 = 44;

/// What one run of `security` returned.
#[derive(Debug)]
struct Outcome {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
}

impl Outcome {
    fn succeeded(&self) -> bool {
        self.code == Some(0)
    }
}

/// Runs `security`. A test replaces it with a fake.
trait Runner: Send + Sync {
    fn run(&self, args: &[&str], stdin: Option<&str>) -> io::Result<Outcome>;
}

/// The real `/usr/bin/security`.
struct SecurityCommand;

impl Runner for SecurityCommand {
    fn run(&self, args: &[&str], stdin: Option<&str>) -> io::Result<Outcome> {
        let mut child = Command::new(SECURITY)
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
            // Dropping the pipe closes it, and `security -i` stops at the end.
            pipe.write_all(text.as_bytes())?;
        }
        let output = child.wait_with_output()?;
        Ok(Outcome {
            code: output.status.code(),
            stdout: output.stdout,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// The credential store. Install it with `keyring_core::set_default_store`.
pub(crate) struct Store {
    runner: Arc<dyn Runner>,
    /// A keychain file other than the default. Only tests use it, so a test
    /// never touches the login keychain.
    keychain: Option<String>,
}

impl Store {
    /// The store for the real command. It fails when `/usr/bin/security` is
    /// missing: then there is no Keychain to ask, as with the native store.
    pub(crate) fn new() -> Result<Arc<Self>> {
        if !std::path::Path::new(SECURITY).is_file() {
            return Err(spawn_failed(io::Error::new(
                io::ErrorKind::NotFound,
                "/usr/bin/security is missing",
            )));
        }
        Ok(Arc::new(Self {
            runner: Arc::new(SecurityCommand),
            keychain: None,
        }))
    }
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("security_cli::Store")
            .finish_non_exhaustive()
    }
}

impl CredentialStoreApi for Store {
    fn vendor(&self) -> String {
        "thinwire security(1) store".to_owned()
    }

    fn id(&self) -> String {
        "macos-security-cli-1".to_owned()
    }

    fn build(
        &self,
        service: &str,
        user: &str,
        modifiers: Option<&HashMap<&str, &str>>,
    ) -> Result<Entry> {
        if modifiers.is_some_and(|modifiers| !modifiers.is_empty()) {
            return Err(Error::NotSupportedByStore(
                "the security command store takes no modifiers".to_owned(),
            ));
        }
        check_name("service", service)?;
        check_name("user", user)?;
        Ok(Entry::new_with_credential(Arc::new(SecurityCredential {
            runner: Arc::clone(&self.runner),
            keychain: self.keychain.clone(),
            service: service.to_owned(),
            user: user.to_owned(),
        })))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn persistence(&self) -> CredentialPersistence {
        CredentialPersistence::UntilDelete
    }
}

/// One Keychain item, named by its service and account.
struct SecurityCredential {
    runner: Arc<dyn Runner>,
    keychain: Option<String>,
    service: String,
    user: String,
}

impl fmt::Debug for SecurityCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecurityCredential")
            .field("service", &self.service)
            .field("user", &self.user)
            .finish()
    }
}

impl SecurityCredential {
    /// `<verb> -s <service> -a <user> [-w] [keychain]`.
    fn lookup(&self, verb: &'static str, print_secret: bool) -> Result<Outcome> {
        let mut args = vec![verb, "-s", &self.service, "-a", &self.user];
        if print_secret {
            args.push("-w");
        }
        if let Some(keychain) = &self.keychain {
            args.push(keychain);
        }
        self.runner.run(&args, None).map_err(spawn_failed)
    }
}

impl CredentialApi for SecurityCredential {
    fn set_secret(&self, secret: &[u8]) -> Result<()> {
        check_secret(secret)?;
        let mut command = format!(
            "add-generic-password -U -s {} -a {} -X {}",
            self.service,
            self.user,
            hex(secret)
        );
        if let Some(keychain) = &self.keychain {
            command.push(' ');
            command.push_str(keychain);
        }
        command.push('\n');
        let outcome = self
            .runner
            .run(&["-i"], Some(&command))
            .map_err(spawn_failed)?;
        if outcome.succeeded() {
            Ok(())
        } else {
            Err(failure(&outcome))
        }
    }

    fn get_secret(&self) -> Result<Vec<u8>> {
        let outcome = self.lookup("find-generic-password", true)?;
        if !outcome.succeeded() {
            return Err(failure(&outcome));
        }
        let mut secret = outcome.stdout;
        // `security` ends the value with one newline.
        if secret.last() == Some(&b'\n') {
            secret.pop();
        }
        Ok(secret)
    }

    fn delete_credential(&self) -> Result<()> {
        let outcome = self.lookup("delete-generic-password", false)?;
        if outcome.succeeded() {
            Ok(())
        } else {
            Err(failure(&outcome))
        }
    }

    /// This credential is its own wrapper. A lookup without `-w` proves that
    /// the item exists and does not read the secret.
    fn get_credential(&self) -> Result<Option<Arc<Credential>>> {
        let outcome = self.lookup("find-generic-password", false)?;
        if outcome.succeeded() {
            Ok(None)
        } else {
            Err(failure(&outcome))
        }
    }

    fn get_specifiers(&self) -> Option<(String, String)> {
        Some((self.service.clone(), self.user.clone()))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A service or an account name that cannot add an option to a command.
fn check_name(what: &str, name: &str) -> Result<()> {
    let plain = !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if plain {
        Ok(())
    } else {
        Err(Error::Invalid(
            what.to_owned(),
            "letters, digits, '.', '_' and '-' only".to_owned(),
        ))
    }
}

/// A value that `security -w` prints back as the same text.
fn check_secret(secret: &[u8]) -> Result<()> {
    if secret.is_empty() {
        return Err(Error::Invalid("secret".to_owned(), "empty".to_owned()));
    }
    if secret.len() > MAX_SECRET {
        return Err(Error::TooLong("secret".to_owned(), MAX_SECRET as u32));
    }
    if !secret.iter().all(|byte| (0x20..=0x7e).contains(byte)) {
        return Err(Error::Invalid(
            "secret".to_owned(),
            "printable ASCII only".to_owned(),
        ));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(DIGITS[usize::from(byte >> 4)] as char);
        text.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    text
}

/// What the platform error says. It has the exit status and never the output
/// of `security`, which can hold the secret.
#[derive(Debug)]
struct SecurityFailed {
    code: Option<i32>,
}

impl fmt::Display for SecurityFailed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            Some(code) => write!(f, "the security command exited with status {code}"),
            None => f.write_str("the security command was stopped by a signal"),
        }
    }
}

impl std::error::Error for SecurityFailed {}

fn spawn_failed(error: io::Error) -> Error {
    Error::PlatformFailure(Box::new(error))
}

/// Map a failed run. A locked keychain, a refusal, and a missing screen to
/// ask on all count as no storage access: the user can fix them and retry.
fn failure(outcome: &Outcome) -> Error {
    if outcome.code == Some(EXIT_NOT_FOUND) {
        return Error::NoEntry;
    }
    let cause = Box::new(SecurityFailed { code: outcome.code });
    let blocked = [
        "User interaction is not allowed",
        "User canceled",
        "authorization",
        "denied",
        "locked",
    ]
    .iter()
    .any(|phrase| outcome.stderr.contains(phrase));
    if blocked {
        Error::NoStorageAccess(cause)
    } else {
        Error::PlatformFailure(cause)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// One call of the fake: the arguments and what went to stdin.
    #[derive(Debug, Clone)]
    struct Call {
        args: Vec<String>,
        stdin: Option<String>,
    }

    /// A stand-in for `security` that keeps items in a map and records calls.
    #[derive(Default)]
    struct FakeSecurity {
        items: Mutex<HashMap<(String, String), Vec<u8>>>,
        calls: Mutex<Vec<Call>>,
        /// Make every run fail with this status and message.
        fail: Mutex<Option<(i32, &'static str)>>,
    }

    impl FakeSecurity {
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().expect("calls").clone()
        }

        fn item_count(&self) -> usize {
            self.items.lock().expect("items").len()
        }
    }

    fn flag<'a>(words: &[&'a str], name: &str) -> Option<&'a str> {
        words
            .iter()
            .position(|word| *word == name)
            .and_then(|at| words.get(at + 1).copied())
    }

    fn unhex(text: &str) -> Vec<u8> {
        text.as_bytes()
            .chunks(2)
            .map(|pair| {
                u8::from_str_radix(std::str::from_utf8(pair).expect("ascii"), 16).expect("hex")
            })
            .collect()
    }

    impl Runner for FakeSecurity {
        fn run(&self, args: &[&str], stdin: Option<&str>) -> io::Result<Outcome> {
            self.calls.lock().expect("calls").push(Call {
                args: args.iter().map(|arg| (*arg).to_owned()).collect(),
                stdin: stdin.map(str::to_owned),
            });
            if let Some((code, message)) = *self.fail.lock().expect("fail") {
                return Ok(Outcome {
                    code: Some(code),
                    stdout: Vec::new(),
                    stderr: message.to_owned(),
                });
            }
            let ok = |stdout: Vec<u8>| {
                Ok(Outcome {
                    code: Some(0),
                    stdout,
                    stderr: String::new(),
                })
            };
            let missing = || {
                Ok(Outcome {
                    code: Some(EXIT_NOT_FOUND),
                    stdout: Vec::new(),
                    stderr: "security: SecKeychainSearchCopyNext: The specified item could not be found in the keychain.".to_owned(),
                })
            };
            let mut items = self.items.lock().expect("items");
            match args {
                ["-i"] => {
                    let line = stdin.expect("a command on stdin");
                    let words: Vec<&str> = line.split_whitespace().collect();
                    assert_eq!(words[0], "add-generic-password");
                    let key = (
                        flag(&words, "-s").expect("service").to_owned(),
                        flag(&words, "-a").expect("account").to_owned(),
                    );
                    items.insert(key, unhex(flag(&words, "-X").expect("hex")));
                    ok(Vec::new())
                }
                [verb, rest @ ..] => {
                    let key = (
                        flag(rest, "-s").expect("service").to_owned(),
                        flag(rest, "-a").expect("account").to_owned(),
                    );
                    match (*verb, items.get(&key).cloned()) {
                        ("find-generic-password", Some(value)) if rest.contains(&"-w") => {
                            let mut out = value;
                            out.push(b'\n');
                            ok(out)
                        }
                        ("find-generic-password", Some(_)) => ok(b"keychain: metadata".to_vec()),
                        ("delete-generic-password", Some(_)) => {
                            items.remove(&key);
                            ok(Vec::new())
                        }
                        (_, None) => missing(),
                        (other, _) => panic!("unexpected verb {other}"),
                    }
                }
                [] => panic!("no arguments"),
            }
        }
    }

    fn store(fake: &Arc<FakeSecurity>) -> Store {
        Store {
            runner: Arc::clone(fake) as Arc<dyn Runner>,
            keychain: None,
        }
    }

    #[test]
    fn a_secret_goes_in_and_comes_out_and_an_update_keeps_one_item() {
        let fake = Arc::new(FakeSecurity::default());
        let entry = store(&fake)
            .build("thinwire", "api_id", None)
            .expect("entry");
        assert!(matches!(entry.get_password(), Err(Error::NoEntry)));
        entry.set_password("123456").expect("set");
        assert_eq!(entry.get_password().expect("get"), "123456");
        entry.set_password("a b\"c; rm -rf").expect("update");
        assert_eq!(entry.get_password().expect("get"), "a b\"c; rm -rf");
        assert_eq!(fake.item_count(), 1, "-U updates in place");
        entry.delete_credential().expect("delete");
        assert!(matches!(entry.get_password(), Err(Error::NoEntry)));
        assert!(matches!(entry.delete_credential(), Err(Error::NoEntry)));
    }

    #[test]
    fn a_secret_is_never_on_a_command_line() {
        let fake = Arc::new(FakeSecurity::default());
        let entry = store(&fake)
            .build("thinwire", "discord.bot_token", None)
            .expect("entry");
        let secret = "bot-token-VALUE_123.part-two";
        entry.set_password(secret).expect("set");
        let _ = entry.get_password().expect("get");
        for call in fake.calls() {
            for arg in &call.args {
                assert!(!arg.contains(secret), "plain text in {:?}", call.args);
                assert!(
                    !arg.contains(&hex(secret.as_bytes())),
                    "hex in {:?}",
                    call.args
                );
            }
        }
        let write = &fake.calls()[0];
        assert_eq!(write.args, ["-i"], "the write has no argument but -i");
        let stdin = write.stdin.as_deref().expect("stdin");
        assert!(
            stdin.contains(&hex(secret.as_bytes())),
            "the value goes as hex on stdin"
        );
        assert!(!stdin.contains(secret), "never as plain text");
        assert!(stdin.ends_with('\n'));
    }

    #[test]
    fn a_value_that_security_would_not_return_as_text_is_refused() {
        let fake = Arc::new(FakeSecurity::default());
        let entry = store(&fake)
            .build("thinwire", "session", None)
            .expect("entry");
        assert!(matches!(entry.set_password(""), Err(Error::Invalid(..))));
        assert!(matches!(
            entry.set_password("two\nlines"),
            Err(Error::Invalid(..))
        ));
        assert!(matches!(
            entry.set_password("ação"),
            Err(Error::Invalid(..))
        ));
        assert!(matches!(
            entry.set_secret(&[0, 1, 255]),
            Err(Error::Invalid(..))
        ));
        let too_long = "x".repeat(MAX_SECRET + 1);
        assert!(matches!(
            entry.set_password(&too_long),
            Err(Error::TooLong(_, _))
        ));
        let longest = "x".repeat(MAX_SECRET);
        entry
            .set_password(&longest)
            .expect("the longest value fits");
        assert_eq!(entry.get_password().expect("get"), longest);
        let writes = fake
            .calls()
            .iter()
            .filter(|call| call.stdin.is_some())
            .count();
        assert_eq!(writes, 1, "a refused value never reaches security");
    }

    #[test]
    fn the_longest_command_stays_under_what_security_reads() {
        let fake = Arc::new(FakeSecurity::default());
        let entry = store(&fake)
            .build("thinwire", &"a".repeat(MAX_NAME), None)
            .expect("entry");
        entry.set_password(&"x".repeat(MAX_SECRET)).expect("set");
        let stdin = fake.calls()[0].stdin.clone().expect("stdin");
        // `security -i` cuts a command at about 4096 bytes. A cut command
        // stores a cut value, so the margin is not optional.
        assert!(stdin.len() < 3000, "{} bytes", stdin.len());
    }

    #[test]
    fn a_name_cannot_add_an_option_to_the_command() {
        let fake = Arc::new(FakeSecurity::default());
        let store = store(&fake);
        for bad in ["", "a b", "a -X 00", "a\nb", "a\"b", "é", "a;b"] {
            assert!(
                matches!(store.build(bad, "user", None), Err(Error::Invalid(..))),
                "service {bad:?}"
            );
            assert!(
                matches!(store.build("thinwire", bad, None), Err(Error::Invalid(..))),
                "user {bad:?}"
            );
        }
        assert!(store.build("thinwire", "signal.store_key", None).is_ok());
        assert!(store.build("thinwire", "whatsapp.store_key", None).is_ok());
        assert!(
            fake.calls().is_empty(),
            "a name check does not run security"
        );
    }

    #[test]
    fn modifiers_are_not_supported() {
        let fake = Arc::new(FakeSecurity::default());
        let modifiers = HashMap::from([("k", "v")]);
        assert!(matches!(
            store(&fake).build("thinwire", "api_id", Some(&modifiers)),
            Err(Error::NotSupportedByStore(_))
        ));
        assert!(
            store(&fake)
                .build("thinwire", "api_id", Some(&HashMap::new()))
                .is_ok()
        );
    }

    #[test]
    fn a_lookup_without_w_never_reads_the_secret() {
        let fake = Arc::new(FakeSecurity::default());
        let entry = store(&fake)
            .build("thinwire", "api_hash", None)
            .expect("entry");
        assert!(matches!(entry.get_credential(), Err(Error::NoEntry)));
        entry.set_password("0123abcd").expect("set");
        entry.get_credential().expect("the item exists");
        let lookup = fake.calls().pop().expect("call");
        assert!(
            !lookup.args.iter().any(|arg| arg == "-w"),
            "{:?}",
            lookup.args
        );
        assert_eq!(
            entry.get_specifiers(),
            Some(("thinwire".to_owned(), "api_hash".to_owned()))
        );
    }

    #[test]
    fn a_failed_run_maps_to_the_error_a_user_can_act_on() {
        let fake = Arc::new(FakeSecurity::default());
        let entry = store(&fake)
            .build("thinwire", "api_id", None)
            .expect("entry");
        *fake.fail.lock().expect("fail") = Some((36, "User interaction is not allowed."));
        assert!(matches!(
            entry.set_password("1"),
            Err(Error::NoStorageAccess(_))
        ));
        assert!(matches!(
            entry.get_password(),
            Err(Error::NoStorageAccess(_))
        ));
        *fake.fail.lock().expect("fail") = Some((2, "usage: something else"));
        assert!(matches!(
            entry.get_password(),
            Err(Error::PlatformFailure(_))
        ));
        *fake.fail.lock().expect("fail") = Some((44, "could not be found"));
        assert!(matches!(entry.get_password(), Err(Error::NoEntry)));
    }

    #[test]
    fn an_error_text_never_carries_the_output_of_security() {
        let fake = Arc::new(FakeSecurity::default());
        let entry = store(&fake)
            .build("thinwire", "api_id", None)
            .expect("entry");
        *fake.fail.lock().expect("fail") = Some((1, "SECRET-IN-STDERR-123456"));
        let error = entry.get_password().expect_err("a failure");
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains("SECRET-IN-STDERR"), "{shown}");
        assert!(shown.contains("status 1"), "{shown}");
    }

    /// A temporary keychain file. It is deleted, and the search list of the
    /// user is restored, even if the test fails.
    struct TempKeychain(String);

    impl TempKeychain {
        fn create() -> Self {
            let path = format!(
                "/private/tmp/thinwire-selftest-{}.keychain-db",
                std::process::id()
            );
            let run = |args: &[&str]| {
                Command::new(SECURITY)
                    .args(args)
                    .output()
                    .expect("run security")
                    .status
                    .success()
            };
            assert!(
                run(&["create-keychain", "-p", "pw", &path]),
                "create-keychain"
            );
            assert!(
                run(&["unlock-keychain", "-p", "pw", &path]),
                "unlock-keychain"
            );
            Self(path)
        }
    }

    impl Drop for TempKeychain {
        fn drop(&mut self) {
            let _ = Command::new(SECURITY)
                .args(["delete-keychain", &self.0])
                .output();
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// The real `/usr/bin/security`, on a temporary keychain: the login
    /// keychain stays untouched. It needs a macOS session that can create a
    /// keychain, so it is not part of a normal run:
    /// `cargo test -p thinwire-core --features keychain-security-cli -- --ignored`
    #[test]
    #[ignore = "creates a temporary keychain with the real security command"]
    fn the_real_security_command_round_trips_on_a_temporary_keychain() {
        let temp = TempKeychain::create();
        let store = Store {
            runner: Arc::new(SecurityCommand),
            keychain: Some(temp.0.clone()),
        };
        let entry = store
            .build("thinwire-selftest", "round_trip", None)
            .expect("entry");
        assert!(matches!(entry.get_password(), Err(Error::NoEntry)));
        assert!(matches!(entry.delete_credential(), Err(Error::NoEntry)));

        entry.set_password("hello").expect("set");
        assert_eq!(entry.get_password().expect("get"), "hello");
        entry.get_credential().expect("exists");

        let odd = "a b\"c; $HOME `x` 123456:ABC-def_GHI.jkl/mno+pqr=";
        entry.set_password(odd).expect("update with odd characters");
        assert_eq!(entry.get_password().expect("get"), odd);

        let longest = "k".repeat(MAX_SECRET);
        entry.set_password(&longest).expect("the longest value");
        assert_eq!(entry.get_password().expect("get"), longest);

        entry.delete_credential().expect("delete");
        assert!(matches!(entry.get_password(), Err(Error::NoEntry)));
    }
}
