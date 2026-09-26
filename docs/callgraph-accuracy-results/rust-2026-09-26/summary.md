# Rust call-graph accuracy, 2026-09-26

rust-analyzer oracle vs `agent-lens analyze function-graph` after the Rust resolver fixes
(see docs/callgraph-accuracy.md, "Rust"). No disagreement is adjudicated. Per-target
disagreements are in `disagreements/<target>.json`.

## pinned (targets.toml): semver, log

oracle: rust-analyzer (type-checker), language: rust, oracle edges: 1566

### Precision of resolved edges

| method       | resolved | caller not type-checked (excluded) | TP  | FP | precision |
| ------------ | -------- | ---------------------------------- | --- | -- | --------- |
| lexical      | 187      | 8                                  | 167 | 12 | 0.933     |
| self_method  | 20       | 0                                  | 20  | 0  | 1.000     |
| last_segment | 73       | 1                                  | 56  | 16 | 0.778     |
| path_suffix  | 71       | 4                                  | 58  | 9  | 0.866     |
| overall      | 351      | 13                                 | 301 | 37 | 0.891     |

### Recall over oracle edges

| dispatch | oracle | found | via lexical | via self_method | via last_segment | via path_suffix | only in candidate set | missed | of which no call site | recall |
| -------- | ------ | ----- | ----------- | --------------- | ---------------- | --------------- | --------------------- | ------ | --------------------- | ------ |
| static   | 473    | 288   | 165         | 18              | 54               | 51              | 98                    | 87     | 3                     | 0.609  |
| dynamic  | 606    | 13    | 2           | 2               | 2                | 7               | 377                   | 216    | 0                     | 0.021  |

### Candidate sets (ambiguous edges)

| method         | sets | caller out of scope (excluded) | mean size | hit rate | candidate precision |
| -------------- | ---- | ------------------------------ | --------- | -------- | ------------------- |
| lexical        | 1    | 0                              | 2.00      | 1.000    | 0.500               |
| last_segment   | 13   | 1                              | 2.00      | 1.000    | 0.500               |
| path_suffix    | 11   | 0                              | 2.00      | 0.273    | 0.136               |
| crate_narrowed | 131  | 9                              | 4.72      | 0.924    | 0.741               |
| overall        | 156  | 10                             | 4.28      | 0.885    | 0.711               |

### Unknown / adjudicated

| bucket                     | reason                             | count |
| -------------------------- | ---------------------------------- | ----- |
| unmapped oracle edge       | callee_nested_or_anonymous         | 32    |
| unmapped oracle edge       | callee_no_enclosing_node           | 63    |
| adjudicated                | oracle_wrong                       | 0     |
| adjudicated                | agent_lens_wrong                   | 0     |
| adjudicated                | definition_gap                     | 0     |
| stale adjudication         | matches no disagreement            | 0     |
| unadjudicated disagreement | agent-lens only + oracle only      | 815   |
| resolved edge excluded     | no caller node (module-level code) | 2     |

## discovery (targets-rust-discovery.toml): either, fastrand, glob, httparse, strsim

oracle: rust-analyzer (type-checker), language: rust, oracle edges: 1014

### Precision of resolved edges

| method       | resolved | caller not type-checked (excluded) | TP  | FP | precision |
| ------------ | -------- | ---------------------------------- | --- | -- | --------- |
| lexical      | 216      | 17                                 | 199 | 0  | 1.000     |
| self_method  | 42       | 0                                  | 42  | 0  | 1.000     |
| last_segment | 264      | 4                                  | 257 | 3  | 0.988     |
| path_suffix  | 45       | 4                                  | 41  | 0  | 1.000     |
| overall      | 567      | 25                                 | 539 | 3  | 0.994     |

### Recall over oracle edges

| dispatch | oracle | found | via lexical | via self_method | via last_segment | via path_suffix | only in candidate set | missed | of which no call site | recall |
| -------- | ------ | ----- | ----------- | --------------- | ---------------- | --------------- | --------------------- | ------ | --------------------- | ------ |
| static   | 607    | 538   | 199         | 41              | 257              | 41              | 4                     | 65     | 2                     | 0.886  |
| dynamic  | 1      | 1     | 0           | 1               | 0                | 0               | 0                     | 0      | 0                     | 1.000  |

### Candidate sets (ambiguous edges)

| method         | sets | caller out of scope (excluded) | mean size | hit rate | candidate precision |
| -------------- | ---- | ------------------------------ | --------- | -------- | ------------------- |
| crate_narrowed | 5    | 0                              | 2.00      | 0.800    | 0.400               |
| overall        | 5    | 0                              | 2.00      | 0.800    | 0.400               |

