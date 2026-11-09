// SPDX-License-Identifier: AGPL-3.0-only
//! The visible name of a Signal person.
//!
//! Order: the contact name of the store, then the Signal profile name, then
//! the phone number, then a short fallback. The raw ACI never shows.

use std::collections::{HashMap, HashSet};

const UNKNOWN_PERSON: &str = "Signal user";
/// The number of id characters that the fallback shows, so that two
/// unknown people do not look the same.
const UNKNOWN_SUFFIX: usize = 4;

/// The best name of a person. `profile` is the decrypted profile name and
/// `phone` the E.164 number, when the store has them.
#[cfg_attr(not(feature = "signal-local"), allow(dead_code))]
#[must_use]
pub(crate) fn person_title(
    contact: &str,
    profile: Option<&str>,
    phone: Option<&str>,
    id: &str,
) -> String {
    [Some(contact), profile, phone]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|name| !name.is_empty())
        .map_or_else(|| unknown_person(id), str::to_string)
}

/// The name of a person with no contact name, no profile name, and no
/// phone number: "Signal user" and the first characters of the id.
#[must_use]
pub(crate) fn unknown_person(id: &str) -> String {
    let raw = id.strip_prefix("pni:").unwrap_or(id);
    let suffix: String = raw
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(UNKNOWN_SUFFIX)
        .collect();
    if suffix.is_empty() {
        UNKNOWN_PERSON.to_string()
    } else {
        format!("{UNKNOWN_PERSON} {suffix}")
    }
}

/// The given name and the family name of a profile, joined.
#[cfg_attr(not(feature = "signal-local"), allow(dead_code))]
#[must_use]
pub(crate) fn profile_display_name(given: &str, family: Option<&str>) -> Option<String> {
    let joined = match family.map(str::trim).filter(|family| !family.is_empty()) {
        Some(family) => format!("{} {family}", given.trim()),
        None => given.trim().to_string(),
    };
    let joined = joined.trim().to_string();
    (!joined.is_empty()).then_some(joined)
}

/// The live senders that this run looked up, and the names it found.
/// `Debug` prints the counts only.
#[cfg_attr(not(feature = "signal-local"), allow(dead_code))]
#[derive(Default)]
pub(crate) struct SenderLookups {
    tried: HashSet<String>,
    found: HashMap<String, String>,
}

#[cfg_attr(not(feature = "signal-local"), allow(dead_code))]
impl SenderLookups {
    /// `true` one time for each sender in this run: start one lookup now.
    /// A failed lookup does not come back here.
    pub(crate) fn claim(&mut self, sender: &str) -> bool {
        !sender.is_empty() && self.tried.insert(sender.to_string())
    }

    /// Keep the name that a lookup found for `sender`.
    pub(crate) fn record(&mut self, sender: String, name: String) {
        self.found.insert(sender, name);
    }

    /// Add the found names to `names`. A name that `names` has stays.
    pub(crate) fn fill(&self, names: &mut HashMap<String, String>) {
        for (id, name) in &self.found {
            names.entry(id.clone()).or_insert_with(|| name.clone());
        }
    }
}

impl std::fmt::Debug for SenderLookups {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SenderLookups")
            .field("tried", &self.tried.len())
            .field("found", &self.found.len())
            .finish()
    }
}

/// The profile of one live sender.
#[cfg_attr(not(feature = "signal-local"), allow(dead_code))]
pub(crate) trait SenderProfile {
    /// The contact name or the stored profile name. No network.
    async fn stored_name(&mut self) -> Option<String>;
    /// Ask Signal for the profile name.
    async fn fetch_name(&mut self) -> Option<String>;
}

