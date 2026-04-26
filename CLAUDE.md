# CLAUDE.md

## Commands

Run `mise install` first to install all tools.

```bash
mise run ci       # Run all ci:* tasks
mise run fmt      # Run all fmt:* tasks
mise run lint     # Run all lint:* tasks
mise run test     # Run all test:* tasks
mise run mutants  # Run all mutants:* tasks (slow; not wired into ci)
```

## Tools

All tools are managed by mise. Run `mise install` to install them.

| Tool          | Purpose                                 |
| ------------- | --------------------------------------- |
| uv            | Python package manager                  |
| dprint        | Code formatter                          |
| prek          | Pre-commit hook runner                  |
| shfmt         | Shell script formatter                  |
| actionlint    | GitHub Actions linter                   |
| zizmor        | GitHub Actions security linter          |
| shellcheck    | Shell script linter                     |
| ghalint       | GitHub Actions linter                   |
| pinact        | Pin GitHub Actions versions to SHAs     |
| rust          | Rust toolchain                          |
| cargo-nextest | Fast Rust test runner                   |
| cargo-deny    | Dependency license and advisory checker |
| cargo-audit   | Security advisory checker for Rust      |
| cargo-mutants | Mutation testing for Rust               |

## Purpose

`agent-lens` は、コーディングエージェント（Claude Code 等）に **コードベースを
より深く見るためのレンズ** を提供する、単一バイナリの Rust CLI。

大きく 2 系統の機能を同じバイナリに束ねる：

1. **Hook 系** — Claude Code / Codex の Hook プロトコル（PreToolUse /
   PostToolUse / UserPromptSubmit / Stop / SubagentStop / SessionStart /
   PermissionRequest）に沿った stdin/stdout JSON 変換。`settings.json` の
   `hooks` から呼ばれる前提。現状は両エージェントで PostToolUse の
   `similarity` / `wrapper` が実装済み。残りの event は schema を
   `agent-hooks` crate に揃えてあるので、handler を増やす時に CLI に
   生やすだけで済む。
2. **Analyzer 系** — 人間向け lint ではなく、**agent にコンテキストとして食わせる
   ための情報** を出す解析器。実装済み：
   - **Similarity**（TSED / APTED ベースの near-duplicate 関数検出）
   - **Wrapper**（trivial adapter chain を介した forwarding 関数の検出）
   - **Cohesion**（`impl` 単位の LCOM4）
   - **Complexity**（関数単位の Cyclomatic / Cognitive / Nesting /
     Halstead / Maintainability Index）
   - **Coupling**（モジュール間 Number of Couplings / Fan-In / Fan-Out /
     Henry-Kafura IFC）
   - **Hotspot**（git の churn × 複雑度で「触るべき/危険な場所」を可視化）
   - **Context Span**（モジュールを理解するのに辿る必要があるモジュール／
     ファイル数。`coupling` の依存グラフの推移閉包）

   実装候補（後述「指標カタログ」）：
   - **Temporal Coupling**（git 履歴で同時に変更されやすいファイル対）
   - **Code Age / Ownership**（最終変更日と作者の偏り）
   - **Public API Surface**（pub 境界の広さと churn・破壊的変更リスク）
   - **Doc Coverage**（pub item の `///` カバレッジ）
   - **Dead / Unused public**（呼ばれない pub 項目）
   - **Token Budget**（tokenizer 換算でのファイルサイズと context window フィット）
   - その他、agent が推論するのに有用な指標は随時追加

> 通常の lint と違い、出力は **LLM のコンテキストに載せて意味がある形** にチューニ
> ングする（余計な装飾は削り、信号対雑音比を高める）。

## Architecture

### CLI 構造

単一バイナリ `agent-lens`、clap derive のサブコマンド方式。第一階層で hook /
codex-hook / analyze の 3 系統を分ける。Hook 系は対象エージェントごとにサブツリー
を分けて、プロトコルの差分（schema / event 名）が CLI 表面まで漏れないようにする：

```
agent-lens
├── hook                            # Claude Code Hook 仕様
│   └── post-tool-use
│       ├── similarity              # 編集ファイル中の near-duplicate 関数を報告
│       └── wrapper                 # 薄い forwarding 関数を報告
├── codex-hook                      # Codex Hook 仕様（複数ファイルの apply_patch 対応）
│   └── post-tool-use
│       ├── similarity
│       └── wrapper
└── analyze                         # オンデマンド解析（stdin 不要）
    ├── cohesion <path>             # impl 単位の LCOM4 凝集度
    ├── complexity <path>           # 関数単位の CC / Cognitive / Nesting / Halstead / MI
    ├── context-span <path>         # モジュールごとの推移閉包（読むべきファイル数）
    ├── coupling <path>             # モジュール間 Fan-In / Fan-Out / IFC
    ├── hotspot <path> [--since W] [--top N]
    ├── similarity <path> [--threshold N]
    └── wrapper <path>
```