### Unknown / adjudicated

| bucket                     | reason                             | count |
| -------------------------- | ---------------------------------- | ----- |
| unmapped oracle edge       | callee_nested_or_anonymous         | 132   |
| adjudicated                | oracle_wrong                       | 0     |
| adjudicated                | agent_lens_wrong                   | 0     |
| adjudicated                | definition_gap                     | 0     |
| stale adjudication         | matches no disagreement            | 0     |
| unadjudicated disagreement | agent-lens only + oracle only      | 72    |
| resolved edge excluded     | no caller node (module-level code) | 1     |

## semver

oracle: rust-analyzer (type-checker), language: rust, oracle edges: 475

### Precision of resolved edges

| method       | resolved | caller not type-checked (excluded) | TP  | FP | precision |
| ------------ | -------- | ---------------------------------- | --- | -- | --------- |
| lexical      | 86       | 0                                  | 86  | 0  | 1.000     |
| self_method  | 10       | 0                                  | 10  | 0  | 1.000     |
| last_segment | 45       | 0                                  | 45  | 0  | 1.000     |
| path_suffix  | 16       | 0                                  | 16  | 0  | 1.000     |
| overall      | 157      | 0                                  | 157 | 0  | 1.000     |

### Recall over oracle edges

| dispatch | oracle | found | via lexical | via self_method | via last_segment | via path_suffix | only in candidate set | missed | of which no call site | recall |
| -------- | ------ | ----- | ----------- | --------------- | ---------------- | --------------- | --------------------- | ------ | --------------------- | ------ |
| static   | 206    | 157   | 86          | 10              | 45               | 16              | 13                    | 36     | 0                     | 0.762  |

### Candidate sets (ambiguous edges)

| method       | sets | caller out of scope (excluded) | mean size | hit rate | candidate precision |
| ------------ | ---- | ------------------------------ | --------- | -------- | ------------------- |
| last_segment | 13   | 0                              | 2.00      | 1.000    | 0.500               |
| overall      | 13   | 0                              | 2.00      | 1.000    | 0.500               |

### Unknown / adjudicated

| bucket                     | reason                             | count |
| -------------------------- | ---------------------------------- | ----- |
| unmapped oracle edge       | callee_nested_or_anonymous         | 4     |
| adjudicated                | oracle_wrong                       | 0     |
| adjudicated                | agent_lens_wrong                   | 0     |
| adjudicated                | definition_gap                     | 0     |
| stale adjudication         | matches no disagreement            | 0     |
| unadjudicated disagreement | agent-lens only + oracle only      | 49    |
| resolved edge excluded     | no caller node (module-level code) | 1     |

## log

oracle: rust-analyzer (type-checker), language: rust, oracle edges: 1091

### Precision of resolved edges

| method       | resolved | caller not type-checked (excluded) | TP  | FP | precision |
| ------------ | -------- | ---------------------------------- | --- | -- | --------- |
| lexical      | 101      | 8                                  | 81  | 12 | 0.871     |
| self_method  | 10       | 0                                  | 10  | 0  | 1.000     |
| last_segment | 28       | 1                                  | 11  | 16 | 0.407     |
| path_suffix  | 55       | 4                                  | 42  | 9  | 0.824     |
| overall      | 194      | 13                                 | 144 | 37 | 0.796     |

### Recall over oracle edges

| dispatch | oracle | found | via lexical | via self_method | via last_segment | via path_suffix | only in candidate set | missed | of which no call site | recall |
| -------- | ------ | ----- | ----------- | --------------- | ---------------- | --------------- | --------------------- | ------ | --------------------- | ------ |
| static   | 267    | 131   | 79          | 8               | 9                | 35              | 85                    | 51     | 3                     | 0.491  |
| dynamic  | 606    | 13    | 2           | 2               | 2                | 7               | 377                   | 216    | 0                     | 0.021  |

### Candidate sets (ambiguous edges)

| method         | sets | caller out of scope (excluded) | mean size | hit rate | candidate precision |
| -------------- | ---- | ------------------------------ | --------- | -------- | ------------------- |
| lexical        | 1    | 0                              | 2.00      | 1.000    | 0.500               |
| last_segment   | 0    | 1                              | -         | -        | -                   |
| path_suffix    | 11   | 0                              | 2.00      | 0.273    | 0.136               |
| crate_narrowed | 131  | 9                              | 4.72      | 0.924    | 0.741               |
| overall        | 143  | 10                             | 4.49      | 0.874    | 0.720               |

### Unknown / adjudicated