/// The new name of a live sender, or `None` when the shown name stays.
/// `shown` is the name that the rows show now. A stored name does not
/// ask Signal. Only a sender with no stored name asks Signal, one time.
#[cfg_attr(not(feature = "signal-local"), allow(dead_code))]
pub(crate) async fn look_up_sender(
    profile: &mut impl SenderProfile,
    shown: Option<&str>,
) -> Option<String> {
    if let Some(name) = profile.stored_name().await {
        return (shown != Some(name.as_str())).then_some(name);
    }
    profile
        .fetch_name()
        .await
        .filter(|name| shown != Some(name.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "5b178e15-fa42-4955-b18b-aa6b5c000001";

    #[test]
    fn the_contact_name_wins() {
        assert_eq!(
            person_title("Ada", Some("Ada L."), Some("+4915112345678"), ID),
            "Ada"
        );
    }

    #[test]
    fn the_profile_name_is_next() {
        assert_eq!(
            person_title("", Some("Ada L."), Some("+4915112345678"), ID),
            "Ada L."
        );
        assert_eq!(person_title("  ", Some("Ada L."), None, ID), "Ada L.");
    }

    #[test]
    fn the_phone_number_is_next() {
        assert_eq!(
            person_title("", None, Some("+4915112345678"), ID),
            "+4915112345678"
        );
        assert_eq!(
            person_title("", Some(" "), Some("+4915112345678"), ID),
            "+4915112345678"
        );
    }

    #[test]
    fn the_fallback_is_last_and_never_the_raw_id() {
        let title = person_title("", None, None, ID);
        assert_eq!(title, "Signal user 5b17");
        assert!(!title.contains(ID));
        assert_eq!(unknown_person(&format!("pni:{ID}")), "Signal user 5b17");
        assert_eq!(unknown_person(""), "Signal user");
    }

    #[test]
    fn a_profile_name_joins_the_given_and_the_family_name() {
        assert_eq!(
            profile_display_name("Ada", Some("Lovelace")).as_deref(),
            Some("Ada Lovelace")
        );
        assert_eq!(
            profile_display_name("Ada", Some(" ")).as_deref(),
            Some("Ada")
        );
        assert_eq!(profile_display_name(" ", None), None);
    }

    /// A fake profile: a stored name, a name that Signal gives, and a count
    /// of the network fetches.
    struct FakeProfile {
        stored: Option<&'static str>,
        remote: Option<&'static str>,
        fetches: usize,
    }

    impl SenderProfile for FakeProfile {
        async fn stored_name(&mut self) -> Option<String> {
            self.stored.map(str::to_string)
        }

        async fn fetch_name(&mut self) -> Option<String> {
            self.fetches += 1;
            self.remote.map(str::to_string)
        }
    }

    fn fake(stored: Option<&'static str>, remote: Option<&'static str>) -> FakeProfile {
        FakeProfile {
            stored,
            remote,
            fetches: 0,
        }
    }

    /// Run a live message from `sender`: claim, look up, and count the
    /// refreshes, as the receive loop does.
    fn on_live_message(
        lookups: &mut SenderLookups,
        profile: &mut FakeProfile,
        sender: &str,
        shown: Option<&str>,
        refreshes: &mut usize,
    ) {
        if !lookups.claim(sender) {
            return;
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        if let Some(name) = runtime.block_on(look_up_sender(profile, shown)) {
            lookups.record(sender.to_string(), name);
            *refreshes += 1;
        }
    }

    #[test]
    fn an_unnamed_live_sender_fetches_one_time() {
        let mut lookups = SenderLookups::default();
        let mut profile = fake(None, None);
        let mut refreshes = 0;
        on_live_message(&mut lookups, &mut profile, ID, None, &mut refreshes);
        assert_eq!(profile.fetches, 1, "one fetch");
        assert_eq!(refreshes, 0, "a failed fetch does not refresh");
        on_live_message(&mut lookups, &mut profile, ID, None, &mut refreshes);
        assert_eq!(profile.fetches, 1, "a failed fetch is not tried again");
    }

    #[test]
    fn a_named_live_sender_does_not_fetch() {
        let mut lookups = SenderLookups::default();
        let mut profile = fake(Some("Ada"), Some("Ada L."));
        let mut refreshes = 0;
        on_live_message(&mut lookups, &mut profile, ID, Some("Ada"), &mut refreshes);
        assert_eq!(profile.fetches, 0);
        assert_eq!(refreshes, 0, "the shown name stays");
    }

    #[test]
    fn a_stored_name_that_the_rows_do_not_show_refreshes_with_no_fetch() {
        let mut lookups = SenderLookups::default();
        let mut profile = fake(Some("Ada"), None);
        let mut refreshes = 0;
        on_live_message(&mut lookups, &mut profile, ID, None, &mut refreshes);
        assert_eq!(profile.fetches, 0);
        assert_eq!(refreshes, 1);
    }

    #[test]
    fn a_fetched_name_refreshes_and_fills_the_names() {
        let mut lookups = SenderLookups::default();
        let mut profile = fake(None, Some("Ada L."));
        let mut refreshes = 0;
        on_live_message(
            &mut lookups,
            &mut profile,
            ID,
            Some("+4915112345678"),
            &mut refreshes,
        );
        assert_eq!(profile.fetches, 1);
        assert_eq!(refreshes, 1, "a fetched name refreshes the rows");
        let mut names = HashMap::from([("other".to_string(), "Bea".to_string())]);
        lookups.fill(&mut names);
        assert_eq!(names.get(ID).map(String::as_str), Some("Ada L."));
        assert_eq!(names.get("other").map(String::as_str), Some("Bea"));
        assert!(
            !format!("{lookups:?}").contains("Ada"),
            "Debug shows no name"
        );
        assert!(!format!("{lookups:?}").contains(ID), "Debug shows no id");
    }

    #[test]
    fn a_second_message_of_the_same_sender_does_not_fetch_again() {
        let mut lookups = SenderLookups::default();
        let mut profile = fake(None, Some("Ada L."));
        let mut refreshes = 0;
        on_live_message(&mut lookups, &mut profile, ID, None, &mut refreshes);
        on_live_message(&mut lookups, &mut profile, ID, None, &mut refreshes);
        assert_eq!(profile.fetches, 1);
        assert_eq!(refreshes, 1);
        assert!(!lookups.claim(""), "an empty id never claims");
    }
}
