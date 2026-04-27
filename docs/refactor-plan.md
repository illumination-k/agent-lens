# agent-lens リファクタ計画

`agent-lens` 自身を `analyze hotspot / complexity / coupling / cohesion / similarity / wrapper / context-span` で計測した結果から導いた、優先度つきのリファクタ計画。

## 1. 計測サマリ

### Top hotspots (`analyze hotspot crates --top 15`)

| 順位 | file                                   | score | commits | cog | cc | loc | fns |
| ---: | -------------------------------------- | ----: | ------: | --: | -: | --: | --: |
|    1 | `agent-lens/src/analyze/similarity.rs` |   170 |      10 |  17 |  9 | 715 |  33 |
|    2 | `agent-lens/src/main.rs`               |    51 |      17 |   3 | 14 | 499 |  45 |
|    3 | `agent-lens/src/analyze/coupling.rs`   |    45 |       5 |   9 |  7 | 459 |  40 |
|    4 | `agent-lens/src/analyze/mod.rs`        |    42 |      14 |   3 |  4 | 217 |  20 |
|    5 | `lens-ts/src/parser.rs`                |    36 |       6 |   6 |  4 | 276 |  25 |
|    6 | `lens-rust/src/coupling.rs`            |    35 |       5 |   7 |  7 | 817 |  61 |
|    7 | `lens-rust/src/cohesion.rs`            |    32 |       4 |   8 |  6 | 345 |  25 |
|    8 | `agent-lens/src/analyze/wrapper.rs`    |    30 |       6 |   5 |  5 | 249 |  19 |
|    9 | `lens-ts/src/complexity.rs`            |    30 |       6 |   5 |  4 | 737 |  81 |
|   10 | `agent-lens/src/analyze/complexity.rs` |    27 |       9 |   3 |  9 | 364 |  27 |

> **読み**: 上位 4 件すべてが `agent-lens` バイナリ crate に集中している。
> 「複雑な所」と「変更頻度が高い所」が一致しているので、ここに手を入れることで投資対効果が最も高い。

### 高 cognitive な関数（要分割候補）

| 関数                             | file                                   | cc | cog | nest |
| -------------------------------- | -------------------------------------- | -: | --: | ---: |
| `SimilarityAnalyzer::find_pairs` | `agent-lens/src/analyze/similarity.rs` |  9 |  17 |    3 |
| `extract_functions`              | `agent-lens/src/analyze/similarity.rs` |  6 |  10 |    2 |
| `resolve_crate_root`             | `agent-lens/src/analyze/coupling.rs`   |  7 |   9 |    3 |
| `collect_item`                   | `lens-rust/src/cohesion.rs`            |  6 |   8 |    3 |
| `walk_tokens`                    | `lens-rust/src/complexity.rs`          |  6 |   7 |    3 |
| `PathResolver::absolutize_super` | `lens-rust/src/coupling.rs`            |  4 |   7 |    3 |
| `collect_churn`                  | `agent-lens/src/analyze/hotspot.rs`    |  7 |   6 |    2 |
| `AnalyzeCommand::run`            | `agent-lens/src/main.rs`               | 14 |   1 |    1 |

### Coupling のホットスポット (IFC desc)

- `agent-lens::hooks::core` — fan_in=9 / IFC=81。**Hook 共通土台が中心ハブ**。
- `lens-ts::parser` — fan_in=4 fan_out=4 / IFC=256。lens-ts のすべてが parser を経由する。
- `agent-lens::analyze` — fan_in=13 / fan_out=0 (instability=0.0)。再エクスポート用集約点としては妥当。

### Cohesion の split 候補

- `impl Visit for EdgeVisitor` (`lens-rust/src/coupling.rs` L438-487) — **LCOM4 = 5**。
  `visit_item_use` / `visit_expr_path` / `visit_type_path` / `visit_item_impl` /
  `visit_item_mod` がフィールドを共有していない。視覚的にも責務が分かれている。
- `impl Visit for SelfRefVisitor` (`lens-rust/src/cohesion.rs`) — LCOM4 = 2。

### 重複（`analyze similarity --threshold 0.85 --exclude-tests`）

134 ペアが閾値超え。本質的なものは大きく 4 系統。

#### A. Hook glue の三重定義 (Claude Code × Codex × `core`)

`hooks/post_tool_use/{similarity,wrapper}.rs` と
`hooks/codex/post_tool_use/{similarity,wrapper}.rs` が、コア処理を
`hooks/core` に切り出した後も **アダプタ層が 100% 重複**している：

