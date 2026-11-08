//! The update check of the Linux and Windows builds.
//!
//! These builds do not install an update. They read the nightly release of
//! the GitHub API, find the newest archive for this OS, and compare its
//! commit count with their own `BUILD_ID`. A newer build opens the release
//! page in the browser when the user asks for it.

use super::{Action, CHECK_FAILED, NOT_A_CLIENT, OfferView, PlainError, Prompt};

/// The release JSON lists every asset of the last days: about 2 KiB each.
pub(super) const MAX_RELEASE_JSON: usize = 1024 * 1024;

const ARCHIVE_SUFFIX: &str = ".tar.gz";

const NEWER_NOTES: &str = "A newer build is available. Download it from the nightly release page.";

/// The commit count of a build id such as `0.1.0.r298.g0be7e95`.
#[must_use]
pub(super) fn build_count(build: &str) -> Option<u64> {
    let mut parts = build.trim().split('.');
    let semver: Vec<&str> = parts.by_ref().take(3).collect();
    if semver.len() != 3
        || semver
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let count = parts.next()?.strip_prefix('r')?;
    let sha = parts.next()?.strip_prefix('g')?;
    if parts.next().is_some()
        || count.is_empty()
        || !count.bytes().all(|b| b.is_ascii_digit())
        || sha.len() != 7
        || !sha.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    count.parse().ok()
}

/// The name prefix of the nightly archive of an OS and a CPU, for example
/// `thinwire-linux-x86_64-`. `None` for a pair that has no nightly archive.
#[must_use]
pub(super) fn asset_prefix(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("thinwire-linux-x86_64-"),
        ("windows", "x86_64") => Some("thinwire-windows-x86_64-"),
        _ => None,
    }
}

#[derive(serde::Deserialize)]
struct ReleaseJson {
    assets: Vec<AssetJson>,
}

#[derive(serde::Deserialize)]
struct AssetJson {
    name: String,
}

/// The build id of the newest archive with this prefix in the release JSON.
/// `Ok(None)` when the release has no archive for this OS.
pub(super) fn newest_build(json: &[u8], prefix: &str) -> Result<Option<String>, PlainError> {
    if json.len() > MAX_RELEASE_JSON {
        return Err(CHECK_FAILED);
    }
    let release: ReleaseJson = serde_json::from_slice(json).map_err(|_| CHECK_FAILED)?;
    Ok(release
        .assets
        .iter()
        .filter_map(|asset| {
            let build = asset
                .name
                .strip_prefix(prefix)?
                .strip_suffix(ARCHIVE_SUFFIX)?;
            Some((build_count(build)?, build.to_string()))
        })
        .max_by_key(|(count, _)| *count)
        .map(|(_, build)| build))
}

/// The update state of a Linux or Windows build.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum State {
    /// This build has no `BUILD_ID`, so it cannot compare.
    Dormant,
    /// No check runs. `skipped` is the newer build that the user put off.
    Quiet {
        our: String,
        skipped: Option<String>,
    },
    /// A check runs. A manual check shows its result, also "up to date".
    Checking {
        our: String,
        skipped: Option<String>,
        manual: bool,
    },
    UpToDate {
        our: String,
    },
    Available {
        our: String,
        newest: String,
    },
    Failed {
        our: Option<String>,
        error: PlainError,
    },
}

