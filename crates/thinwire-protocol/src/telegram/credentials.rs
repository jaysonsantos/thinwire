//! Resolve Telegram `api_id` / `api_hash` without putting them on commands.
//!
//! Precedence: keychain override, then publisher inject from
//! `TELEGRAM_API_ID` / `TELEGRAM_API_HASH` at compile time. Neither value is
//! logged or stored in the git tree.

use std::fmt;

use crate::adapter::{AdapterError, ProtocolId};
use crate::secrets::{TelegramSecretKey, TelegramSecretVault};

/// Where a resolved API pair came from. Never carries the values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelegramApiOrigin {
    KeychainOverride,
    PublisherInject,
}

/// Compile-time publisher pair, if the official build injected one.
#[derive(Clone)]
pub struct TelegramApiSource {
    publisher_id: Option<String>,
    publisher_hash: Option<String>,
}

impl TelegramApiSource {
    /// Read `option_env!("TELEGRAM_API_ID")` / `TELEGRAM_API_HASH`.
    /// Empty strings count as missing. Public CI must not set these.
    #[must_use]
    pub fn from_build() -> Self {
        Self {
            publisher_id: option_env!("TELEGRAM_API_ID")
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned),
            publisher_hash: option_env!("TELEGRAM_API_HASH")
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned),
        }
    }

    /// No publisher pair. Dev / CI default unless env was injected.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            publisher_id: None,
            publisher_hash: None,
        }
    }

    /// Test helper. Callers must not use real production credentials.
    #[must_use]
    pub fn with_publisher(api_id: &str, api_hash: &str) -> Self {
        let id = api_id.trim();
        let hash = api_hash.trim();
        if id.is_empty() || hash.is_empty() {
            return Self::empty();
        }
        Self {
            publisher_id: Some(id.to_string()),
            publisher_hash: Some(hash.to_string()),
        }
    }

    #[must_use]
    pub fn has_publisher(&self) -> bool {
        self.publisher_id.is_some() && self.publisher_hash.is_some()
    }

    fn publisher_pair(&self) -> Option<(&str, &str)> {
        Some((
            self.publisher_id.as_deref()?,
            self.publisher_hash.as_deref()?,
        ))
    }
}

impl Default for TelegramApiSource {
    fn default() -> Self {
        Self::from_build()
    }
}

impl fmt::Debug for TelegramApiSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TelegramApiSource")
            .field("publisher", &self.has_publisher())
            .finish()
    }
}

/// Keychain override wins. Publisher inject is second. Values are never logged.
#[must_use]
pub fn resolve_telegram_api(
    vault: &dyn TelegramSecretVault,
    source: &TelegramApiSource,
) -> Option<(String, String, TelegramApiOrigin)> {
    let override_id = vault.get_secret(TelegramSecretKey::ApiId);
    let override_hash = vault.get_secret(TelegramSecretKey::ApiHash);
    if let (Some(id), Some(hash)) = (override_id, override_hash)
        && !id.is_empty()
        && !hash.is_empty()
    {
        return Some((id, hash, TelegramApiOrigin::KeychainOverride));
    }
    source.publisher_pair().map(|(id, hash)| {
        (
            id.to_string(),
            hash.to_string(),
            TelegramApiOrigin::PublisherInject,
        )
    })
}

/// True when either a keychain override or a publisher pair is present.
#[must_use]
pub fn telegram_api_available(vault: &dyn TelegramSecretVault, source: &TelegramApiSource) -> bool {
    resolve_telegram_api(vault, source).is_some()
}

pub(super) fn require_resolved_api(
    vault: &dyn TelegramSecretVault,
    source: &TelegramApiSource,
) -> Result<(String, String), AdapterError> {
    resolve_telegram_api(vault, source)
        .map(|(id, hash, _)| (id, hash))
        .ok_or(AdapterError::Unavailable {
            protocol: ProtocolId::Telegram,
            reason: "telegram api credentials are missing; set a keychain override or rebuild with TELEGRAM_API_ID",
        })
}