未実装の hook event（`pre-tool-use` / `user-prompt-submit` / `stop` /
`subagent-stop` / Codex の `session-start` / `permission-request`）は schema
だけ `agent-hooks` に揃っており、handler が必要になった時点で同じ流儀で
サブコマンドを追加する。

### ディレクトリ構成

Cargo workspace。役割が違うものは crate を分け、`agent-lens` バイナリは
それらをまとめて clap で表面化する薄い層に保つ：

```
/
├── Cargo.toml                      # workspace ルート（resolver = "3", edition = 2024）
├── crates/
│   ├── agent-lens/                 # CLI バイナリ。clap derive ディスパッチのみ
│   │   └── src/
│   │       ├── main.rs             # トップレベルパース → run()
│   │       ├── lib.rs              # `analyze` / `hooks` を再エクスポート
│   │       ├── analyze/            # 各 analyzer サブコマンド
│   │       │   ├── cohesion.rs
│   │       │   ├── complexity.rs
│   │       │   ├── context_span.rs
│   │       │   ├── coupling.rs
│   │       │   ├── hotspot.rs
│   │       │   ├── similarity.rs
│   │       │   └── wrapper.rs
│   │       └── hooks/              # PostToolUse handler。core/ は両エージェント共通の土台
│   │           ├── core/
│   │           ├── post_tool_use/  # Claude Code 用
│   │           └── codex/post_tool_use/
│   ├── agent-hooks/                # Hook プロトコル schema（serde 型のみ）
│   │   └── src/{claude_code,codex}/
│   ├── lens-domain/                # 言語非依存の解析プリミティブ
│   │   └── src/{tree,apted,tsed,function,cohesion,complexity,coupling}.rs
│   ├── lens-rust/                  # syn ベースの Rust アダプタ
│   ├── lens-ts/                    # oxc ベースの TS / JS アダプタ
│   └── lens-py/                    # ruff_python_parser ベースの Python アダプタ
├── .cargo/mutants.toml             # cargo-mutants 設定（test_tool = nextest）
├── mise.base.toml / mise.rust.toml # tools と tasks（fmt / lint / test / ci / mutants）
└── .github/workflows/              # ci_rust, lint_base, mutants, codeql, dependency-review, …
```

責務分離の要点：

- **`lens-domain`** — TreeNode / APTED / TSED / FunctionDef / CohesionUnit /
  FunctionComplexity / CouplingReport といった「言語に依らない」部分。各言語
  アダプタはこの crate の trait を実装し、メトリクス本体（LCOM4・MI・IFC など）の
  ロジックはここに集約する。
- **`lens-{rust,ts,py}`** — 言語固有のパーサとアダプタ。AST → `TreeNode` /
  `FunctionDef` / `CouplingEdge` への変換だけを受け持つ。
- **`agent-hooks`** — Claude Code / Codex の stdin/stdout JSON 型と `Hook` trait。
  ドメインロジックは持たない。
- **`agent-lens`** — 上記 3 つを使う薄い CLI。stdin から JSON を読む glue・
  stdout への JSON / Markdown 書き出し・clap のサブコマンド配線が中心。

### 出力方針（agent 向けに寄せる）

- デフォルトは **JSON**（`stdout`）。agent が構造化データとして扱える形にする
- `--format md` で **agent friendly な markdown サマリ** を出せるようにする
  （人間の目視でも読めるが、主目的は LLM に context として食わせること）
- `stdout` は常にプロトコル／結果専用。ログは **必ず stderr**
- 直接の `println!` / `eprintln!` は禁止（clippy で `deny`）。stdout 出力は
  `serde_json` 等で構造化して書き、ログ・診断は `tracing` マクロ経由で stderr
  に流す。`unwrap()` / `expect()` も同様に `deny`

### Hook プロトコル

- stdin: Claude Code / Codex から渡される JSON
- stdout: `{"decision": "approve" | "block", ...}` 等、各エージェントの Hook 仕様に
  準拠した JSON
