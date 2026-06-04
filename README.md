# claude-meter

Live Claude plan usage (5-hour + 7-day windows) in your macOS menu bar.

![menu bar](https://img.shields.io/badge/macOS-menu%20bar-000?logo=apple) ![license](https://img.shields.io/badge/license-MIT-blue) [![release](https://img.shields.io/github/v/release/m13v/claude-meter)](https://github.com/m13v/claude-meter/releases/latest)

Anthropic's Pro / Max plans run on a rolling 5-hour window plus a weekly quota, with metered "extra usage" billing on top. The only place to see where you stand is `claude.ai/settings/usage`. `claude-meter` shows those numbers live in your menu bar and as a CLI.

## Requirements

`claude-meter` reads usage through the OAuth token that the [Claude Code](https://docs.anthropic.com/en/docs/claude-code) CLI already stores in your macOS Keychain. You need Claude Code installed and logged in (run `claude` once). No browser cookies, no extension, no separate login.

## Install

```
brew install --cask m13v/tap/claude-meter
```

That's it. The menu-bar app launches and starts showing your usage. On first launch macOS may ask to read the **`Claude Code-credentials`** keychain item; click **Always Allow**. That item is just Claude Code's OAuth token, not your browser's cookies or passwords.

## Privacy

- Usage data stays local: the OAuth token, account email, org ID, and Claude
  API responses are never sent to us.
- Anonymous health telemetry is enabled by default: crash reports and one
  `app_daily_active` ping per local day, keyed by a random install ID. The daily
  ping includes app version, platform, and local date only.
- Disable all telemetry with `CLAUDE_METER_NO_TELEMETRY=1`. Disable only crash
  reporting with `CLAUDE_METER_NO_SENTRY=1`.
- Aside from that anonymous telemetry, network egress is to `api.anthropic.com`
  (your own usage, Bearer-authed with the token you already have).
- Open source (MIT), audit it.

## CLI

The brew cask also installs a `claude-meter` CLI next to the app:

```
/Applications/ClaudeMeter.app/Contents/MacOS/claude-meter
/Applications/ClaudeMeter.app/Contents/MacOS/claude-meter --json
```

Prints the same data as the menu bar, one-shot, machine-readable with `--json`.

## How it works

1. Reads the Bearer token Claude Code stores in the Keychain (service
   `Claude Code-credentials`) via `/usr/bin/security`.
2. Calls `https://api.anthropic.com/api/oauth/usage` (the 5h / 7d windows plus
   extra-usage block) and `/api/oauth/profile` (account email + org uuid).
3. Polls adaptively, faster as you approach a limit, and paints the menu bar.

claude-meter never refreshes the token itself; the running Claude Code CLI
rotates it automatically. claude-meter just reads whatever is in the Keychain.

## Limitations

- macOS only. Linux/Windows not planned.
- Requires the Claude Code CLI logged in; if its token is expired, run `claude`
  once to refresh.
- `subscription_details` (next charge date, payment method) and the dedicated
  overage endpoint aren't reachable through OAuth scopes, so those fields are
  omitted.
- Endpoints are undocumented; Anthropic can change them at any time.

## Build from source

```
git clone https://github.com/m13v/claude-meter
cd claude-meter
cargo build --release
# CLI:  ./target/release/claude-meter
# App:  bash scripts/build-app.sh
```

## License

MIT. See [LICENSE](LICENSE).
