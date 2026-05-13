# claude-meter — project instructions

## Releasing a new version

**A release is NOT complete until a signed, notarized DMG is uploaded to a GitHub release for the new tag.** The download endpoint at `claude-meter.com/api/download` resolves the latest `.dmg` asset from `api.github.com/repos/m13v/claude-meter/releases/latest`. Skipping the GitHub release step means every email-gated download link keeps serving the previous version, even after the new tag is pushed. This bit us on v0.3.0 (2026-05-13): the website served 0.2.4 because the new tag had no DMG asset.

### The full pipeline (do all of it, in order)

1. **Bump version** in `Cargo.toml` (and let `cargo check` update `Cargo.lock`).
2. **Commit** the bump: `git commit -m "chore: release v<VERSION>"`.
3. **Tag**: `git tag v<VERSION>`.
4. **Push branch + tag**: `git push origin main && git push origin v<VERSION>`.
5. **Run `bash scripts/release.sh`.** This builds, codesigns (Developer ID), notarizes, staples, builds the DMG, notarizes the DMG, AND publishes the GitHub release with the DMG + zip uploaded (step 8/8 in the script as of 2026-05-13).
6. **Verify**: `curl -s https://api.github.com/repos/m13v/claude-meter/releases/latest | jq -r '.tag_name + " — " + (.assets | map(.name) | join(", "))'` must show the new tag and a `.dmg` asset.

### Required toolchain on PATH

`scripts/release.sh` needs the following binaries on `PATH`:

- `cargo` (from `~/.cargo/bin`)
- `rsvg-convert` (from `librsvg`, in `/opt/homebrew/bin`)
- `iconutil` (Xcode CLT)
- `codesign`, `xcrun notarytool`, `xcrun stapler`, `hdiutil` (Xcode CLT)
- `gh` (GitHub CLI, in `/opt/homebrew/bin`)

If launching from a non-login shell (e.g., a Claude Code bash sandbox), prepend `export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$PATH"` before invoking the script.

### Notarization profile

The script uses keychain profile `claude-meter-notary` for `xcrun notarytool`. If it's missing, recreate it with `xcrun notarytool store-credentials claude-meter-notary --apple-id … --team-id S6DP5HF77G --password <app-specific-password>`.

### Skip flags (only for debugging)

- `SKIP_DMG_NOTARIZE=1` — sign the DMG but skip its notarization (zip is still notarized + stapled). Don't use for real releases.
- `SKIP_GH_RELEASE=1` — skip the GitHub release publish step. Don't use for real releases; the website will serve the stale version.

### Homebrew tap

`brew install --cask m13v/tap/claude-meter` points at a separate `homebrew-tap` repo. If the cask formula has the version pinned, it needs a bump there too. The website download flow does NOT use brew; it pulls the DMG straight from the GitHub release.
