#!/usr/bin/env bash
# index extension launcher.
#
# terva runs an extension by executing the manifest's `exec` verbatim — it never
# compiles Rust. This wrapper provides the binary, in order of preference:
#   1. an up-to-date build already on disk (target/release/terva-ext-index);
#   2. a fresh `cargo build` from source, when a Rust toolchain is present;
#   3. the prebuilt binary from the LATEST GitHub release, when cargo isn't
#      available or the build fails.
# Then it execs it. So `terva ext install <path|git-url>` works on any host with
# a Rust toolchain — and, via the download fallback, on hosts without one.
#
# IMPORTANT: stdout is the protocol wire. Every byte of build/download chatter
# must go to stderr (terva captures it to $TERVA_HOME/logs/ext-index.log); a
# stray stdout write corrupts the JSON stream.
set -euo pipefail
cd "$(dirname "$0")"

bin="target/release/terva-ext-index"

# Where the download fallback fetches prebuilt binaries from. It always pulls the
# LATEST release (GitHub's /releases/latest/download/<asset> redirect), not a
# version-pinned one. Override for a private mirror or for testing (e.g. a
# file:// base). Assets are named by Rust target triple, matching
# .github/workflows/release.yml.
RELEASE_BASE="${TERVA_EXT_INDEX_RELEASE_BASE:-https://github.com/terva-sh/terva-ext-index/releases}"

needs_build() {
	[ -x "$bin" ] || return 0
	# Rebuild if any source or the manifest is newer than the binary.
	if [ -n "$(find src -name '*.rs' -newer "$bin" -print -quit 2>/dev/null)" ]; then
		return 0
	fi
	if [ Cargo.toml -nt "$bin" ]; then
		return 0
	fi
	if [ -f Cargo.lock ] && [ Cargo.lock -nt "$bin" ]; then
		return 0
	fi
	return 1
}

# Map this host to the Rust target triple used in the release asset names.
host_triple() {
	case "$(uname -s)/$(uname -m)" in
		Linux/x86_64) echo x86_64-unknown-linux-gnu ;;
		Linux/aarch64 | Linux/arm64) echo aarch64-unknown-linux-gnu ;;
		Darwin/arm64 | Darwin/aarch64) echo aarch64-apple-darwin ;;
		*) return 1 ;;
	esac
}

# Download the latest prebuilt binary for this platform, verify its sha256, and
# install it at $bin. Leaves no partial binary and returns non-zero on any
# failure, so the caller can report a clear error.
download_binary() {
	local triple url sha_url tmp sha actual get
	triple=$(host_triple) || {
		echo "[index] no prebuilt binary for $(uname -s)/$(uname -m)." >&2
		return 1
	}
	url="$RELEASE_BASE/latest/download/terva-ext-index-${triple}"
	sha_url="${url}.sha256"

	if command -v curl >/dev/null 2>&1; then
		get=curl
	elif command -v wget >/dev/null 2>&1; then
		get=wget
	else
		echo "[index] need curl or wget to download a prebuilt binary." >&2
		return 1
	fi

	echo "[index] downloading latest prebuilt terva-ext-index-${triple}…" >&2
	tmp=$(mktemp "${TMPDIR:-/tmp}/terva-ext-index.XXXXXX")
	if [ "$get" = curl ]; then
		curl -fsSL "$url" -o "$tmp" || { echo "[index] download failed: $url" >&2; rm -f "$tmp"; return 1; }
		sha=$(curl -fsSL "$sha_url" 2>/dev/null | awk '{print $1}' || true)
	else
		wget -qO "$tmp" "$url" || { echo "[index] download failed: $url" >&2; rm -f "$tmp"; return 1; }
		sha=$(wget -qO- "$sha_url" 2>/dev/null | awk '{print $1}' || true)
	fi

	# Verify against the published checksum (our releases always ship one).
	if [ -n "$sha" ]; then
		if command -v sha256sum >/dev/null 2>&1; then
			actual=$(sha256sum "$tmp" | awk '{print $1}')
		elif command -v shasum >/dev/null 2>&1; then
			actual=$(shasum -a 256 "$tmp" | awk '{print $1}')
		else
			actual=""
		fi
		if [ -n "$actual" ] && [ "$actual" != "$sha" ]; then
			echo "[index] checksum mismatch — refusing binary (want ${sha:0:12}…, got ${actual:0:12}…)." >&2
			rm -f "$tmp"
			return 1
		fi
		[ -n "$actual" ] && echo "[index] sha256 verified." >&2
	else
		echo "[index] warning: no published checksum found; binary not verified." >&2
	fi

	mkdir -p "$(dirname "$bin")"
	chmod +x "$tmp"
	mv -f "$tmp" "$bin"
	echo "[index] installed latest prebuilt -> $bin" >&2
}

if needs_build; then
	if command -v cargo >/dev/null 2>&1; then
		echo "[index] building $bin (first launch or sources changed)…" >&2
		# A C compiler is also needed to build the tree-sitter grammars; cargo
		# surfaces a clear error if cc is missing. --locked: build only from the
		# audited, pinned Cargo.lock.
		if cargo build --release --locked >&2; then
			echo "[index] build complete." >&2
		else
			echo "[index] build failed; falling back to a prebuilt download." >&2
			download_binary || { echo "[index] could not build or download a binary (see above)." >&2; exit 1; }
		fi
	else
		echo "[index] Rust/cargo not found; trying a prebuilt download." >&2
		if ! download_binary; then
			echo "[index] no toolchain and no usable prebuilt binary." >&2
			echo "[index] Install Rust (https://rustup.rs) to build from source, or check network/platform." >&2
			exit 1
		fi
	fi
fi

exec "$bin" "$@"
