//! Telegram auth state machine. Reads secrets from the vault; never logs them.

use super::credentials::{parse_resolved_api_id, require_resolved_api};
use crate::adapter::{AdapterError, ProtocolId, TelegramAuthPhase, TelegramAuthStep};
use crate::secrets::{TelegramSecretKey, TelegramSecretVault};
use crate::telegram::credentials::TelegramApiSource;

/// Pure login stepper. Default builds emit the next UI phase; the live TDLib
/// client (feature `telegram-tdlib`) consumes the same steps off this thread.
#[derive(Debug, Default)]
pub struct TelegramAuthEngine {
    last_phase: Option<TelegramAuthPhase>,
}

impl TelegramAuthEngine {
    #[must_use]
    pub const fn new() -> Self {
        Self { last_phase: None }
    }

    #[must_use]
    pub const fn last_phase(&self) -> Option<TelegramAuthPhase> {
        self.last_phase
    }

    pub fn reset(&mut self) {
        self.last_phase = None;
    }

    /// Validate the vault / publisher pair for `step` and return the next UI phase.
    ///
    /// When TDLib is compiled in, `TwoFactor` stays on the current screen
    /// (`NeedTwoFactor`) until the client reports [`TelegramAuthPhase::Ready`].
    /// Default builds finish with [`TelegramAuthPhase::Unavailable`].
    pub fn submit(
        &mut self,
        step: TelegramAuthStep,
        vault: &dyn TelegramSecretVault,
        source: &TelegramApiSource,
    ) -> Result<TelegramAuthPhase, AdapterError> {
        let phase = match step {
            TelegramAuthStep::ApiCredentials => {
                let (api_id, _) = require_resolved_api(vault, source)?;
                parse_resolved_api_id(&api_id)?;
                TelegramAuthPhase::NeedPhone
            }
            TelegramAuthStep::Phone => {
                require_resolved_api(vault, source)?;
                require(vault, TelegramSecretKey::Phone)?;
                TelegramAuthPhase::NeedCode
            }
            TelegramAuthStep::Code => {
                require(vault, TelegramSecretKey::Code)?;
                TelegramAuthPhase::NeedTwoFactor
            }
            // A new code for the same phone: the code step stays.
            TelegramAuthStep::ResendCode => {
                require_resolved_api(vault, source)?;
                TelegramAuthPhase::NeedCode
            }
            TelegramAuthStep::TwoFactor | TelegramAuthStep::Complete => {
                if crate::telegram::uses_tdlib_hook() {
                    TelegramAuthPhase::NeedTwoFactor
                } else {
                    TelegramAuthPhase::Unavailable
                }
            }
        };
        self.last_phase = Some(phase);
        Ok(phase)
    }
}

fn require(
    vault: &dyn TelegramSecretVault,
    key: TelegramSecretKey,
) -> Result<String, AdapterError> {
    vault
        .get_secret(key)
        .filter(|value| !value.is_empty())
        .ok_or(AdapterError::Unavailable {
            protocol: ProtocolId::Telegram,
            reason: missing_reason(key),
        })
}