- 型は `agent-hooks` crate に集約（`claude_code` / `codex` モジュール）。
  仕様変更に 1 ヶ所で追従できるようにする
- 未知フィールドは許容、必須フィールド欠落は即エラーで非 0 終了
- 共通の `Hook` trait（`Input` / `Output` / `Error` 型を実装側で指定）を介して
  handler を書くので、CLI 側の glue は `read stdin → handle → write stdout` の
  数行で済む

#### Codex の差分

`agent-hooks::codex` は Claude Code とよく似た形をしているが、差分は以下：

- 全 payload が active な `model` slug を持つ
- `transcript_path` が nullable
- ターン scoped event は `turn_id` を持つ
- `PostToolUse` は `additionalContext` で developer 文脈を追記できる
- `PermissionRequest` は専用 event（通常の承認プロンプトの前に hook が
  approve / deny を返せる）
- `SessionStart` event がある（Claude Code 側にはない）
- `SubagentStop` event はない

### Analyzer 実装方針

- 各言語アダプタが `lens-domain::LanguageParser` を実装し、AST を共通の
  `TreeNode` / `FunctionDef` に正規化する。新しい言語を足すときは crate を
  1 つ追加して analyzer 側の `match` に arm を増やすだけ
- Rust: [`syn`](https://docs.rs/syn) full feature。`mod` ツリーを辿るために
  ファイルシステムも見る
- TypeScript / JavaScript: [oxc](https://oxc.rs/) (`oxc_parser` / `oxc_ast`)。
  関数宣言・メソッド・arrow 式を関数として拾う
- Python: [`ruff_python_parser`](https://docs.rs/ruff_python_parser)（ruff の
  パーサ部分）。トップレベルの `def` / `async def` とクラス内メソッドを拾う
- **Similarity**: 関数本体を正規化 AST に落とし、APTED で tree edit distance を
  取り、TSED で 0.0–1.0 のスコアに正規化。`--threshold` 以上のペアを報告
- **Wrapper**: 関数本体が「`?` / `.unwrap()` / `.into()` / `.await` 等の trivial
  adapter を短い chain だけ挟んだ別関数の forwarding」になっているケースを検出
- **Cohesion**: Rust の `impl` ブロック単位で LCOM4 を算出。フィールド共有
  グラフの連結成分数として実装
- **Complexity**: 関数単位で Cyclomatic（McCabe）、Cognitive（Sonar）、最大ネスト
  深度、Halstead Volume、Maintainability Index を算出。複合指標（Hotspot 等）の
  入力としても再利用する
- **Coupling**: Rust の `mod` 単位で Number of Couplings / Fan-In / Fan-Out /
  簡略 Henry-Kafura IFC（`(fan_in × fan_out)^2`）/ モジュール対の Inter-module
  Coupling（distinct シンボル数）を算出。クレートルート (`src/lib.rs` /
  `src/main.rs`) から `mod foo;` を辿り、`use` / 関数呼び出し / 型参照 /
  `impl OtherTrait for MyType` をエッジとして集める
- **Context Span**: `coupling` の依存グラフを再利用し、各モジュールから
  outgoing edges に沿って到達できる他モジュールを BFS で集めて返す。
  CLI 層では到達モジュール集合を `CrateModule.file` 経由でファイル
  パスにマップし、自身の所属ファイルを除いた**ユニークなファイル数**を
  「読むべきファイル数」として併記する。サイクルがあっても始点モジュールは
  自身の集合に含めない

### 追加候補の指標カタログ

「随時追加」の候補。`agent-lens` のコンセプト（agent 向けに信号密度の高い情報を
出す）に沿うものを優先する。実装難易度は AST のみ＝低、AST + git ＝中、ML/PDG
が要るもの＝高、を目安とする。

#### 複雑度・重複系

| 指標                             | 一行定義                                          | 入力           | 難易度 |
| -------------------------------- | ------------------------------------------------- | -------------- | ------ |
| Cyclomatic Complexity            | 関数の制御フロー分岐数 +1（McCabe）               | AST            | 低     |
| Cognitive Complexity             | ネスト深さで重み付けした人間視点の複雑度（Sonar） | AST            | 低     |
| Max Nesting Depth                | 制御構造の最大ネスト                              | AST            | 低     |
| Halstead / Maintainability Index | 演算子・オペランド数から導く可読性スコア          | AST            | 中     |
| Type-3 Clone                     | AST 正規化後の near-miss 重複                     | AST + 編集距離 | 中     |

#### git 履歴系

| 指標                 | 一行定義                                 | 入力          | 難易度 |
| -------------------- | ---------------------------------------- | ------------- | ------ |
| Hotspot              | churn × 複雑度。「触るべき・危険」な場所 | AST + git log | 中     |
| Temporal Coupling    | 同時に変更されやすいファイル対           | git log       | 中     |
| Code Age             | 行/ファイルの最終変更からの経過時間      | git blame/log | 低     |
| Code Ownership       | 主要作者比率（Major/Minor authors）      | git log       | 低     |
| Coverage Gap × Churn | 「変わるのにテストない」場所             | cov + git log | 中     |

#### 構造・API 系

| 指標                         | 一行定義                              | 入力           | 難易度 |
| ---------------------------- | ------------------------------------- | -------------- | ------ |
| Instability (I = Ce/(Ca+Ce)) | パッケージ単位の変更しやすさ          | 依存グラフ     | 中     |
| Cyclic dependencies          | モジュール間循環依存（SCC 検出）      | 依存グラフ     | 中     |
| Public API Surface           | pub 項目の数とシグネチャ複雑度・churn | AST + git      | 中     |
| Dead / Unused public         | 外から呼ばれない pub 項目             | AST + 呼出解析 | 中     |

> Fan-In / Fan-Out / Henry-Kafura IFC は `analyze coupling` として実装済み。
> 上記 Instability・Cyclic dependencies は同じ `coupling` モジュールの依存
> グラフを再利用して追加できる。

#### LLM コンテキスト系（agent-lens 独自色）

| 指標            | 一行定義                                          | 入力      | 難易度 |
| --------------- | ------------------------------------------------- | --------- | ------ |
| Token Budget    | tokenizer 換算でファイル/モジュールの実トークン数 | tokenizer | 低     |
| Doc Coverage    | pub item の `///` 付与率                          | AST       | 低     |
| Onboarding Cost | 複雑度＋依存幅＋doc 不足の合成スコア              | 複合      | 中     |

> Context Span は `analyze context-span` として実装済み。Onboarding Cost は
> `complexity` × `context-span` × `doc-coverage` の合成として、Doc Coverage が
> 入った時点で組み立てられる。

> 「人間向け lint」と違い、ここでの判断軸は **agent の意思決定に効くか** と
> **入力が現実的か**。コメント密度のような表層指標や、研究寄りの ML ベース
> 可読性スコアは、明確な agent 用途が見えるまで採用しない。

### ログ

- [`tracing`](https://docs.rs/tracing) + `tracing-subscriber` で stderr に出力
- `RUST_LOG` 環境変数でレベル制御（デフォルト `info`）
- `eprintln!` は使わず必ず `tracing` マクロ（`info!` / `warn!` / `error!` 等）
  を経由する。フォーマットやレベル制御を一元化するため

### Mutation Testing

- [`cargo-mutants`](https://mutants.rs/) でテストの実効性（assertion 不足や
  到達していない分岐）を検出する
- 設定は `.cargo/mutants.toml`（`test_tool = "nextest"` で nextest と統一）
- 通常 CI には含めない（重いため）。代わりに `.github/workflows/mutants.yml` が
  PR 差分のみを対象に `--in-diff` で走らせ、`workflow_dispatch` で全体実行も可能
- ローカルでは `mise run mutants:rust` か `mise run mutants` で実行
- `mutants.out/` は `.gitignore` 済み

## Notes

- **「agent 向け lint」という立ち位置を崩さない**：人間が読んで嬉しい装飾（色、
  アニメーション、絵文字）は analyzer 出力に入れない。信号密度を最優先する。
- Hook と Analyzer を同一バイナリに束ねるのは、ユーザー側の導入コスト（インストール
  1 回・`settings.json` の記述 1 種類）を下げるため。機能はサブコマンドで隔離する。
- 解析ロジックは crate を分けてある（`lens-domain` 言語非依存 + `lens-{rust,ts,py}`
  アダプタ）。新しい言語を足す作業は「アダプタ crate を 1 つ書いて
  `agent-lens::analyze` の `SourceLang` に arm を増やす」で閉じる
- 配布は `cargo install` と GitHub Releases のプリビルドバイナリ
  （`release-latest.yml` で main ビルドを rolling latest として公開）の両方を想定
- 将来的に MCP server 化も視野に入れるが、まずは CLI として完成度を上げる
- Codex 用 hook は protocol schema が一通り揃っているので、必要になった event
  から順に handler を生やしていく
