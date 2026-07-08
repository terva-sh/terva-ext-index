# terva-ext-index dev tasks. Run `just` to list.
#
# This is a Rust extension built from source: run.sh compiles the release binary
# on first launch (and `just install` copies a prebuilt one into the install
# dir). Build/test/lint recipes all use the --release profile so the slow
# tree-sitter grammar C compile is shared across them rather than rebuilt once
# per profile.
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

# Optional maintainer-only release plumbing; the repo works fine without it.
import? 'release.just'

# List recipes.
default:
    @just --list

# Build the release binary, the way run.sh does (--locked = pinned Cargo.lock).
build:
    cargo build --release --locked
    @echo "built target/release/terva-ext-index"

# Run the tests (outline fixtures, sandbox/jail, and the wire protocol smoke test).
test *ARGS:
    cargo test --release {{ARGS}}

# Formatting + lint gate: rustfmt check and clippy (warnings are errors).
lint:
    cargo fmt --check
    cargo clippy --release --all-targets -- -D warnings

# Format all sources.
fmt:
    cargo fmt

# Build, then (re)install into terva's extensions dir so the latest code loads.
install: build
    #!/usr/bin/env bash
    set -euo pipefail
    # terva (>= v0.109.1) installs under the manifest NAME ("index"), not the
    # source directory basename — so that's the name `ext remove`/`ext list`
    # key on (the just-built binary below is still copied in by its repo name).
    name="index"
    terva ext remove "$name" -y >/dev/null 2>&1 || true
    terva ext install .
    # Install dir is the last column of `ext list`; the path can contain spaces,
    # so take everything from the first '/'.
    line="$(terva ext list | grep -E "/${name}\$" || true)"
    [[ -n "$line" ]] || { echo "install: could not find $name in 'terva ext list'" >&2; exit 1; }
    dir="/${line#*/}"
    # `ext install` skips the .gitignore'd target/, and run.sh would otherwise do
    # a cold from-source build on first launch. Copy our just-built release
    # binary in so run.sh sees it's current and execs it immediately — the Rust
    # analog of the Go siblings' copy-the-binary step.
    mkdir -p "$dir/target/release"
    cp -f target/release/terva-ext-index "$dir/target/release/terva-ext-index"
    echo "installed -> $dir"
    terva ext list

# Build, then load this dir into a one-off terva session (DIR = the session cwd).
try DIR=".": build
    terva --ext . --cwd "{{DIR}}"

# Pre-push gate: formatting, lint, and the full locked test suite (mirrored in CI).
ci: lint
    cargo test --release --locked

# Print the crate version (kept in lockstep by the version_matches_manifests test).
version:
    @grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"(.*)".*/\1/'

# Remove build output.
clean:
    cargo clean
