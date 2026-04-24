#!/bin/bash

set -eu

cd "$(dirname "$0")/../.."

source .claude/hooks/common.sh

# Exit code Claude Code interprets as "return this output as feedback to the
# model" (see Stop hook docs).
readonly CLAUDE_CODE_FEEDBACK_EXIT_CODE=2

# Upper bound on consecutive feedback loops. Once exceeded, this hook stops
# returning feedback and lets Claude end the turn so the user isn't stuck in
# an infinite fix-and-retry loop.
readonly MAX_FEEDBACK_RETRIES=5
readonly FAILURE_COUNTER_FILE=".claude/.stop-hook-failures"

# Run a command without letting `set -e` abort this script, storing the exit
# code in RUN_STATUS. We avoid `$(...)` so the command's stdout/stderr is
# still shown to the user.
RUN_STATUS=0
function run_checked() {
	set +e
	"$@"
	RUN_STATUS=$?
	set -e
}

function read_failure_count() {
	if [ -f "$FAILURE_COUNTER_FILE" ]; then
		cat "$FAILURE_COUNTER_FILE"
	else
		echo 0
	fi
}

function feedback_or_give_up() {
	local message=$1
	local count
	count=$(read_failure_count)
	count=$((count + 1))
	echo "$count" >"$FAILURE_COUNTER_FILE"

	if [ "$count" -ge "$MAX_FEEDBACK_RETRIES" ]; then
		echo "$message" >&2
		echo "Stop hook has failed $count times in a row; giving up to avoid an infinite loop. Fix the issues above, then send a new instruction to re-enable the check." >&2
		# Reset the counter so the next user-initiated turn starts fresh.
		rm -f "$FAILURE_COUNTER_FILE"
		exit 0
	fi

	echo "$message (attempt $count/$MAX_FEEDBACK_RETRIES)"
	exit "$CLAUDE_CODE_FEEDBACK_EXIT_CODE"
}

if ! check_command mise; then
	echo "mise command not found. Please install mise to use this hook."
	exit 1
fi

# Skip fmt/lint when the working tree is clean — pure Q&A turns (no file
# edits) shouldn't pay the cost of running the full check suite.
if check_command git && git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
	if [ -z "$(git status --porcelain)" ]; then
		exit 0
	fi
fi

run_checked mise run fmt
fmt_status=$RUN_STATUS
run_checked mise run lint
lint_status=$RUN_STATUS

if [ "$fmt_status" -ne 0 ] || [ "$lint_status" -ne 0 ]; then
	feedback_or_give_up "Formatting or linting failed. Please fix the issues above."
fi

# All checks passed — reset the failure counter.
rm -f "$FAILURE_COUNTER_FILE"