```
hooks/codex/post_tool_use/similarity.rs : SimilarityHook::new   ─┐
hooks/codex/post_tool_use/wrapper.rs    : WrapperHook::new      ─┤
hooks/post_tool_use/similarity.rs       : SimilarityHook::new   ─┼─ 全部 100% similar
hooks/post_tool_use/wrapper.rs          : WrapperHook::new      ─┘
hooks/codex/post_tool_use/similarity.rs : SimilarityHook::handle ── 100% similar to
hooks/post_tool_use/similarity.rs       : SimilarityHook::handle    （出力ラップだけ違う）
```

#### B. Setup の双子コード (`hooks/setup.rs` ↔ `hooks/codex/setup.rs`)

`SetupPlan::changed` / `resolve_path` / `has_command_prefix` が完全一致。
さらに `extract_patch_command` (codex) ↔ `extract_file_path` (claude) が
100% similar — どちらも tool_input から対象パスを取り出すだけ。

#### C. Analyzer サブコマンドの定型化されたボイラープレート

```
AnalyzeCommand::run_cohesion / run_complexity / run_wrapper       — 100% similar
AnalyzeCommand::run_coupling / run_context_span                   — 100% similar
CohesionAnalyzer::analyze ↔ ComplexityAnalyzer::analyze           — 100% similar
analyze/cohesion.rs::Report::new ↔ analyze/wrapper.rs::Report::new — 100% similar
analyze/{cohesion,complexity,coupling}.rs::format_optional_f64    — 3 重複
analyze/coupling.rs::resolve_crate_root ↔ analyze/context_span.rs::resolve_crate_root
ContextSpanAnalyzerError::from ↔ CouplingAnalyzerError::from
run_post_tool_use ↔ run_codex_post_tool_use (main.rs)
```

#### D. 言語アダプタ間の重複 (`lens-rust` / `lens-ts` / `lens-py`)

3 言語で形が揃っている小さなユーティリティが各 crate に複製されている：

```
extract_functions_excluding_tests   — 3 言語で 100% similar
qualify / qualify_name              — lens-rust と lens-py 両方の parser/wrapper/complexity に
type_path_last_ident                — lens-rust 内 3 箇所 + parser/cohesion 重複
ComplexityVisitor::new              — lens-ts ↔ lens-py
ComplexityVisitor::enter_nest       — lens-rust / lens-ts / lens-py
args_pass_through                   — 3 言語で重複 (lens-rust/lens-ts/lens-py の wrapper)
walk_decl ↔ collect_decl            — lens-ts/walk.rs ↔ lens-ts/wrapper.rs
collect_module_body                 — lens-ts/{walk,wrapper,cohesion}.rs に 3 重実装
```

#### E. 同一 file 内の隣接重複

- `EdgeVisitor::visit_expr_path` ↔ `EdgeVisitor::visit_type_path` (`lens-rust/coupling.rs`)
- `ComplexityVisitor::visit_while_statement` ↔ `visit_for_in_statement` ↔ `visit_for_of_statement` (`lens-ts/complexity.rs`)
- `ComplexityVisitor::visit_expr_while` ↔ `visit_expr_for_loop` (`lens-rust/complexity.rs`)
- `extract_cohesion_units` ↔ `extract_complexity_units` ↔ `find_wrappers` (lens-rust)

> 注: `format_optional_f64` ↔ `qualify` のクロスペアは AST shape が同じだけの偽陽性。
> 各重複に対して機械的に書き換えるのではなく、構造的に同型なものだけを束ねる。

### Wrapper analyzer の指摘

意図的なアクセサのため対処不要。

- `CohesionUnit::lcom4` → `self.components.len`
- `FunctionComplexity::halstead_volume` → `self.halstead.volume`

---

## 2. リファクタ方針

「analyzer は最小限に直す」「言語アダプタ間と Hook glue の重複を畳む」の 2 軸。
全体として**新たに crate を増やさず**、既存 crate 内の `mod` 整理で済ませる。

### 方針 1: コアと「アダプタ層」の責務をはっきり分ける

`hooks/core` に処理は寄っているが、アダプタ層の **構造** が共通化できていない。
両エージェントを跨ぐ trait を導入してアダプタ層を 1 種類のジェネリック実装に畳む。

### 方針 2: analyze サブコマンドを「データ駆動」に

