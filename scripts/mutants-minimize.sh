#!/usr/bin/env bash
# Report the redundant tests of one package with mutrim's test-suite minimizer.
#
# Usage: scripts/mutants-minimize.sh <package> <jobs>
#
# cargo-mutants gives the kill matrix (which tests fail against which mutant),
# one coverage run per test gives the lines each test executes, and
# `mutrim minimize` runs a weighted greedy set cover over both: the tests it
# does not select are redundant, each listed with the tests subsuming it, and
# the essential ones with what only they satisfy. Nothing is deleted.
#
# Per-test coverage builds the package's test binaries once with
# `-C instrument-coverage` and runs each test on its own, instead of one
# `cargo llvm-cov` invocation (build, run, report) per test.
#
# Everything lands in target/mutants-minimize/<package>/; minimize.json is the
# result. With MUTANTS_REUSE=1 the mutants.out of the previous run is reused and
# only the coverage and the minimization run again. mutrim must be on PATH (or
# named by $MUTRIM):
#   go install github.com/illumination-k/mutrim/cmd/mutrim@latest
set -euo pipefail

pkg="${1:?usage: mutants-minimize.sh <package> <jobs>}"
jobs="${2:-2}"
mutrim="${MUTRIM:-mutrim}"
if ! command -v "$mutrim" >/dev/null 2>&1; then
	echo "mutrim not found; install it with: go install github.com/illumination-k/mutrim/cmd/mutrim@latest" >&2
	exit 1
fi

root="$PWD"
target="${CARGO_TARGET_DIR:-$root/target}"
out="$target/mutants-minimize/$pkg"
obs="$out/obs"
rm -rf "$obs" "$out/profraw"
mkdir -p "$obs" "$out/profraw"

# Kills. A failing test binary must not stop the others, or the kill matrix
# misses every test after it.
if [ "${MUTANTS_REUSE:-}" != 1 ] || [ ! -f "$out/mutants.out/outcomes.json" ]; then
	cargo mutants --package "$pkg" --no-shuffle --jobs "$jobs" \
		--cargo-test-arg=--no-fail-fast --output "$out" >&2
fi
"$mutrim" import cargo-mutants -o "$obs/mutants.json" "$out/mutants.out"

# Coverage, in a target directory of its own: instrumented artifacts must not
# replace the normal ones. The instrumented build scripts write profiles too.
host="$(rustc -vV | sed -n 's/^host: //p')"
llvm="$(rustc --print sysroot)/lib/rustlib/$host/bin"
bins="$out/binaries.tsv"
RUSTFLAGS="-C instrument-coverage" CARGO_TARGET_DIR="$target/coverage-per-test" \
	LLVM_PROFILE_FILE="$out/profraw/build-%p-%m.profraw" \
	cargo test --package "$pkg" --all-features --locked --no-run --message-format=json |
	jq -r 'select(.reason == "compiler-artifact" and .executable != null)
    | [(.target.name | gsub("-"; "_")), (.profile.test | tostring), .executable, (.manifest_path | sub("/Cargo.toml$"; ""))]
    | @tsv' >"$bins"
# Every executable is an object of the export, so the code a test reaches by
# spawning the package's binary counts as well.
objects=()
while IFS=$'\t' read -r _ _ exe _; do objects+=(-object "$exe"); done <"$bins"

# Only the package's own sources are requirements: its dependencies' code is
# their own tests' to cover.
files="^$(cargo metadata --format-version 1 --no-deps |
	jq -r --arg pkg "$pkg" '.packages[] | select(.name == $pkg) | .manifest_path' |
	sed "s|^$root/||; s|Cargo.toml$|src/|")"

n=0
jq -r '.tests[].name' "$obs/mutants.json" | while IFS= read -r test; do
	crate="${test%%::*}" path="${test#*::}"
	read -r exe dir < <(awk -F'\t' -v c="$crate" '$1 == c && $2 == "true" { print $3 "\t" $4; exit }' "$bins")
	if [ -z "${exe:-}" ]; then
		echo "no test binary for $test; skipped" >&2
		continue
	fi
	n=$((n + 1))
	prof="$out/profraw/$n"
	mkdir -p "$prof"
	# cargo test runs a test binary from its package directory.
	(cd "$dir" && LLVM_PROFILE_FILE="$prof/%p-%m.profraw" "$exe" --exact "$path" --quiet >/dev/null 2>&1) ||
		echo "$test failed on its own" >&2
	"$llvm/llvm-profdata" merge -sparse "$prof"/*.profraw -o "$prof.profdata"
	"$llvm/llvm-cov" export -format=text -instr-profile "$prof.profdata" "$exe" "${objects[@]}" >"$prof.json"
	"$mutrim" import llvm-cov -root "$root" -files "$files" -test "$test" -o "$obs/cov-$n.json" "$prof.json"
	rm -rf "$prof" "$prof.profdata" "$prof.json"
done

"$mutrim" minimize -o "$out/minimize.json" "$obs"/*.json
jq -r '"selected \(.selected | length) (essential \([.selected[] | select(.essential)] | length)), redundant \(.redundant | length), weak spots \(.weak_spots | length)"' "$out/minimize.json" >&2
echo "$out/minimize.json"