| bucket                     | reason                             | count |
| -------------------------- | ---------------------------------- | ----- |
| unmapped oracle edge       | callee_nested_or_anonymous         | 28    |
| unmapped oracle edge       | callee_no_enclosing_node           | 63    |
| adjudicated                | oracle_wrong                       | 0     |
| adjudicated                | agent_lens_wrong                   | 0     |
| adjudicated                | definition_gap                     | 0     |
| stale adjudication         | matches no disagreement            | 0     |
| unadjudicated disagreement | agent-lens only + oracle only      | 766   |
| resolved edge excluded     | no caller node (module-level code) | 1     |

## either

oracle: rust-analyzer (type-checker), language: rust, oracle edges: 31

### Precision of resolved edges

| method       | resolved | caller not type-checked (excluded) | TP | FP | precision |
| ------------ | -------- | ---------------------------------- | -- | -- | --------- |
| lexical      | 3        | 0                                  | 3  | 0  | 1.000     |
| self_method  | 2        | 0                                  | 2  | 0  | 1.000     |
| last_segment | 6        | 0                                  | 6  | 0  | 1.000     |
| overall      | 11       | 0                                  | 11 | 0  | 1.000     |

### Recall over oracle edges

| dispatch | oracle | found | via lexical | via self_method | via last_segment | only in candidate set | missed | of which no call site | recall |
| -------- | ------ | ----- | ----------- | --------------- | ---------------- | --------------------- | ------ | --------------------- | ------ |
| static   | 19     | 10    | 3           | 1               | 6                | 0                     | 9      | 2                     | 0.526  |
| dynamic  | 1      | 1     | 0           | 1               | 0                | 0                     | 0      | 0                     | 1.000  |

### Unknown / adjudicated

| bucket                     | reason                             | count |
| -------------------------- | ---------------------------------- | ----- |
| unmapped oracle edge       | callee_nested_or_anonymous         | 7     |
| adjudicated                | oracle_wrong                       | 0     |
| adjudicated                | agent_lens_wrong                   | 0     |
| adjudicated                | definition_gap                     | 0     |
| stale adjudication         | matches no disagreement            | 0     |
| unadjudicated disagreement | agent-lens only + oracle only      | 9     |
| resolved edge excluded     | no caller node (module-level code) | 0     |

## fastrand

oracle: rust-analyzer (type-checker), language: rust, oracle edges: 134

### Precision of resolved edges

| method       | resolved | caller not type-checked (excluded) | TP  | FP | precision |
| ------------ | -------- | ---------------------------------- | --- | -- | --------- |
| lexical      | 28       | 0                                  | 28  | 0  | 1.000     |
| self_method  | 15       | 0                                  | 15  | 0  | 1.000     |
| last_segment | 39       | 0                                  | 38  | 1  | 0.974     |
| path_suffix  | 26       | 0                                  | 26  | 0  | 1.000     |
| overall      | 108      | 0                                  | 107 | 1  | 0.991     |

### Recall over oracle edges

| dispatch | oracle | found | via lexical | via self_method | via last_segment | via path_suffix | only in candidate set | missed | of which no call site | recall |
| -------- | ------ | ----- | ----------- | --------------- | ---------------- | --------------- | --------------------- | ------ | --------------------- | ------ |
| static   | 114    | 107   | 28          | 15              | 38               | 26              | 0                     | 7      | 0                     | 0.939  |

### Unknown / adjudicated

| bucket                     | reason                             | count |
| -------------------------- | ---------------------------------- | ----- |
| adjudicated                | oracle_wrong                       | 0     |
| adjudicated                | agent_lens_wrong                   | 0     |
| adjudicated                | definition_gap                     | 0     |
| stale adjudication         | matches no disagreement            | 0     |
| unadjudicated disagreement | agent-lens only + oracle only      | 8     |
| resolved edge excluded     | no caller node (module-level code) | 0     |

## glob

oracle: rust-analyzer (type-checker), language: rust, oracle edges: 323

### Precision of resolved edges

| method       | resolved | caller not type-checked (excluded) | TP | FP | precision |
| ------------ | -------- | ---------------------------------- | -- | -- | --------- |
| lexical      | 33       | 0                                  | 33 | 0  | 1.000     |
| self_method  | 5        | 0                                  | 5  | 0  | 1.000     |
| last_segment | 15       | 0                                  | 13 | 2  | 0.867     |
| overall      | 53       | 0                                  | 51 | 2  | 0.962     |

### Recall over oracle edges

| dispatch | oracle | found | via lexical | via self_method | via last_segment | only in candidate set | missed | of which no call site | recall |
| -------- | ------ | ----- | ----------- | --------------- | ---------------- | --------------------- | ------ | --------------------- | ------ |
| static   | 62     | 51    | 33          | 5               | 13               | 0                     | 11     | 0                     | 0.823  |

