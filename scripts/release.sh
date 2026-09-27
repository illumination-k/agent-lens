#!/usr/bin/env bash
# Cut a release: bump the workspace version in a PR, merge it once its checks
# pass, and push the annotated `v<version>` tag that `release-tag.yml` builds.
#
# Usage: scripts/release.sh <version> [notes-file]
#
# Every step checks the state it would produce first, so a run that stopped
# half way (checks failed, network dropped) is resumed by running it again:
#   1. branch `chore/release-<version>` with the bump commit, pushed
#   2. PR for that branch
#   3. checks watched, then the PR squash-merged
#   4. `v<version>` tagged on the merge commit and pushed
#
# The commit and PR body are the notes file when given, otherwise the non-merge,
# non-dependabot subjects since the previous tag. Repo auto-merge is disabled,
# so this waits on `gh pr checks --watch` instead of `gh pr merge --auto`.
set -euo pipefail

version="${1:?usage: release.sh <version> [notes-file]}"
notes_file="${2:-}"
version="${version#v}"
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
	echo "version must be X.Y.Z, got '$version'" >&2
	exit 1
fi
tag="v$version"
branch="chore/release-$version"
title="chore(release): bump version to $version"

# Not `fetch --tags`: the rolling release force-moves `latest` and `rolling`,
# and fetching them onto stale local copies is rejected as a clobber.
git fetch --quiet origin main
if git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null; then
	echo "$tag already exists" >&2
	exit 1
fi

current="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"

# --- 1. bump commit on the release branch -----------------------------------
if git ls-remote --exit-code --heads origin "$branch" >/dev/null; then
	echo "==> $branch already pushed; resuming"
else
	if [ -n "$(git status --porcelain)" ]; then
		echo "working tree is not clean" >&2
		exit 1
	fi
	if [ "$current" = "$version" ]; then
		echo "Cargo.toml is already at $version" >&2
		exit 1
	fi

	prev_tag="$(git describe --tags --abbrev=0 --match 'v[0-9]*' origin/main)"
	if [ -n "$notes_file" ]; then
		notes="$(cat "$notes_file")"
	else
		notes="Changes since $prev_tag:"$'\n\n'"$(git log --no-merges --format='- %s' "$prev_tag..origin/main" |
			grep -v -e '^- chore(deps' -e '^- chore(deps-dev' || true)"
	fi

	echo "==> bumping $current -> $version on $branch"
	git switch --quiet -c "$branch" origin/main
	escaped="${current//./\\.}"
	sed -i.bak "s/^version = \"$escaped\"/version = \"$version\"/" Cargo.toml
	for manifest in crates/*/Cargo.toml; do
		sed -i.bak -E "s/(path = \"\.\.\/[a-z-]+\", version = )\"$escaped\"/\1\"$version\"/" "$manifest"
	done
	rm -f Cargo.toml.bak crates/*/Cargo.toml.bak
	cargo update --workspace --quiet
	if git grep -q "version = \"$current\"" -- Cargo.toml 'crates/*/Cargo.toml'; then
		echo "some manifests still reference $current:" >&2
		git grep -n "version = \"$current\"" -- Cargo.toml 'crates/*/Cargo.toml' >&2
		exit 1
	fi

	git add Cargo.toml Cargo.lock crates/*/Cargo.toml
	git commit --quiet -F - <<EOF
$title

$notes
EOF
	git push --quiet -u origin "$branch"
fi

# --- 2. pull request ----------------------------------------------------------
pr="$(gh pr list --head "$branch" --state all --json number --jq '.[0].number // empty')"
if [ -z "$pr" ]; then
	echo "==> opening PR"
	body="$(git log -1 --format=%b "origin/$branch")"
	gh pr create --head "$branch" --base main --title "$title" --body "$body" >/dev/null
	pr="$(gh pr list --head "$branch" --json number --jq '.[0].number')"
fi
echo "==> PR #$pr: $(gh pr view "$pr" --json url --jq .url)"

# --- 3. wait for checks, then merge -----------------------------------------
state="$(gh pr view "$pr" --json state --jq .state)"
if [ "$state" = OPEN ]; then
	# Polled rather than `gh pr checks --watch`: that exits non-zero on a
	# dropped connection just as on a failed check. A failed query here is
	# retried; only a check in the fail/cancel bucket stops the release. An
	# empty list means the checks have not registered since the push yet.
	echo "==> waiting for checks on #$pr"
	while :; do
		buckets="$(gh pr checks "$pr" --json bucket --jq '[.[].bucket] | unique | join(" ")' 2>/dev/null)" || buckets=unknown
		case " $buckets " in
		*" fail "* | *" cancel "*)
			gh pr checks "$pr" >&2 || true
			echo "checks failed on #$pr; fix them and rerun this script" >&2
			exit 1
			;;
		*" pending "* | "  " | " unknown ") sleep 30 ;;
		*) break ;;
		esac
	done
	echo "==> merging #$pr"
	gh pr merge "$pr" --squash --delete-branch
	state=MERGED
fi
if [ "$state" != MERGED ]; then
	echo "PR #$pr is $state, not merged" >&2
	exit 1
fi

# --- 4. tag the merge commit --------------------------------------------------
sha="$(gh pr view "$pr" --json mergeCommit --jq .mergeCommit.oid)"
git fetch --quiet origin main
git switch --quiet main
git merge --quiet --ff-only origin/main
echo "==> tagging $sha as $tag"
git tag -a "$tag" -m "$tag" "$sha"
git push --quiet origin "$tag"
echo "==> pushed $tag; release-tag.yml builds it:"
echo "    $(gh repo view --json url --jq .url)/actions/workflows/release-tag.yml"