const fn missing_reason(key: TelegramSecretKey) -> &'static str {
    match key {
        TelegramSecretKey::ApiId => "telegram api_id is missing from the secret store",
        TelegramSecretKey::ApiHash => "telegram api_hash is missing from the secret store",
        TelegramSecretKey::Phone => "telegram phone is missing from the secret store",
        TelegramSecretKey::Code => "telegram login code is missing from the secret store",
        TelegramSecretKey::Password => "telegram 2fa password is missing from the secret store",
        TelegramSecretKey::Session => "telegram session is missing from the secret store",
        TelegramSecretKey::DbEncryption => {
            "telegram database encryption key is missing from the secret store"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::MemorySecretVault;

    fn filled_vault() -> MemorySecretVault {
        let vault = MemorySecretVault::new();
        vault.set_secret(TelegramSecretKey::ApiId, "11111");
        vault.set_secret(TelegramSecretKey::ApiHash, "hash-value");
        vault.set_secret(TelegramSecretKey::Phone, "+15551234567");
        vault.set_secret(TelegramSecretKey::Code, "12345");
        vault.set_secret(TelegramSecretKey::Password, "2fa-secret");
        vault
    }

    #[test]
    fn missing_api_credentials_do_not_advance() {
        let vault = MemorySecretVault::new();
        let source = TelegramApiSource::empty();
        let mut engine = TelegramAuthEngine::new();
        let err = engine
            .submit(TelegramAuthStep::ApiCredentials, &vault, &source)
            .expect_err("missing api");
        assert!(matches!(
            err,
            AdapterError::Unavailable {
                protocol: ProtocolId::Telegram,
                ..
            }
        ));
        assert!(err.to_string().contains("TELEGRAM_API_ID"));
        assert_eq!(engine.last_phase(), None);
    }

    #[test]
    fn publisher_inject_does_not_need_vault_api_keys() {
        let vault = MemorySecretVault::new();
        vault.set_secret(TelegramSecretKey::Phone, "+15551234567");
        let source = TelegramApiSource::with_publisher("11111", "publisher-hash");
        let mut engine = TelegramAuthEngine::new();
        assert_eq!(
            engine
                .submit(TelegramAuthStep::ApiCredentials, &vault, &source)
                .expect("api"),
            TelegramAuthPhase::NeedPhone
        );
        assert_eq!(
            engine
                .submit(TelegramAuthStep::Phone, &vault, &source)
                .expect("phone"),
            TelegramAuthPhase::NeedCode
        );
        let debug = format!("{source:?}");
        assert!(!debug.contains("publisher-hash"));
        assert!(!debug.contains("11111"));
    }

    #[test]
    fn non_numeric_api_id_is_rejected_without_echoing_the_value() {
        let vault = MemorySecretVault::new();
        vault.set_secret(TelegramSecretKey::ApiId, "not-a-number");
        vault.set_secret(TelegramSecretKey::ApiHash, "hash-value");
        let mut engine = TelegramAuthEngine::new();
        let err = engine
            .submit(
                TelegramAuthStep::ApiCredentials,
                &vault,
                &TelegramApiSource::empty(),
            )
            .expect_err("bad api_id");
        assert!(err.to_string().contains("must be a number"));
        assert!(!err.to_string().contains("not-a-number"));
        assert!(!err.to_string().contains("hash-value"));
    }

    #[test]
    fn default_build_walks_phone_code_2fa_then_unavailable() {
        let vault = filled_vault();
        let source = TelegramApiSource::empty();
        let mut engine = TelegramAuthEngine::new();
        assert_eq!(
            engine
                .submit(TelegramAuthStep::ApiCredentials, &vault, &source)
                .expect("api"),
            TelegramAuthPhase::NeedPhone
        );
        assert_eq!(
            engine
                .submit(TelegramAuthStep::Phone, &vault, &source)
                .expect("phone"),
            TelegramAuthPhase::NeedCode
        );
        assert_eq!(
            engine
                .submit(TelegramAuthStep::Code, &vault, &source)
                .expect("code"),
            TelegramAuthPhase::NeedTwoFactor
        );
        let finish = engine
            .submit(TelegramAuthStep::TwoFactor, &vault, &source)
            .expect("2fa");
        if crate::telegram::uses_tdlib_hook() {
            assert_eq!(finish, TelegramAuthPhase::NeedTwoFactor);
        } else {
            assert_eq!(finish, TelegramAuthPhase::Unavailable);
        }
    }

    #[test]
    fn commands_and_phases_debug_omit_secret_values() {
        let step = TelegramAuthStep::ApiCredentials;
        let phase = TelegramAuthPhase::NeedPhone;
        let debug = format!("{step:?} {phase:?}");
        assert!(debug.contains("ApiCredentials"));
        assert!(debug.contains("NeedPhone"));
        assert!(!debug.contains("11111"));
        assert!(!debug.contains("hash"));
        assert!(!debug.contains("+1555"));
    }
}
