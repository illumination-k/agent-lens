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

1. **Hook 系** — Claude Code の Hook プロトコル（PreToolUse / PostToolUse /
   UserPromptSubmit / Stop / SubagentStop）に沿った stdin/stdout JSON 変換。
   `settings.json` の `hooks` から呼ばれる前提。
2. **Analyzer 系** — 人間向け lint ではなく、**agent にコンテキストとして食わせる
   ための情報** を出す解析器。例：
   - **Hotspot**（git の churn × 複雑度で「触るべき/危険な場所」を可視化）
   - **Function similarity**（似た関数・重複ロジックの検出）
   - **Cohesion / Coupling**（関数・モジュールの凝集度・結合度）
   - **Complexity**（関数単位の Cyclomatic / Cognitive / Nesting depth /
     Halstead / Maintainability Index）
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

単一バイナリ `agent-lens`、clap derive のサブコマンド方式。第一階層で hook / analyze
の 2 系統を分ける：

```
agent-lens
├── hook
│   ├── pre-tool-use <name>          # Hook 系：stdin/stdout は Claude Code 仕様
│   ├── post-tool-use <name>
│   ├── user-prompt-submit <name>
│   ├── stop <name>
│   └── subagent-stop <name>
└── analyze
    ├── hotspot [--since <rev>]      # git churn × 複雑度
    ├── similarity [--threshold N]   # 関数類似度
    ├── cohesion [--path <glob>]     # 凝集度 / LCOM 系
    ├── complexity <path>            # 関数単位の CC / Cognitive / Nesting / MI
    └── ...                          # 追加指標
```

### ディレクトリ構成（想定）

```
/
├── src/
│   ├── main.rs                 # clap derive でトップレベルディスパッチ
│   ├── hooks/
│   │   ├── mod.rs
│   │   ├── pre_tool_use/
│   │   ├── post_tool_use/
│   │   ├── user_prompt_submit/
│   │   └── stop/
│   ├── analyze/
│   │   ├── mod.rs
│   │   ├── hotspot.rs          # git 履歴 × 複雑度
│   │   ├── similarity.rs       # AST ベースの関数類似度
│   │   └── cohesion.rs         # LCOM 系メトリクス
│   ├── io.rs                   # stdin/stdout JSON I/O 共通ヘルパ
│   ├── schema.rs               # Claude Code Hook I/O 型（serde）
│   └── report.rs               # agent 向け出力フォーマット（JSON / MD）
└── tests/                      # 入出力スナップショット・小規模プロジェクトの回帰
```

### 出力方針（agent 向けに寄せる）

- デフォルトは **JSON**（`stdout`）。agent が構造化データとして扱える形にする
- `--format md` で **agent friendly な markdown サマリ** を出せるようにする
  （人間の目視でも読めるが、主目的は LLM に context として食わせること）
- `stdout` は常にプロトコル／結果専用。ログは **必ず stderr**
- 直接の `println!` / `eprintln!` は禁止（clippy で `deny`）。stdout 出力は
  `serde_json` 等で構造化して書き、ログ・診断は `tracing` マクロ経由で stderr
  に流す。`unwrap()` / `expect()` も同様に `deny`

### Hook プロトコル

- stdin: Claude Code から渡される JSON
- stdout: `{"decision": "approve" | "block", ...}` 等、Claude Code の Hook 仕様に
  準拠した JSON
- 型は `src/schema.rs` に集約し、仕様変更に 1 ヶ所で追従できるようにする
- 未知フィールドは許容、必須フィールド欠落は即エラーで非 0 終了

### Analyzer 実装方針

- Rust 解析は [`syn`](https://docs.rs/syn) / [`tree-sitter`](https://tree-sitter.github.io/)
  を使い分ける想定。第 1 言語は Rust、次点で TS / Python を追加予定
- **Hotspot**: `git log --numstat` 等で churn を取り、複雑度（CC / LOC）と掛け合わせる
- **Similarity**: AST を正規化 → トークン列を winnowing / MinHash で近似比較
- **Cohesion**: モジュール単位で LCOM4 / 相互参照グラフをベースに算出
- **Complexity**: 関数単位で Cyclomatic（McCabe）、Cognitive（Sonar）、最大ネスト
  深度、Halstead Volume、Maintainability Index を算出。Hotspot の「複雑度」入力
  としても再利用する

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

| 指標                         | 一行定義                              | 入力              | 難易度 |
| ---------------------------- | ------------------------------------- | ----------------- | ------ |
| Fan-in / Fan-out             | 呼ばれる側 / 呼ぶ側の数               | AST + import 解決 | 中     |
| Instability (I = Ce/(Ca+Ce)) | パッケージ単位の変更しやすさ          | 依存グラフ        | 中     |
| Cyclic dependencies          | モジュール間循環依存（SCC 検出）      | 依存グラフ        | 中     |
| Public API Surface           | pub 項目の数とシグネチャ複雑度・churn | AST + git         | 中     |
| Dead / Unused public         | 外から呼ばれない pub 項目             | AST + 呼出解析    | 中     |

#### LLM コンテキスト系（agent-lens 独自色）

| 指標            | 一行定義                                               | 入力       | 難易度 |
| --------------- | ------------------------------------------------------ | ---------- | ------ |
| Token Budget    | tokenizer 換算でファイル/モジュールの実トークン数      | tokenizer  | 低     |
| Context Span    | 関数を理解するのに辿る必要があるファイル数（推移閉包） | 依存グラフ | 中     |
| Doc Coverage    | pub item の `///` 付与率                               | AST        | 低     |
| Onboarding Cost | 複雑度＋依存幅＋doc 不足の合成スコア                   | 複合       | 中     |

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
  アニメーション、絵文字）は入れない。信号密度を最優先する。
- Hook と Analyzer を同一バイナリに束ねるのは、ユーザー側の導入コスト（インストール
  1 回・`settings.json` の記述 1 種類）を下げるため。機能はサブコマンドで隔離する。
- Analyzer はまず Rust コードベースを対象に実装し、他言語は tree-sitter 経由で
  段階的に広げる。
- 配布は `cargo install agent-lens` と GitHub Releases のプリビルドバイナリの
  両方を想定。
- 将来的に MCP server 化も視野に入れるが、まずは CLI として完成度を上げる。