`AnalyzeCommand` 各バリアントごとに `run_*` を 7 個書いているのを、
`Analyzer::run(path, format) -> String` 風の trait + 設定 builder にして、
`main.rs` の dispatch を 1 表で済ませる。

### 方針 3: 言語アダプタの共通ユーティリティを `lens-domain` に集約

3 言語で 100% 同型の関数（`qualify_name` / `type_path_last_ident` /
`ComplexityVisitor` の nest 制御 / `args_pass_through` / `extract_functions_excluding_tests`）
は **言語非依存** にできる。`lens-domain` に小さな module を足し、
`lens-{rust,ts,py}` から再利用する。

### 方針 4: 単一 file 内の重複は内部ヘルパーで畳む

ファイル内で 100% 同型のメソッド対は `__visit_loop_like` のような
private helper を 1 本入れて呼び出すだけにすれば良い。新しい抽象を立てる必要はない。

### 方針 5: `EdgeVisitor` を凝集の良い 4 つに割る

LCOM4=5 は「フィールド共有がない 5 つの責務が同居している」サイン。
`use` / `expr_path` / `type_path` / `impl` の各 visit を別 struct に分け、
親 visitor からは「edges を集める」インタフェースだけ見せる。

---

## 3. 優先度つきタスクリスト

> 各タスクには「期待する agent-lens の数値変化」を併記。完了後にもう一度
> 同じコマンドを回してデルタを確認できるようにする。

### P0 — boilerplate を畳む（1〜2 日、低リスク）

P0-1. **`hooks/post_tool_use` と `hooks/codex/post_tool_use` のアダプタを統合**

- `core` 側に `OutputWriter` trait を切る:
  ```rust
  trait HookEnvelope {
      type Input;  type Output;
      fn extract_sources(input: &Self::Input) -> Result<Vec<EditedSource>, HookError>;
      fn wrap_report(report: String) -> Self::Output;
      fn empty_output() -> Self::Output;
  }
  ```
- `SimilarityHook<E: HookEnvelope>` / `WrapperHook<E: HookEnvelope>` を `core` に置く。
- `hooks/post_tool_use/` は `ClaudeCodeEnvelope` だけ、`hooks/codex/post_tool_use/` は `CodexEnvelope` だけを定義（10〜20 行）。
- 期待: `analyze similarity` の Hook 系 4 ペア消滅。`hooks/post_tool_use/{similarity,wrapper}.rs` の LOC を -50% 以上。

P0-2. **`hooks/setup.rs` ↔ `hooks/codex/setup.rs` の共通化**

- `resolve_path` / `SetupPlan::changed` / `has_command_prefix` を `hooks/setup_common.rs` に移し、両側で `pub use` する。
- `extract_file_path` (claude) と `extract_patch_command` (codex) の差は「tool_input の経路」だけ。tool→path のテーブルを作って 1 関数にする。
- 期待: `setup.rs` の重複 3 ペア消滅。

P0-3. **`analyze/*::{Report::new, format_optional_f64, resolve_crate_root, *AnalyzerError::from}` を共通化**

- 新規ヘルパは作らず、`agent-lens/src/analyze/mod.rs` に `pub(crate)` で `format_optional_f64` と `resolve_crate_root` を移す。
- `*AnalyzerError::from` は `thiserror` の `#[from]` で生成すれば手書き不要 → boilerplate 関数自体を削除。
- 期待: similarity report の analyze 内 100% ペア -7 程度減。

P0-4. **`AnalyzeCommand` の dispatch を畳む**

- `AnalyzeCommand::run` は **cyclomatic 14 / cognitive 1** で「直線的に長い」だけ。
  下位の `run_cohesion / run_complexity / run_wrapper` は 100% similar。
- `Analyzer::run(self, path, format) -> String` 系の小 trait か、
  クロージャを 1 度書くだけのテーブルに置き換える。
- `run_post_tool_use` ↔ `run_codex_post_tool_use` も同様にテーブル化。
- 期待: `main.rs` の cc が 14→5 程度に。`run_*` 7 関数 → 1〜2 関数。

### P1 — 高リスクファイルの「複雑な関数」を割る（2〜3 日、要テスト）

P1-1. **`SimilarityAnalyzer::find_pairs` (cog=17, nest=3) を分解**

