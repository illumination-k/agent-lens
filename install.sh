#!/usr/bin/env bash
# Install agent-lens from the GitHub Releases pre-built binaries.
#
# Flags, environment variables, and the invocation forms live in the
# `usage()` heredoc below — the single source of truth, so that
# `--help` works identically when the script is piped into bash.

set -euo pipefail

REPO="${AGENT_LENS_REPO:-illumination-k/agent-lens}"
TAG="${AGENT_LENS_TAG:-latest}"
INSTALL_DIR="${AGENT_LENS_DIR:-$HOME/.local/bin}"
VERIFY=1
if [ "${AGENT_LENS_NO_VERIFY:-0}" = "1" ]; then
	VERIFY=0
fi

# Printed by --help. Kept as a heredoc rather than sed'ing the header out
# of "$0": under the documented `curl … | bash -s -- --help` invocation
# "$0" is the bash binary, not this script.
usage() {
	cat <<'EOF'
Install agent-lens from the GitHub Releases pre-built binaries.

Usage:
  curl -fsSL https://raw.githubusercontent.com/illumination-k/agent-lens/main/install.sh | bash
  curl -fsSL https://raw.githubusercontent.com/illumination-k/agent-lens/main/install.sh | bash -s -- --tag rolling --dir "$HOME/.local/bin"

Options:
  --tag TAG         Release tag to install (default: latest stable release).
  --dir DIR         Install directory (default: $HOME/.local/bin).
  --repo OWNER/NAME GitHub repo (default: illumination-k/agent-lens).
  --no-verify       Skip SHA-256 verification of the downloaded archive.
                    Verification is mandatory otherwise: a missing
                    checksum asset or a missing sha256sum/shasum aborts
                    the install rather than proceeding unverified.
  -h, --help        Print this help and exit.

Environment variables:
  AGENT_LENS_TAG        Same as --tag.
  AGENT_LENS_DIR        Same as --dir.
  AGENT_LENS_REPO       Same as --repo.
  AGENT_LENS_NO_VERIFY  Set to 1 for the same effect as --no-verify.
EOF
}

while [ $# -gt 0 ]; do
	case "$1" in
	--tag)
		TAG="$2"
		shift 2
		;;
	--dir)
		INSTALL_DIR="$2"
		shift 2
		;;
	--repo)
		REPO="$2"
		shift 2
		;;
	--no-verify)
		VERIFY=0
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		printf 'unknown argument: %s\n' "$1" >&2
		exit 2
		;;
	esac
done

log() { printf '[install.sh] %s\n' "$*" >&2; }
err() {
	printf '[install.sh] error: %s\n' "$*" >&2
	exit 1
}

need() { command -v "$1" >/dev/null 2>&1 || err "missing required command: $1"; }

need uname
need mktemp
need tar

if command -v curl >/dev/null 2>&1; then
	FETCH="curl"
elif command -v wget >/dev/null 2>&1; then
	FETCH="wget"
else
	err "need either curl or wget"
fi

fetch() {
	url="$1"
	out="$2"
	if [ "$FETCH" = "curl" ]; then
		curl -fsSL --proto '=https' --tlsv1.2 --retry 3 -o "$out" "$url"
	else
		wget -q --https-only --secure-protocol=TLSv1_2 -O "$out" "$url"
	fi
}

detect_libc() {
	# Prefer musl on systems whose dynamic loader is provided by musl (Alpine,
	# Void musl, etc). Fall back to glibc otherwise. The loader probe is
	# arch-specific: an x86_64 musl loader on disk says nothing about an
	# aarch64 host (and vice versa), so only the loader matching "$1" counts.
	loader_arch="$1"
	if command -v ldd >/dev/null 2>&1 && ldd --version 2>&1 | grep -qi musl; then
		echo "musl"
	elif [ -n "$loader_arch" ] && [ -f "/lib/ld-musl-${loader_arch}.so.1" ]; then
		echo "musl"
	else
		echo "gnu"
	fi
}

detect_target() {
	os="$(uname -s)"
	arch="$(uname -m)"
	case "$os" in
	Linux)
		case "$arch" in
		x86_64 | amd64) loader_arch="x86_64" ;;
		aarch64 | arm64) loader_arch="aarch64" ;;
		*) loader_arch="" ;;
		esac
		libc="$(detect_libc "$loader_arch")"
		case "$arch" in
		x86_64 | amd64) echo "x86_64-unknown-linux-${libc}" ;;
		aarch64 | arm64) echo "aarch64-unknown-linux-${libc}" ;;
		*) err "unsupported Linux architecture: $arch (only x86_64 and aarch64 have pre-built binaries; build from source with cargo)" ;;
		esac
		;;
	Darwin)
		case "$arch" in
		arm64 | aarch64) echo "aarch64-apple-darwin" ;;
		x86_64) echo "x86_64-apple-darwin" ;;
		*) err "unsupported macOS architecture: $arch" ;;
		esac
		;;
	MINGW* | MSYS* | CYGWIN*)
		err "use install.sh from a POSIX shell only; on Windows, download the .zip from https://github.com/$REPO/releases manually (x86_64-pc-windows-msvc or aarch64-pc-windows-msvc)"
		;;
	*)
		err "unsupported OS: $os"
		;;
	esac
}

