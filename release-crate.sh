#!/bin/bash
# Pre-flight for publishing the Rust crate to crates.io.
#
# Does NOT publish. Does everything that doesn't require a crates.io token:
#   - verifies tests pass
#   - verifies docs build on the target platform
#   - shows the exact file list that would ship
#   - runs `cargo publish --dry-run` (validates manifest + builds from tarball)
#
# When the `repository` URL is filled in and you're ready:
#     cargo login <token>   # token from https://crates.io/me
#     cargo publish
set -euo pipefail

cd "$(dirname "$0")"

VERSION=$(grep -E '^version = ' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
RUST_VERSION=$(grep -E '^rust-version = ' Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
echo "=== power-monitor $VERSION pre-flight (rustc $RUST_VERSION) ==="

# Warnings-as-errors on every gate. Any regression fails the pre-flight.
export RUSTFLAGS="-D warnings"
export RUSTDOCFLAGS="-D warnings -D rustdoc::broken-intra-doc-links -D rustdoc::private-intra-doc-links -D rustdoc::invalid-codeblock-attributes"

echo
echo ">> cargo clippy --all-targets (warnings-as-errors)"
cargo clippy --all-targets -- -D warnings

echo
echo ">> cargo test --release"
cargo test --release

echo
echo ">> cargo doc --no-deps (rustdoc warnings-as-errors)"
cargo doc --no-deps

echo
echo ">> cargo package --list (first 40 lines)"
# --allow-dirty: pre-flight runs before the final repo commit; real publish
# still requires a clean tree by default.
cargo package --list --allow-dirty | head -40

echo
echo ">> cargo publish --dry-run"
cargo publish --dry-run --allow-dirty

echo
cat <<'EOF'
=== pre-flight OK ===

Remaining manual steps (require crates.io account):
  1. Fill in `repository` URL in Cargo.toml (currently commented out).
  2. `cargo login <token>`           — token from https://crates.io/me
  3. `cargo publish`                 — uploads; immutable once done.
  4. `git tag v<version> && git push --tags` (in the final repo).

Crate page will be live at https://crates.io/crates/power-monitor
Docs at https://docs.rs/power-monitor (builds within ~15 min).
EOF
