#!/bin/bash

# README:
# This hook runs at the start of each claude code session. It sets up the dev environment for claude code.
# About environment variables for claude code, view following document:
# https://code.claude.com/docs/en/settings#environment-variables
#
# If you want to debug this hook, you run `claude --debug` and view the debug log file.

set -eu

cd "$(dirname "$0")/../.."

source .claude/hooks/common.sh

# Send progress to stderr so Claude Code doesn't capture it as additionalContext.
# stdout is reserved for the hook protocol (JSON or empty).
exec 3>&1 1>&2

if ! check_command mise; then
	curl https://mise.run | sh
	export PATH="$HOME/.local/bin:$PATH"
fi

mise trust --all
mise settings experimental=true
mise install

# Build the agent-lens binary so the PostToolUse hooks wired up in
# .claude/settings.json can invoke it directly. `mise exec` activates the
# Rust toolchain for this one command without depending on the env file
# below being sourced first. Debug build is intentional — incremental
# rebuilds are sub-second and these hooks do small, fast analysis.
mise exec -- cargo build -p agent-lens

if [ -n "${CLAUDE_ENV_FILE:-}" ]; then
	DETECTED_SHELL=${CLAUDE_CODE_SHELL:-$(basename "${SHELL:-/bin/bash}")}
	case "$DETECTED_SHELL" in
	bash | zsh) ;;
	*)
		echo "Unsupported shell: $DETECTED_SHELL; falling back to bash."
		DETECTED_SHELL=bash
		;;
	esac

	# Use `mise env` (direct export statements) rather than `mise activate`
	# (interactive-shell hooks via PROMPT_COMMAND) so non-interactive Bash tool
	# invocations get the resolved tool PATH on first source.
	# `target/debug` is prepended so `agent-lens` is callable by name from
	# the Bash tool too (the PostToolUse hook commands use the absolute path
	# directly so they don't depend on the env file being sourced).
	{
		echo "export PATH=\"\$HOME/.local/bin:\$PATH\""
		mise env -s "$DETECTED_SHELL"
		echo "export PATH=\"$PWD/target/debug:\$PATH\""
	} >"$CLAUDE_ENV_FILE"
else
	echo "CLAUDE_ENV_FILE is not set. Skipping shell environment setup."
fi