#[derive(Debug)]
pub(super) enum Event {
    Action(Action),
    Release(Result<Option<String>, PlainError>),
    Recheck,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Effect {
    None,
    Fetch,
}

#[must_use]
pub(super) fn start(our: Option<String>) -> State {
    match our {
        Some(our) if build_count(&our).is_some() => State::Quiet { our, skipped: None },
        _ => State::Dormant,
    }
}

fn check(our: String, skipped: Option<String>, manual: bool) -> (State, Effect) {
    (
        State::Checking {
            our,
            skipped,
            manual,
        },
        Effect::Fetch,
    )
}

#[must_use]
pub(super) fn step(state: State, event: Event) -> (State, Effect) {
    match (state, event) {
        (State::Dormant, Event::Action(Action::CheckNow)) => (
            State::Failed {
                our: None,
                error: NOT_A_CLIENT,
            },
            Effect::None,
        ),
        (State::Quiet { our, skipped }, Event::Recheck) => check(our, skipped, false),
        (State::Quiet { our, skipped }, Event::Action(Action::CheckNow)) => {
            check(our, skipped, true)
        }
        (
            State::Checking {
                our,
                skipped,
                manual,
            },
            Event::Release(result),
        ) => match result {
            Ok(Some(newest))
                if build_count(&newest) > build_count(&our)
                    && (manual || skipped.as_deref() != Some(newest.as_str())) =>
            {
                (State::Available { our, newest }, Effect::None)
            }
            Ok(_) if manual => (State::UpToDate { our }, Effect::None),
            Ok(_) => (State::Quiet { our, skipped }, Effect::None),
            Err(error) if manual => (
                State::Failed {
                    our: Some(our),
                    error,
                },
                Effect::None,
            ),
            Err(_) => (State::Quiet { our, skipped }, Effect::None),
        },
        // Hide a manual check: its result then shows only a newer build.
        (State::Checking { our, skipped, .. }, Event::Action(Action::Dismiss | Action::Later)) => (
            State::Checking {
                our,
                skipped,
                manual: false,
            },
            Effect::None,
        ),
        (State::Available { our, newest }, Event::Action(Action::Later | Action::Dismiss)) => (
            State::Quiet {
                our,
                skipped: Some(newest),
            },
            Effect::None,
        ),
        (
            State::Available { our, .. } | State::UpToDate { our },
            Event::Action(Action::CheckNow),
        ) => check(our, None, true),
        (State::UpToDate { our }, Event::Action(Action::Dismiss | Action::Later)) => {
            (State::Quiet { our, skipped: None }, Effect::None)
        }
        (State::Failed { our: Some(our), .. }, Event::Action(Action::Retry | Action::CheckNow)) => {
            check(our, None, true)
        }
        (State::Failed { our: Some(our), .. }, Event::Recheck) => check(our, None, false),
        (State::Failed { our: Some(our), .. }, Event::Action(Action::Dismiss)) => {
            (State::Quiet { our, skipped: None }, Effect::None)
        }
        (State::Failed { our: None, .. }, Event::Action(Action::Dismiss)) => {
            (State::Dormant, Effect::None)
        }
        (state, _) => (state, Effect::None),
    }
}

#[must_use]
pub(super) fn project(state: &State) -> Prompt {
    match state {
        State::Dormant | State::Quiet { .. } | State::Checking { manual: false, .. } => {
            Prompt::Quiet
        }
        State::Checking { manual: true, .. } => Prompt::Checking,
        State::UpToDate { our } => Prompt::UpToDate(our.clone()),
        State::Available { newest, .. } => Prompt::Available(OfferView {
            version: newest.clone(),
            notes: NEWER_NOTES.to_string(),
        }),
        State::Failed { error, .. } => Prompt::Failed(*error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OURS: &str = "0.1.0.r298.g0be7e95";

    fn quiet() -> State {
        start(Some(OURS.to_string()))
    }

    #[test]
    fn the_build_count_comes_from_a_full_build_id_only() {
        assert_eq!(build_count(OURS), Some(298));
        assert_eq!(build_count("0.1.0.r298.g0be7e95\n"), Some(298));
        assert_eq!(build_count("0.1.0"), None);
        assert_eq!(build_count("0.1.0.r298"), None);
        assert_eq!(build_count("0.1.0.r298.gxyz1234"), None);
        assert_eq!(build_count("0.1.0.rabc.g0be7e95"), None);
        assert_eq!(build_count("0.1.r298.g0be7e95"), None);
        assert_eq!(build_count("0.1.0.r298.g0be7e95.tar"), None);
    }

    #[test]
    fn only_linux_and_windows_on_x86_64_have_an_archive() {
        assert_eq!(
            asset_prefix("linux", "x86_64"),
            Some("thinwire-linux-x86_64-")
        );
        assert_eq!(
            asset_prefix("windows", "x86_64"),
            Some("thinwire-windows-x86_64-")
        );
        assert_eq!(asset_prefix("macos", "aarch64"), None);
        assert_eq!(asset_prefix("linux", "aarch64"), None);
    }

    #[test]
    fn the_newest_archive_of_this_os_wins() {
        let json = br#"{"assets":[
            {"name":"thinwire-linux-x86_64-0.1.0.r299.g1111111.tar.gz"},
            {"name":"thinwire-linux-x86_64-0.1.0.r1000.g2222222.tar.gz"},
            {"name":"thinwire-windows-x86_64-0.1.0.r2000.g3333333.tar.gz"},
            {"name":"thinwire-linux-x86_64-0.1.0.r3000.gbad.tar.gz"},
            {"name":"appcast.xml"}
        ]}"#;
        assert_eq!(
            newest_build(json, "thinwire-linux-x86_64-"),
            Ok(Some("0.1.0.r1000.g2222222".to_string()))
        );
        assert_eq!(
            newest_build(json, "thinwire-windows-x86_64-"),
            Ok(Some("0.1.0.r2000.g3333333".to_string()))
        );
        assert_eq!(
            newest_build(br#"{"assets":[]}"#, "thinwire-linux-x86_64-"),
            Ok(None)
        );
        assert_eq!(newest_build(b"not json", "x"), Err(CHECK_FAILED));
    }

    #[test]
    fn a_build_with_no_build_id_says_that_it_cannot_check() {
        assert_eq!(start(None), State::Dormant);
        assert_eq!(start(Some("dev".to_string())), State::Dormant);
        let (state, effect) = step(State::Dormant, Event::Action(Action::CheckNow));
        assert_eq!(effect, Effect::None);
        assert_eq!(project(&state), Prompt::Failed(NOT_A_CLIENT));
        let (state, _) = step(state, Event::Action(Action::Dismiss));
        assert_eq!(state, State::Dormant);
    }

    #[test]
    fn a_manual_check_with_no_newer_build_says_up_to_date() {
        let (state, effect) = step(quiet(), Event::Action(Action::CheckNow));
        assert_eq!(effect, Effect::Fetch);
        assert_eq!(project(&state), Prompt::Checking);
        let (state, _) = step(state, Event::Release(Ok(Some(OURS.to_string()))));
        assert_eq!(project(&state), Prompt::UpToDate(OURS.to_string()));
        let (state, _) = step(state, Event::Action(Action::Dismiss));
        assert_eq!(project(&state), Prompt::Quiet);
    }

    #[test]
    fn a_newer_build_shows_the_download_offer() {
        let newer = "0.1.0.r300.gabcdef1".to_string();
        let (state, _) = step(quiet(), Event::Action(Action::CheckNow));
        let (state, _) = step(state, Event::Release(Ok(Some(newer.clone()))));
        match project(&state) {
            Prompt::Available(offer) => assert_eq!(offer.version(), newer),
            other => panic!("expected an offer, got {other:?}"),
        }
    }

    #[test]
    fn an_automatic_check_is_silent_unless_a_newer_build_exists() {
        let (state, effect) = step(quiet(), Event::Recheck);
        assert_eq!(effect, Effect::Fetch);
        assert_eq!(project(&state), Prompt::Quiet);
        let (current, _) = step(state.clone(), Event::Release(Ok(Some(OURS.to_string()))));
        assert_eq!(project(&current), Prompt::Quiet);
        let (failed, _) = step(state, Event::Release(Err(CHECK_FAILED)));
        assert_eq!(project(&failed), Prompt::Quiet);
    }

    #[test]
    fn a_build_put_off_with_later_does_not_come_back_on_its_own() {
        let newer = "0.1.0.r300.gabcdef1".to_string();
        let (state, _) = step(quiet(), Event::Recheck);
        let (state, _) = step(state, Event::Release(Ok(Some(newer.clone()))));
        let (state, _) = step(state, Event::Action(Action::Later));
        assert_eq!(project(&state), Prompt::Quiet);
        let (state, _) = step(state, Event::Recheck);
        let (auto, _) = step(state, Event::Release(Ok(Some(newer.clone()))));
        assert_eq!(project(&auto), Prompt::Quiet);
        let (manual, _) = step(auto, Event::Action(Action::CheckNow));
        let (manual, _) = step(manual, Event::Release(Ok(Some(newer))));
        assert!(matches!(project(&manual), Prompt::Available(_)));
    }

    #[test]
    fn a_failed_manual_check_can_be_retried() {
        let (state, _) = step(quiet(), Event::Action(Action::CheckNow));
        let (state, _) = step(state, Event::Release(Err(CHECK_FAILED)));
        assert_eq!(project(&state), Prompt::Failed(CHECK_FAILED));
        let (state, effect) = step(state, Event::Action(Action::Retry));
        assert_eq!(effect, Effect::Fetch);
        assert_eq!(project(&state), Prompt::Checking);
    }

    #[test]
    fn hiding_a_manual_check_keeps_the_check_running() {
        let (state, _) = step(quiet(), Event::Action(Action::CheckNow));
        let (state, effect) = step(state, Event::Action(Action::Dismiss));
        assert_eq!(effect, Effect::None);
        assert_eq!(project(&state), Prompt::Quiet);
        let (state, _) = step(state, Event::Release(Ok(None)));
        assert_eq!(project(&state), Prompt::Quiet);
    }
}