- 現在 1 関数で「フィルタリング・ペア生成・スコアリング・ソート」を全部やっている。
- 抽出案:
  - `corpus_passes_min_lines(...)`: short-circuit フィルタを iterator メソッドに。
  - `score_pair(a, b, opts) -> Option<f64>`: TSED + threshold ガード。
  - `sort_by_similarity_desc(&mut pairs)`: 並び替え単独。
- `find_pairs` 本体は `iproduct!()` ライクなフラット iterator + `.filter_map` に。
- 期待: cog 17→6 以下、nest 3→2、関数長 37 行→15 行程度。

P1-2. **`extract_functions` (cog=10) を `corpus.rs` 等に切り出してテスト追加**

- パスの拡張子 → SourceLang → Parser のテーブルが 1 関数に詰まっている。
- table-driven 化して言語追加時のコストを下げる。

P1-3. **`AnalyzeCommand::run_hotspot` の `since` 場合分けを `with_since(Option<_>)` 内に押し込む**

- `HotspotAnalyzer::with_since(impl Into<Option<String>>)` を生やせば、main.rs の `match since` を消せる。

P1-4. **`resolve_crate_root` (analyze/coupling.rs L128, cog=9) を共通化 + 早期 return 優先**

- P0-3 で共通化したあと、`metadata.is_file() / is_dir()` ぶら下げを `match` 1 段に揃える。

### P2 — 言語アダプタの共通化（3〜5 日、コア API 変更を伴う）

P2-1. **`lens-domain` に `language_utils` を新設**

- 移動候補:
  - `qualify_name(impl_path, item_name) -> String`
  - `type_path_last_ident(path: &[String]) -> Option<String>`（言語非依存に正規化）
  - `extract_functions_excluding_tests` の汎用ロジック（test 判定だけ言語側 hook）
  - `ComplexityVisitor` の `enter_nest` / `exit_nest` / `new` 共通部分（Nesting カウンタを domain に切り出し）
- 期待: `lens-{rust,ts,py}` のヘルパ重複ペア 20+ が消滅。`lens-domain::complexity` の責務が増えるが、言語非依存 metric の置き場として正当。

P2-2. **`args_pass_through` を domain に**

- 形が揃っているのは仕様上当然なので、引数列の structural eq だけ抽出してテストを 1 本に集約。

P2-3. **`lens-ts` の `walk.rs` ↔ `wrapper.rs::collect_*` ↔ `cohesion.rs::collect_module_body` を統合**

- 「decl/stmt/module body を辿って関数を吸い上げる」軸が 3 重実装になっているので、`lens-ts/walk.rs` を **唯一の walker** にして、`wrapper`/`cohesion` 側はコールバックだけ渡す。

### P3 — 凝集と coupling の整理（数日、設計判断あり）

P3-1. **`EdgeVisitor`（LCOM4=5）を分割**

- 各 `visit_*` を専用の小さな struct に切り出し、親 visitor は `record(target, kind, symbol)` だけを共通インタフェースとして持つ。
- 期待: LCOM4 5 → 1〜2、`lens-rust/coupling.rs` の cog 上位（`walk_use_tree` cog=5、`split_modules` cog=6）も自然に下がる。

P3-2. **`hooks::core` ハブを軽くする**

- IFC=81 / fan_in=9 は中心ハブとして妥当だが、`HookError` だけ別 mod にすれば「データ型のみを依存先として持つ薄い層」を作れる。`hooks::core::error` に切り出して fan_in を分散。

P3-3. **`lens-ts::parser` (IFC=256) の責務確認**

- IFC 256 は fan_in=4 / fan_out=4 から来ている。Public API として `parser::*` を素通しさせている関数があれば lib.rs に上げて parser 自体は **AST → 内部表現** だけに閉じる。

### P4 — 既存指標を補強する追加メトリクス（オプション）

CLAUDE.md の「指標カタログ」のうち、上記リファクタの効果検証に直接効くものを先に。

- **Temporal Coupling**: `hooks/post_tool_use/*` と `hooks/codex/post_tool_use/*` が
  常に同時変更されるはず → P0-1 の効果が温存されているかを後付けで検証できる。
- **Token Budget**: `analyze/similarity.rs` (715 LOC) が context window のどれくらいを食うかを定量化。P1-1 の前後で比較。

---

## 4. 進め方と検証フロー

各 P0/P1 タスク完了ごとに：

```bash
mise run lint && mise run test            # 既存ガード
./target/debug/agent-lens analyze hotspot crates --top 15 --format md
./target/debug/agent-lens analyze similarity crates --format md --threshold 0.85 --exclude-tests | wc -l
./target/debug/agent-lens analyze cohesion crates/lens-rust/src/coupling.rs --format md
./target/debug/agent-lens analyze complexity crates/agent-lens/src/analyze/similarity.rs --format md
```