# Pull the digest for "$2" (the archive's basename) out of a
# `sha256sum`-style checksum file. Prefers the line naming the archive so
# a multi-asset checksum file can never hand back another asset's digest;
# falls back to a lone unnamed digest line (GNU coreutils and the
# PowerShell `Get-FileHash` output both round-trip through this).
expected_sha256() {
	expected_file="$1"
	archive_name="$2"
	awk -v want="$archive_name" '
		{ gsub(/\r$/, "") }
		$1 == "" { next }
		{
			name = $2
			sub(/^\*/, "", name)
			sub(/^.*\//, "", name)
			if (name == want) { print tolower($1); found = 1; exit }
			if (NF == 1 || name == "") { lone = tolower($1); lone_count++ }
			else { other++ }
		}
		END { if (!found && lone_count == 1 && other == 0) print lone }
	' "$expected_file"
}

verify_sha256() {
	archive="$1"
	expected_file="$2"
	archive_name="$3"
	expected="$(expected_sha256 "$expected_file" "$archive_name")"
	if [ -z "$expected" ]; then
		err "could not find a SHA-256 digest for ${archive_name} in $expected_file"
	fi
	if command -v sha256sum >/dev/null 2>&1; then
		actual="$(sha256sum "$archive" | awk '{print tolower($1)}')"
	elif command -v shasum >/dev/null 2>&1; then
		actual="$(shasum -a 256 "$archive" | awk '{print tolower($1)}')"
	else
		err "no sha256sum/shasum available to verify ${archive_name}; install one, or re-run with --no-verify to install without verification"
	fi
	if [ "$expected" != "$actual" ]; then
		err "SHA-256 mismatch: expected $expected, got $actual"
	fi
	log "SHA-256 verified"
}

TARGET="$(detect_target)"
ARCHIVE="agent-lens-${TARGET}.tar.gz"
if [ "${TAG}" = "latest" ]; then
	BASE_URL="https://github.com/${REPO}/releases/latest/download"
else
	BASE_URL="https://github.com/${REPO}/releases/download/${TAG}"
fi

log "repo:   ${REPO}"
log "tag:    ${TAG}"
log "target: ${TARGET}"
log "dest:   ${INSTALL_DIR}"

TMP="$(mktemp -d 2>/dev/null || mktemp -d -t agent-lens)"
trap 'rm -rf "$TMP"' EXIT

log "downloading ${BASE_URL}/${ARCHIVE}"
fetch "${BASE_URL}/${ARCHIVE}" "${TMP}/${ARCHIVE}"

if [ "$VERIFY" = "1" ]; then
	if fetch "${BASE_URL}/${ARCHIVE}.sha256" "${TMP}/${ARCHIVE}.sha256" 2>/dev/null; then
		verify_sha256 "${TMP}/${ARCHIVE}" "${TMP}/${ARCHIVE}.sha256" "${ARCHIVE}"
	else
		err "checksum asset ${ARCHIVE}.sha256 not published at ${BASE_URL}; refusing to install unverified (re-run with --no-verify to override)"
	fi
else
	log "checksum verification disabled (--no-verify); the archive is NOT verified"
fi

log "extracting"
tar -xzf "${TMP}/${ARCHIVE}" -C "${TMP}"

SRC="${TMP}/agent-lens-${TARGET}/agent-lens"
[ -f "$SRC" ] || err "expected binary not found in archive: agent-lens-${TARGET}/agent-lens"

mkdir -p "$INSTALL_DIR"
DEST="${INSTALL_DIR}/agent-lens"
install -m 0755 "$SRC" "$DEST" 2>/dev/null || {
	cp "$SRC" "$DEST"
	chmod 0755 "$DEST"
}

log "installed: ${DEST}"

case ":${PATH}:" in
*":${INSTALL_DIR}:"*) ;;
*)
	log "note: ${INSTALL_DIR} is not on your PATH; add it with:"
	log "      export PATH=\"${INSTALL_DIR}:\$PATH\""
	;;
esac

if "$DEST" --version >/dev/null 2>&1; then
	"$DEST" --version >&2 || true
else
	log "binary installed but --version failed; check ${DEST} manually"
fi
