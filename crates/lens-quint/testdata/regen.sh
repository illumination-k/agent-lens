#!/usr/bin/env bash
# Re-capture the `quint parse` output the unit tests read, so they run
# without the quint CLI. Source paths are rewritten to /fixture/<file>;
# run `dprint fmt` afterwards to restore the checked-in JSON layout.
set -euo pipefail
cd "$(dirname "$0")"
for name in counter helpers; do
	quint parse "$name.qnt" --out "$name.ir.json" --source-map "$name.map.json" || true
done
python3 - <<'PY'
import json, os
for name in ["counter", "helpers"]:
    with open(f"{name}.map.json") as f:
        source_map = json.load(f)
    source_map["sourceIndex"] = {
        k: "/fixture/" + os.path.basename(v) for k, v in source_map["sourceIndex"].items()
    }
    with open(f"{name}.map.json", "w") as f:
        json.dump(source_map, f, separators=(",", ":"))
PY
