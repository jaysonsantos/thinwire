# Slack official OAuth workspace spike

**Status:** spike (scaffold). Chat list + messages on TDLib stays next. Slack auth UI stays out until Telegram messages work.

## Context
Product lock (`0001`) names Slack as official OAuth, a workspace app, not a personal desktop clone. This beat only scaffolds that path. It does not install an app, open a socket, or show a Slack login screen.

## Decision
Use `slack-morphism` for official Web API / OAuth v2 types and the Socket Mode client config. Cargo feature `slack-oauth` on `thinwire-protocol` and `thinwire`. Default features stay empty. `ci.yml`, `scripts/test.sh`, `scripts/lint.sh`, and `os-zips.yml` do not enable the feature and do not set Slack secrets.

`slack-morphism` is built with its default features off. The `hyper` feature stays off in this spike, so the dependency does not open HTTP or a websocket.

### Install (local callback, not executed)
Workspace admin install uses Slack's OAuth v2 authorize URL:

`https://slack.com/oauth/v2/authorize`

Bot scopes only (`channels:history`, `channels:read`, `chat:write`, `groups:history`, `groups:read`, `im:history`, `im:read`, `mpim:history`, `mpim:read`, `users:read`). No `user_scope`. A user token would be a personal client.

Redirect URL to register on the Slack app:

`http://127.0.0.1:8976/slack/oauth/callback`

A later tokio worker (never the egui thread) binds that loopback port, checks `state`, and exchanges `code` with `oauth.v2.access`. This spike builds the authorize URL, parses the callback query, and — when `slack-oauth` is on — fills `SlackOAuthV2AccessTokenRequest`. It does not bind the port and does not call Slack.

### Events (Socket Mode plan, not connected)
After the bot token is stored, the same worker connects with `slack-morphism` Socket Mode and the publisher app-level token (`connections:write`). Socket Mode is the plan so the desktop app does not expose a public Events API URL. This spike constructs `SlackClientSocketModeConfig` and does not open a websocket. Turning on `slack-morphism`'s `hyper` feature is a later change, still behind `slack-oauth`, still off the UI thread.

### Secrets
Same pattern as Telegram publisher inject (`0007`):

| Name | Role |
| --- | --- |
| `SLACK_CLIENT_ID` | Compile-time publisher inject |
| `SLACK_CLIENT_SECRET` | Compile-time publisher inject |
| `SLACK_APP_TOKEN` | Compile-time publisher app-level token for Socket Mode |

Keychain override wins when both client id and client secret are present. The app-level token override is independent. Workspace bot token and team id are stored after install (`slack.bot_token`, `slack.team_id`). OAuth `code` and `state` are memory-only. OS service name is `thinwire` (same store as Telegram). UI thread stays memory-only; a future flush uses `spawn_blocking`. Never put these values on `AdapterCommand`, in git, in public CI, or in logs.

## Consequences
Default builds keep the existing Slack stub in the shell (not ready, no login). Enabling `slack-oauth` compiles types into the binary and still shows no Slack auth UI.

Rejected: reverse-engineered Slack Desktop; a personal user-token client; Slack secrets in the MIT tree or public CI; network I/O on the UI thread; enabling this feature in default CI.
