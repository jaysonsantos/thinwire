// SPDX-License-Identifier: AGPL-3.0-only
//! The visible name of a Signal person.
//!
//! Order: the contact name of the store, then the Signal profile name, then
//! the phone number, then a short fallback. The raw ACI never shows.

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
}
