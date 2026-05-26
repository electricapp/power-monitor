# Release checklist

Two artifacts ship independently — the Rust crate and the Swift menu bar app.

## 1. Rust crate → crates.io

Scaffolded:

- `Cargo.toml` has `readme`, `license`, `keywords`, `categories`, `exclude`.
- Repository URL is commented out; uncomment after moving to the public repo.
- `CHANGELOG.md` has a stamped `[0.1.0]` section.
- `release-crate.sh` runs the full pre-flight.

Run locally (no account needed):

```bash
./release-crate.sh
```

That runs `cargo test`, builds docs, lists the publishable tarball, and
does `cargo publish --dry-run`. If it finishes clean, you're ready.

Manual steps — require a crates.io account:

1. Uncomment + set `repository` / `homepage` in `Cargo.toml` once the
   public repo exists.
2. `cargo login <token>` — token from <https://crates.io/me>.
3. `cargo publish` — immutable.
4. `git tag v0.1.0 && git push --tags` (in the final repo).

Post-publish:

- Crate page at `https://crates.io/crates/power-monitor`.
- Docs at `https://docs.rs/power-monitor` (builds in ~15 min; already
  pinned to `aarch64-apple-darwin` via `[package.metadata.docs.rs]`).

## 2. Swift menu bar app → GitHub release (unsigned)

Scaffolded:

- `app/build.sh` — strict-lint swiftc build into `.app` bundle.
- `app/release-app.sh` — runs the build, strips xattrs, zips with `ditto`,
  prints size + sha256 + the `gh release create` command.
- `app/release-notes.md` — release-notes template with install +
  quarantine-removal instructions for downloaders.

Run:

```bash
cd app && ./release-app.sh
```

Produces `app/build/PowerMonitorMenuBar-v0.1.0-macos-arm64.zip`.

Manual — requires a GitHub repo:

```bash
gh release create v0.1.0 \
    --title "v0.1.0" \
    --notes-file app/release-notes.md \
    app/build/PowerMonitorMenuBar-v0.1.0-macos-arm64.zip
```

Downloaders on the other end run:

```bash
xattr -dr com.apple.quarantine /Applications/PowerMonitorMenuBar.app
```

…to clear Gatekeeper's quarantine bit on the unsigned bundle.

## 3. Swift app → signed + notarized (future)

Scaffolded:

- `app/notarize.sh` — wraps `codesign --options runtime` + `notarytool
submit --wait` + `stapler staple`. Fails fast if signing identity /
  notary profile aren't configured.

Requires (one-time):

- Apple Developer Program enrollment ($99/yr).
- Developer ID Application certificate (Xcode → Settings → Accounts).
- Notary profile: `xcrun notarytool store-credentials notary-profile
--apple-id … --team-id … --password <app-specific-pw>`.
- Env vars: `CODESIGN_IDENTITY="Developer ID Application: ORG (TEAMID)"`,
  `NOTARY_PROFILE=notary-profile`.

Run:

```bash
cd app && ./notarize.sh
```

Output replaces the unsigned zip — downloaders no longer need the
`xattr` workaround.

## 4. Homebrew cask (after signed release)

Scaffolded:

- `homebrew/power-monitor.rb` — cask template with `TODO` markers for
  the signed-release URL and sha256.

Workflow:

1. Signed release uploaded to GitHub releases.
2. Paste the signed `sha256` (printed by `notarize.sh`) into
   `homebrew/power-monitor.rb`, replace `url "about:blank"` with the
   release asset URL, uncomment `homepage`.
3. Fork `homebrew/homebrew-cask`, drop the file at
   `Casks/p/power-monitor.rb`, open a PR.
4. After merge: `brew install --cask power-monitor`.

## What's intentionally NOT scaffolded

- CI workflow (`.github/workflows/*`) — will add when the repo is in
  its final home.
- `.crates-io` publish token — lives on the release machine, never in
  source control.
- Signing identity / notary profile — machine-local credentials, never
  in source control.
