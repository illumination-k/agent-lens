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

if [ -n "${CLAUDE_ENV_FILE:-}" ]; then
	# Use `mise env` (direct export statements) rather than `mise activate`
	# (interactive-shell hooks via PROMPT_COMMAND) so non-interactive Bash tool
	# invocations get the resolved tool PATH on first source.
	{
		echo "export PATH=\"\$HOME/.local/bin:\$PATH\""
		mise env -s bash
	} >"$CLAUDE_ENV_FILE"
else
	echo "CLAUDE_ENV_FILE is not set. Skipping shell environment setup."
fi