期待されるデルタ目標（リファクタ完了時点）:

| 指標                                                 | 現状 | 目標 |
| ---------------------------------------------------- | ---: | ---: |
| similar pair 数 (`--threshold 0.85 --exclude-tests`) |  134 |  <60 |
| `analyze/similarity.rs::find_pairs` cognitive        |   17 |   ≤6 |
| `main.rs::AnalyzeCommand::run` cyclomatic            |   14 |   ≤5 |

## 6. 実施結果（P0–P3 完了時点）

P0–P3 を実装した時点の `agent-lens` 自身に対する計測値:

| 指標                                                  |   開始 |     P3 終了 |
| ----------------------------------------------------- | -----: | ----------: |
| similar pair 数 (`--threshold 0.85 --exclude-tests`)  |    134 |          87 |
| Top hotspot score                                     |    170 |          80 |
| `analyze/similarity.rs::find_pairs` cognitive         |     17 |  消滅 (1)\* |
| `analyze/similarity.rs` 関数あたり最大 cognitive      |     17 |           5 |
| `analyze/mod.rs::resolve_crate_root` cognitive        |      9 |           5 |
| `main.rs::AnalyzeCommand::run` cyclomatic             |     14 |    14 (現状維持) |
| `lens-rust/coupling.rs` 上位 LCOM4                    |      5 |          5\*\* |
| nextest 合計                                          | 207\*\*\* | 801 (＋ 5 共通化テスト) |

\* `find_pairs` は `candidate_pairs` iterator + `score_pair` filter_map に分割
され、本体は `cog=1` のフラットなチェーンになった。最大 cog は別の関数
（`SimilarityAnalyzer::collect_directory`, cog=5）に移っている。

\*\* P3-1 で `visit_expr_path` ↔ `visit_type_path` の 100% 重複は解消したが、
LCOM4 のメトリクス自体は 5 のまま。これは現在の cohesion analyzer が
`impl Visit for X` 内のメソッドから `impl X` 内の inherent helper 呼び出しを
「兄弟呼び出し」として認識しないためで、コードの実質的な凝集は改善している。
将来 cohesion analyzer をクロス-impl 呼び出しまで追跡するように拡張すれば
自然に下がる予定。

\*\*\* P0 開始時点で計測した nextest は agent-lens crate 単体（207）。
P3 終了時点の 801 はワークスペース全体（lens-domain / lens-rust /
lens-ts / lens-py を含む）。

### 残課題（P4）

P4（Temporal Coupling / Token Budget）は既存メトリクスへのリファクタでなく
**新規 analyzer の追加** に当たる。リファクタリングのスコープ外として
未実施。実装時の参考だけ残す:

- **Temporal Coupling**: `git log --name-only` をパースし、ファイル対の
  共起 commit 数を集計。`compute_temporal_coupling(commit_files, min_co_changes,
  min_support)` を `lens-domain` に置き、CLI で `analyze temporal-coupling
  --since 30.days.ago --top 20` を生やす。
- **Token Budget**: ファイルごとの `chars/4` 概算を出す。pluggable な
  `Tokenizer` trait を `lens-domain` に置けば、将来 `tiktoken-rs` などの
  正確なトークナイザを差し替えられる。

> いずれも既存の `analyze hotspot` と同じ「git × ファイル」グルーで実装でき、
> P0–P3 の整理が乗ったあとなら数百行で追加できる見積もり。
| `lens-rust/coupling.rs` 上位 LCOM4                   |    5 |   ≤2 |
| `agent-lens/src/analyze/similarity.rs` LOC           |  715 | ≤500 |

> 重複ペア数は「真の重複が消えたか」と「テスト由来の偽陽性が増えていないか」の 2 つで見る。
> `--exclude-tests` を付けた値を ground truth にする。

## 5. やらないこと

- 言語アダプタを 1 つの crate に統合する（trait object のオーバーヘッドと
  parser の lifetime に踏み込む割に得が少ない）。
- 出力 format の変更（agent 側の hook 設定や下流ツールに影響する）。
- 新しい crate の追加（CLAUDE.md の「単一バイナリ」方針を維持）。
- `*::Default for *Hook` の自動生成化（trait と builder の絡みで複雑になる）。
