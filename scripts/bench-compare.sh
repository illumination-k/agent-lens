#!/usr/bin/env bash
# Benchmark a git ref and the working tree back to back and fail when any
# Criterion benchmark got slower than the threshold.
#
# Usage: scripts/bench-compare.sh <ref> <threshold-percent>
#
# Both sides run on the same machine in one session, so the comparison holds
# on a noisy CI runner where a baseline stored from another machine would not.
# A benchmark counts as regressed only when the lower bound of the 95%
# confidence interval on its mean change exceeds the threshold, so ordinary
# run-to-run noise does not fail the gate. A benchmark present on only one side
# is not compared.
set -euo pipefail

ref="${1:-main}"
threshold="${2:-20}"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
export CRITERION_HOME="$CARGO_TARGET_DIR/criterion-compare"
summary="$CARGO_TARGET_DIR/bench-compare.md"

worktree="$(mktemp -d)"
cleanup() { git worktree remove --force "$worktree" >/dev/null 2>&1 || true; }
trap cleanup EXIT

rm -rf "$CRITERION_HOME"
git worktree add --detach "$worktree" "$ref" >/dev/null
(cd "$worktree" && cargo bench --workspace --all-features -- --save-baseline base)
cargo bench --workspace --all-features -- --baseline-lenient base

# One markdown row per compared benchmark; the verdict is the last cell.
report_row() {
	jq -r --arg id "$1" --argjson t "$threshold" '
    def pct: . * 1000 | round / 10;
    .mean as $m
    | ($m.confidence_interval.lower_bound | pct) as $lo
    | ($m.confidence_interval.upper_bound | pct) as $hi
    | (if $lo > $t then "REGRESSED" elif $hi < -$t then "improved" else "ok" end) as $v
    | "| \($id) | \($m.point_estimate | pct)% | [\($lo)%, \($hi)%] | \($v) |"
  ' "$2"
}

regressed=0
{
	echo "| benchmark | mean change | 95% CI | verdict |"
	echo "| --- | ---: | --- | --- |"
} >"$summary"
while IFS= read -r estimates; do
	id="${estimates#"$CRITERION_HOME"/}"
	id="${id%/change/estimates.json}"
	row="$(report_row "$id" "$estimates")"
	echo "$row" >>"$summary"
	if [[ "$row" == *"| REGRESSED |" ]]; then
		regressed=$((regressed + 1))
	fi
done < <(find "$CRITERION_HOME" -path '*/change/estimates.json' | sort)

cat "$summary" >&2
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
	{
		echo "## Benchmarks vs \`$ref\` (threshold ${threshold}%)"
		echo
		cat "$summary"
	} >>"$GITHUB_STEP_SUMMARY"
fi

if ((regressed > 0)); then
	echo "$regressed benchmark(s) regressed by more than ${threshold}% against $ref" >&2
	exit 1
fi
