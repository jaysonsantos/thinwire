// SPDX-License-Identifier: AGPL-3.0-only
#![cfg_attr(not(feature = "signal-local"), allow(dead_code))]
//! Signal timestamps are milliseconds. `ChatMessage::sent_at` is Unix seconds.

#[must_use]
pub(crate) fn sent_at_secs(millis: u64) -> i64 {
    i64::try_from(millis / 1000).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn milliseconds_become_unix_seconds() {
        assert_eq!(sent_at_secs(1_700_000_123_000), 1_700_000_123);
        assert_eq!(sent_at_secs(999), 0);
        assert_eq!(sent_at_secs(0), 0);
    }
}
