# claude-meter

Live Claude plan usage and extra-usage balance, from the terminal.

Anthropic's Pro, Max, and Max 20x plans run on a rolling 5-hour window plus a weekly quota, and once you blow through either, you're on metered "extra usage" billing. Today there's no official CLI, no documented API, no menu-bar indicator - the only place to see where you stand is `claude.ai/settings/usage`.

`claude-meter` reads the same numbers that page reads, prints them in your terminal in under a second, and runs headless (no browser window).

## What it shows

```
claude-meter
============
5-hour             0.0% used
7-day all         40.0% used    -> resets Fri Apr 24 10:00 (in 4d 6h)
7-day Sonnet       0.0% used
Extra usage      $202.14 / $200.00 (101%)  BLOCKED until Fri May 1
Next charge      2026-04-30   mastercard ..0936

fetched 2026-04-19 10:55:02 PDT   org d03d2d69-...
```

`--json` returns the full snapshot as structured JSON for scripting.

## Install

Requires Rust 1.85+ and, for now, macOS with Google Chrome.

```bash
git clone https://github.com/m13v/claude-meter
cd claude-meter
cargo install --path .
```

Then:

```bash
claude-meter
claude-meter --json
```

## How it works

1. Reads the Chrome Safe Storage AES key from the macOS Keychain.
2. Scans your Chrome profiles, finds the one logged into `claude.ai`, and decrypts the session cookies from that profile's SQLite cookie store.
3. Calls three undocumented endpoints, impersonating Chrome's TLS fingerprint so Cloudflare lets it through:
   - `/api/organizations/{uuid}/usage` - 5-hour and 7-day utilization per model family
   - `/api/organizations/{uuid}/overage_spend_limit` - extra-usage credits used, monthly cap, block state
   - `/api/organizations/{uuid}/subscription_details` - plan status and next charge

Your cookies, tokens, and responses never leave your machine.

## Requirements

- macOS
- Google Chrome with an active `claude.ai` session (you must have logged in)
- Rust 1.85+ to build from source

## Limitations (v0.1)

- macOS + Chrome only. Arc/Brave/Edge use the same Chromium cookie format and should work with a small path tweak; Firefox and Safari do not and are not planned for v0.1.
- Plan tier name (Pro / Max / Max 20x) is not returned by these endpoints. You get the utilization and the dollar balance, not the tier label.
- Relies on undocumented `claude.ai` internal endpoints. Anthropic can change or remove them at any time. There is no stable public API for subscription usage today (see Anthropic support issue [#40395](https://github.com/anthropics/claude-code/issues/40395), closed "not planned").
- Session cookies expire; if `claude-meter` starts returning auth errors, log into `claude.ai` in Chrome and retry.

## Why this exists

Since Anthropic introduced the "extra usage" model in April 2026 (third-party apps like Claude Code, Cursor, and Zed now draw from a metered pool that sits on top of your plan), people get blindsided by billing at runtime. The phrase `"third-party apps now draw from your extra usage, not your plan limits"` is one of the most-searched Anthropic error messages on Google. `claude-meter` is the first tool I've found that surfaces that number before the error.

This is the CLI layer. A macOS menu-bar app that polls on a schedule and notifies you when you're about to hit the wall is in progress.

## License

MIT
