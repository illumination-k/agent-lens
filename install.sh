#!/usr/bin/env bash
# Install agent-lens from the GitHub Releases pre-built binaries.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/illumination-k/agent-lens/main/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/illumination-k/agent-lens/main/install.sh | bash -s -- --tag main --dir "$HOME/.local/bin"
#
# Environment variables:
#   AGENT_LENS_TAG    Release tag to install (default: latest stable release).
#   AGENT_LENS_DIR    Install directory (default: $HOME/.local/bin).
#   AGENT_LENS_REPO   GitHub repo (default: illumination-k/agent-lens).

set -euo pipefail

REPO="${AGENT_LENS_REPO:-illumination-k/agent-lens}"
TAG="${AGENT_LENS_TAG:-latest}"
INSTALL_DIR="${AGENT_LENS_DIR:-$HOME/.local/bin}"

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
	-h | --help)
		sed -n '2,12p' "$0"
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
		curl -fsSL --retry 3 -o "$out" "$url"
	else
		wget -q -O "$out" "$url"
	fi
}

detect_target() {
	os="$(uname -s)"
	arch="$(uname -m)"
	case "$os" in
	Linux)
		case "$arch" in
		x86_64 | amd64) echo "x86_64-unknown-linux-gnu" ;;
		*) err "unsupported Linux architecture: $arch (only x86_64 has a pre-built binary; build from source with cargo)" ;;
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
		err "Windows is not supported by install.sh; download the .zip from https://github.com/$REPO/releases manually"
		;;
	*)
		err "unsupported OS: $os"
		;;
	esac
}

verify_sha256() {
	archive="$1"
	expected_file="$2"
	expected="$(awk '{print $1}' "$expected_file")"
	if [ -z "$expected" ]; then
		err "could not parse expected SHA-256 from $expected_file"
	fi
	if command -v sha256sum >/dev/null 2>&1; then
		actual="$(sha256sum "$archive" | awk '{print $1}')"
	elif command -v shasum >/dev/null 2>&1; then
		actual="$(shasum -a 256 "$archive" | awk '{print $1}')"
	else
		log "no sha256sum/shasum available; skipping checksum verification"
		return 0
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

if fetch "${BASE_URL}/${ARCHIVE}.sha256" "${TMP}/${ARCHIVE}.sha256" 2>/dev/null; then
	verify_sha256 "${TMP}/${ARCHIVE}" "${TMP}/${ARCHIVE}.sha256"
else
	log "no .sha256 file published; skipping checksum verification"
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
