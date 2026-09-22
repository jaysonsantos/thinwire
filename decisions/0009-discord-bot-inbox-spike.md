# Discord bot/OAuth inbox spike

**Status:** accepted (scaffold only)

## Context
Option B (`0001`) locks Discord v1 to a bot/OAuth inbox. User-account automation and personal DM clients are out. Telegram chat list and messages are still the product next step (`ROADMAP.md`). This beat only scaffolds the Discord seam so later inbox work has a crate, a token store, and a refusal path.

## Decision
Use **twilight** (`twilight-http` and `twilight-model`), behind Cargo feature `discord-bot`.

Why twilight, not serenity:

- The spike needs a bot HTTP client and gateway intent bits, not a command framework, cache, or voice stack. twilight splits those crates.
- `twilight_http::Client::new` is a bot-token client. It prefixes an unprefixed token with `Bot `. An OAuth install token is the `Bearer ` form documented by twilight. There is no user-account login API on this path.
- Serenity sits next to `serenity_self`, a fork whose purpose is user-account automation. That is the path this repo refuses. twilight does not ship that fork.
- Both crates are ISC, which the MIT binary can link. License fit is not the deciding factor.

Hard constraints:

- Feature `discord-bot` is off in default CI, `scripts/test.sh`, and the main OS zip workflow. Enabling it locally compiles the placeholder. `Client::new` runs on the tokio worker and does not open a gateway or send HTTP. twilight-http 0.17 does not select a rustls crypto provider, so the feature enables rustls `ring` for that constructor.
- Default UI hides Discord until that feature is compiled **and** Telegram has delivered a message. First-run and Add account stay Telegram only. No Discord auth form.
- The bot token or OAuth bearer token is keychain account `discord.bot_token` (service `thinwire`). UI reads and writes memory only. OS keychain I/O stays on `spawn_blocking`. The token never rides on `AdapterCommand`, never lands in git, and never is logged.
- Tokens prefixed `User ` or `mfa.` are refused. OAuth install scope is `bot` only. Gateway intents are guild inbox bits. Direct-message intents are not set.
- README keeps the three Critic risk bullets and does not market a personal Discord client.

## Consequences
A later beat can attach a gateway on the tokio worker without revisiting the crate choice or the user-token refusal. Until Telegram messages work, the shell does not show this inbox.

Rejected: serenity; a user-token or self-bot client; compiling twilight in default CI; a Discord login screen in this beat.
