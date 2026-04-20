# claude-meter

Live Claude plan usage (5-hour + 7-day windows) in your macOS menu bar.

![menu bar](https://img.shields.io/badge/macOS-menu%20bar-000?logo=apple) ![license](https://img.shields.io/badge/license-MIT-blue) [![release](https://img.shields.io/github/v/release/m13v/claude-meter)](https://github.com/m13v/claude-meter/releases/latest)

Anthropic's Pro / Max plans run on a rolling 5-hour window plus a weekly quota, with metered "extra usage" billing on top. The only place to see where you stand is `claude.ai/settings/usage`. `claude-meter` shows those numbers live in your menu bar and as a CLI.

## Install

Pick one of the two routes below.

### Route A: Menu-bar app + browser extension (recommended, no keychain prompt)

```
brew install --cask m13v/tap/claude-meter
```

Then load the extension in each browser you use with Claude (Chrome, Arc, Brave, Edge):

1. Clone this repo: `git clone https://github.com/m13v/claude-meter`
2. Open `chrome://extensions` (or `arc://extensions`, etc.)
3. Enable **Developer mode** (top right)
4. **Load unpacked** → select the `extension/` folder
5. Pin the ClaudeMeter icon if you want the popup

The extension fetches your usage using your existing `claude.ai` cookies and pushes it to the menu-bar app over `localhost:63762`. No passwords, no keychain prompts, no new logins.

### Route B: Menu-bar app only (keychain password)

```
brew install --cask m13v/tap/claude-meter
```

On first launch, macOS asks:

> **ClaudeMeter wants to use the confidential information stored in "Chrome Safe Storage" in your keychain.**

Click **Always Allow**. The app then reads your Chrome cookie database directly and decrypts the session cookie. Works without the extension, but the prompt is broad because "Chrome Safe Storage" is Chrome's master key for cookies, saved passwords, and credit cards. Click Deny → the app has no data source and shows `!`.

## Privacy

- No telemetry, no analytics, no network egress beyond `claude.ai` itself.
- All cookies, tokens, and API responses stay on your machine.
- The menu-bar app's bridge (`127.0.0.1:63762`) is localhost-only.
- Open source (MIT), audit it.

## CLI

The brew cask also installs a `claude-meter` CLI next to the app:

```
/Applications/ClaudeMeter.app/Contents/MacOS/claude-meter
/Applications/ClaudeMeter.app/Contents/MacOS/claude-meter --json
```

Prints the same data as the menu bar, one-shot, machine-readable with `--json`.

## How it works

1. **Via extension** (Route A): the extension runs every 60 seconds, fetches `/api/organizations/{uuid}/usage` with your logged-in cookies, and POSTs the snapshot to the menu-bar app over localhost.
2. **Via keychain** (Route B): the menu-bar app shells out to `security find-generic-password` to get Chrome's Safe Storage AES key, locates the profile logged into `claude.ai`, decrypts its cookies, and calls the same endpoints itself (Chromium-family browsers only).

The menu-bar app identifies which browser sent each POST by looking up the peer TCP socket's owning process, so Chrome and Arc get labeled correctly without any user configuration.

## Limitations

- macOS only. Linux/Windows not planned.
- Safari uses `.binarycookies` under Full Disk Access; not supported yet.
- Endpoints are undocumented; Anthropic can change them at any time.
- Session cookies expire; log back into `claude.ai` in your browser and the extension picks it up.

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
