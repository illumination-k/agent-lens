#!/usr/bin/env bash
# Write the agent-facing reference files the site serves next to `llms.txt`.
#
# Usage (from web/): tools/generate-llms.sh
#
# Both references are generated from the code (`help --md`, `config schema`),
# so they are built at deploy time and gitignored rather than committed, the
# same way `function-graph.json` is. `llms-full.txt` is the two concatenated,
# for an agent that wants the whole surface in one fetch.
set -euo pipefail

agent_lens() {
	cargo run --quiet --manifest-path ../Cargo.toml --locked -p agent-lens -- "$@"
}

mkdir -p public
agent_lens help --md >public/cli.md
agent_lens config schema >public/agent-lens-toml.md
{
	cat public/cli.md
	printf '\n'
	cat public/agent-lens-toml.md
} >public/llms-full.txt