### Candidate sets (ambiguous edges)

| method         | sets | caller out of scope (excluded) | mean size | hit rate | candidate precision |
| -------------- | ---- | ------------------------------ | --------- | -------- | ------------------- |
| crate_narrowed | 1    | 0                              | 2.00      | 0.000    | 0.000               |
| overall        | 1    | 0                              | 2.00      | 0.000    | 0.000               |

### Unknown / adjudicated

| bucket                     | reason                             | count |
| -------------------------- | ---------------------------------- | ----- |
| unmapped oracle edge       | callee_nested_or_anonymous         | 116   |
| adjudicated                | oracle_wrong                       | 0     |
| adjudicated                | agent_lens_wrong                   | 0     |
| adjudicated                | definition_gap                     | 0     |
| stale adjudication         | matches no disagreement            | 0     |
| unadjudicated disagreement | agent-lens only + oracle only      | 13    |
| resolved edge excluded     | no caller node (module-level code) | 0     |

## httparse

oracle: rust-analyzer (type-checker), language: rust, oracle edges: 363

### Precision of resolved edges

| method       | resolved | caller not type-checked (excluded) | TP  | FP | precision |
| ------------ | -------- | ---------------------------------- | --- | -- | --------- |
| lexical      | 136      | 17                                 | 119 | 0  | 1.000     |
| self_method  | 15       | 0                                  | 15  | 0  | 1.000     |
| last_segment | 112      | 4                                  | 108 | 0  | 1.000     |
| path_suffix  | 9        | 4                                  | 5   | 0  | 1.000     |
| overall      | 272      | 25                                 | 247 | 0  | 1.000     |

### Recall over oracle edges

| dispatch | oracle | found | via lexical | via self_method | via last_segment | via path_suffix | only in candidate set | missed | of which no call site | recall |
| -------- | ------ | ----- | ----------- | --------------- | ---------------- | --------------- | --------------------- | ------ | --------------------- | ------ |
| static   | 284    | 247   | 119         | 15              | 108              | 5               | 4                     | 33     | 0                     | 0.870  |

### Candidate sets (ambiguous edges)

| method         | sets | caller out of scope (excluded) | mean size | hit rate | candidate precision |
| -------------- | ---- | ------------------------------ | --------- | -------- | ------------------- |
| crate_narrowed | 4    | 0                              | 2.00      | 1.000    | 0.500               |
| overall        | 4    | 0                              | 2.00      | 1.000    | 0.500               |

### Unknown / adjudicated

| bucket                     | reason                             | count |
| -------------------------- | ---------------------------------- | ----- |
| unmapped oracle edge       | callee_nested_or_anonymous         | 9     |
| adjudicated                | oracle_wrong                       | 0     |
| adjudicated                | agent_lens_wrong                   | 0     |
| adjudicated                | definition_gap                     | 0     |
| stale adjudication         | matches no disagreement            | 0     |
| unadjudicated disagreement | agent-lens only + oracle only      | 37    |
| resolved edge excluded     | no caller node (module-level code) | 1     |

## strsim

oracle: rust-analyzer (type-checker), language: rust, oracle edges: 163

### Precision of resolved edges

| method       | resolved | caller not type-checked (excluded) | TP  | FP | precision |
| ------------ | -------- | ---------------------------------- | --- | -- | --------- |
| lexical      | 16       | 0                                  | 16  | 0  | 1.000     |
| self_method  | 5        | 0                                  | 5   | 0  | 1.000     |
| last_segment | 92       | 0                                  | 92  | 0  | 1.000     |
| path_suffix  | 10       | 0                                  | 10  | 0  | 1.000     |
| overall      | 123      | 0                                  | 123 | 0  | 1.000     |

### Recall over oracle edges

| dispatch | oracle | found | via lexical | via self_method | via last_segment | via path_suffix | only in candidate set | missed | of which no call site | recall |
| -------- | ------ | ----- | ----------- | --------------- | ---------------- | --------------- | --------------------- | ------ | --------------------- | ------ |
| static   | 128    | 123   | 16          | 5               | 92               | 10              | 0                     | 5      | 0                     | 0.961  |

### Unknown / adjudicated

| bucket                     | reason                             | count |
| -------------------------- | ---------------------------------- | ----- |
| adjudicated                | oracle_wrong                       | 0     |
| adjudicated                | agent_lens_wrong                   | 0     |
| adjudicated                | definition_gap                     | 0     |
| stale adjudication         | matches no disagreement            | 0     |
| unadjudicated disagreement | agent-lens only + oracle only      | 5     |
| resolved edge excluded     | no caller node (module-level code) | 0     |