pub(super) fn parse_resolved_api_id(id: &str) -> Result<i32, AdapterError> {
    id.parse::<i32>().map_err(|_| AdapterError::Unavailable {
        protocol: ProtocolId::Telegram,
        reason: "telegram api_id must be a number",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::MemorySecretVault;

    #[test]
    fn override_wins_over_publisher() {
        let vault = MemorySecretVault::new();
        vault.set_secret(TelegramSecretKey::ApiId, "11111");
        vault.set_secret(TelegramSecretKey::ApiHash, "override-hash");
        let source = TelegramApiSource::with_publisher("99999", "publisher-hash");
        let (id, hash, origin) = resolve_telegram_api(&vault, &source).expect("pair");
        assert_eq!(id, "11111");
        assert_eq!(hash, "override-hash");
        assert_eq!(origin, TelegramApiOrigin::KeychainOverride);
        let debug = format!("{source:?} {origin:?}");
        assert!(debug.contains("publisher: true"));
        assert!(!debug.contains("99999"));
        assert!(!debug.contains("publisher-hash"));
        assert!(!debug.contains("override-hash"));
    }

    #[test]
    fn publisher_used_when_vault_empty() {
        let vault = MemorySecretVault::new();
        let source = TelegramApiSource::with_publisher("11111", "publisher-hash");
        let (id, hash, origin) = resolve_telegram_api(&vault, &source).expect("pair");
        assert_eq!(id, "11111");
        assert_eq!(hash, "publisher-hash");
        assert_eq!(origin, TelegramApiOrigin::PublisherInject);
    }

    #[test]
    fn missing_both_is_none() {
        let vault = MemorySecretVault::new();
        let source = TelegramApiSource::empty();
        assert!(resolve_telegram_api(&vault, &source).is_none());
        assert!(!telegram_api_available(&vault, &source));
        assert!(!source.has_publisher());
    }

    #[test]
    fn from_build_debug_never_prints_values() {
        let source = TelegramApiSource::from_build();
        let debug = format!("{source:?}");
        assert!(debug.contains("TelegramApiSource"));
        assert!(!debug.contains("TELEGRAM_API"));
        assert!(!debug.to_ascii_lowercase().contains("api_hash"));
    }

    /// Public CI and pull_request workflows must not receive publisher secrets.
    /// Main-only `os-zips.yml` is the inject path and must fail closed.
    #[test]
    fn public_ci_workflows_do_not_receive_publisher_secrets() {
        let ci = include_str!("../../../../.github/workflows/ci.yml");
        let release = include_str!("../../../../.github/workflows/release-tag.yml");
        let claude = include_str!("../../../../.github/workflows/claude.yml");
        let review = include_str!("../../../../.github/workflows/claude-code-review.yml");
        for (name, src) in [
            ("ci.yml", ci),
            ("release-tag.yml", release),
            ("claude.yml", claude),
            ("claude-code-review.yml", review),
        ] {
            assert!(
                !src.contains("TELEGRAM_API"),
                "{name} must not mention publisher Telegram credentials"
            );
            assert!(
                !src.contains("secrets.TELEGRAM"),
                "{name} must not pass Telegram repository secrets"
            );
        }

        let os_zips = include_str!("../../../../.github/workflows/os-zips.yml");
        assert!(
            !os_zips.contains("pull_request:"),
            "os-zips must not run on pull_request"
        );
        assert!(os_zips.contains("secrets.TELEGRAM_API_ID"));
        assert!(os_zips.contains("secrets.TELEGRAM_API_HASH"));
        assert!(os_zips.contains("--features telegram-tdlib"));
        assert!(os_zips.contains(
            "Official OS zip refused: set repository secrets TELEGRAM_API_ID and TELEGRAM_API_HASH."
        ));
        assert!(
            !os_zips.contains("echo \"$TELEGRAM_API"),
            "os-zips must not print publisher credentials"
        );
        assert!(
            !os_zips.contains("echo ${TELEGRAM_API"),
            "os-zips must not print publisher credentials"
        );
        assert!(
            os_zips.contains("Never add these secrets"),
            "os-zips must warn against copying secrets into public CI"
        );
        assert!(
            os_zips.contains("scripts/stage-os-artifact.sh"),
            "os-zips must stage the payload with scripts/stage-os-artifact.sh"
        );
        assert!(
            os_zips.contains("path: dist/"),
            "upload-artifact must upload the payload directory"
        );
        assert!(
            !os_zips.contains("make_archive"),
            "upload-artifact is the only zip layer"
        );
        let stage = include_str!("../../../../scripts/stage-os-artifact.sh");
        assert!(
            !stage.contains("make_archive"),
            "the stage script must not pre-zip the payload"
        );
        assert!(
            stage.contains("third_party/tdlib/LICENSE_1_0.txt"),
            "os-zips must copy the vendored TDLib Boost license into the payload"
        );
        assert!(
            stage.contains("dist/THIRD_PARTY_NOTICES"),
            "os-zips must place third-party notices in the payload tree"
        );
        assert!(
            stage.contains("/usr/share/doc/"),
            "Linux payloads must attach the LLVM runtime package copyright files"
        );
        assert!(
            stage.contains("/usr/share/common-licenses/Apache-2.0"),
            "Linux LLVM notices must include the Apache-2.0 text the copyright file cites"
        );
        assert!(
            stage.contains("patchelf --set-rpath '$ORIGIN' \"$so\""),
            "copied LLVM runtimes must get an ORIGIN rpath"
        );
        assert!(
            stage.contains("Thinwire.app"),
            "macOS payload must be an app bundle"
        );
        assert!(stage.contains("Contents/MacOS/thinwire"));
        assert!(stage.contains("dev.jaysonsantos.thinwire"));
        assert!(stage.contains("CFBundleExecutable"));
        assert!(stage.contains("CFBundlePackageType"));
        assert!(stage.contains("<string>APPL</string>"));
        assert!(stage.contains("NSHighResolutionCapable"));
        assert!(stage.contains("<string>11.0</string>"));
        assert!(stage.contains("Contents/Resources"));
        assert!(
            !os_zips.contains("tr -d"),
            "TELEGRAM_API_ID check must not strip embedded whitespace"
        );
        assert!(
            os_zips.contains("^[0-9]+$"),
            "TELEGRAM_API_ID must be validated as digits after edge trim"
        );
    }

    #[test]
    fn os_zip_vendored_tdlib_notice_is_boost_license() {
        let notice = include_str!("../../../../third_party/tdlib/LICENSE_1_0.txt");
        assert!(
            notice.starts_with("Boost Software License - Version 1.0 - August 17th, 2003\n"),
            "TDLib notice must be the upstream Boost license, not a summary"
        );
        assert!(
            notice.contains("must be included in all copies of the Software, in whole or in part")
        );
    }
}
