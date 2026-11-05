# 0014: More than one account for each protocol

**Status:** accepted (2026-11-05)

**Date:** 2026-11-05

**Refs:** ADR [0010](0010-frontend-independent-core.md), ADR [0013](0013-ship-all-protocols-agpl-helpers.md), ux F8

## Context

The owner asked on 2026-11-05: "allow to login more than one account on all of the services". Later: "do all services at once".

Before this ADR, thinwire had one account for each protocol. `ProtocolId` was an enum with one value for each protocol. The keychain names, the data folders, and the helper programs had one fixed name each. Add account was hidden while Telegram was signed in (ux F8).

## Decision

1. `ProtocolId` names one account. It holds a `ProtocolKind` and a slot number from 0 to `MAX_ACCOUNT_SLOT` (8). The old constants (`ProtocolId::Telegram` and the others) are account 0. Code that asks for a protocol uses `kind()` or `is(kind)`.
2. Account 0 keeps every old name. A user with one account sees no change on disk or in the keychain.
3. Another account `n` adds its number to each name:
    - Keychain: `name#n`, for example `telegram.session#1` and `discord.bot_token#1`. The Telegram `api_id` and `api_hash` name the app, not the account. All accounts share them.
    - Data folders: `tdlib-n`, `whatsapp-n`, `signal-n`.
    - Helpers: each WhatsApp or Signal account starts its own helper program with `--account n`. Each helper has its own lock file, its own store, and its own key (`whatsapp.store_key#n`, `signal.store_key#n`).
    - Key in files: `telegram#1`. The settings list the accounts after account 0 under `accounts`. The mute file uses the same key.
    - Label: "Telegram 2" for account 1. Account 0 is "Telegram".
4. The adapter host keeps one adapter for each account. `AddAccount` starts the adapter of an account, and `RemoveAccount` stops it. An adapter does not know its slot: the host gives it each command as account 0, and the host puts the account on each of its events. `ToAccount` and `FromAccount` carry the commands and events that have no protocol field, for example a Telegram login step.
5. The core builds the adapters of the accounts after account 0 with a factory. Telegram, Discord, and Slack accounts get their own `SecretStore` with the slot as scope. The app gives the core a `HelperBuilder` for the WhatsApp and Signal helpers, because the core does not depend on the helper programs.
6. One login screen is open for each protocol at a time. The state holds the account of each screen: `telegram_login_account`, `whatsapp_link_account`, `signal_link_account`. A login event of another account does not change the screen. When a login ends, the screen is for account 0 again.
7. A Telegram account that is not on the login screens resumes its saved session at start with no screen. Ready links its row. A request for a phone number, a code, or a password unlinks the row, and the row shows "Signed out. Click Link to sign in again."
8. A WhatsApp or Signal account after account 0 starts its helper at start if the user accepted the current gate copy. The acceptance is of the risk text, so all accounts share it. A cancel for another account does not clear the saved acceptance of account 0. A new Signal account shows the notice again, because its own helper needs the acceptance.
9. UI:
    - Add account stays on the rail. It opens a menu with each protocol of the build. A protocol with no helper program shows as disabled.
    - If account 0 of that protocol is free, the entry opens the login of account 0. Otherwise it adds the next free account and opens its login. ux F8 still holds for one account: Add account does not start a second login of a linked account.
    - A row after account 0 that is not linked shows Link and Remove. Remove forgets the account in the settings. Its keychain values and its data folder stay on disk.
10. The retained Signal history (old numbers) shows for account 0 only. Another Signal account drops its retained rows and their status lines.

## Consequences

- A user can add up to 9 accounts of each protocol.
- A second Discord account needs a second bot token in the keychain, under `discord.bot_token#n`.
- Each WhatsApp or Signal account is one more helper process.
- Remove does not delete data. To delete it, the user removes the folder and the keychain values by hand.
- The retained Signal history of an account after account 0 is on disk, but thinwire does not show it.
