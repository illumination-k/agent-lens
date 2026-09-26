# 静的解析サーベイ: トレンドと agent-lens の実装方針 (2026-09)

agent-lens に関係する論文・OSS を 4 領域で調べ、現状との差分から実装方針を整理した。
URL は調査時にサブエージェントが確認したもの。`[未検証]` は本文を読めていないもの。

## TL;DR

1. **方向性は正しい。** 決定的な AST 由来のグラフは LLM で作る KG より正確で安い
   ([2601.08773](https://arxiv.org/abs/2601.08773))。エージェントは任意で呼べる構造ツールを使いたがらず、
   受動的に注入したほうが効く ([CodeAnchor 2606.26979](https://arxiv.org/abs/2606.26979),
   [LSP study 2608.13568](https://arxiv.org/abs/2608.13568))。これは hook 中心の設計を支持する。
2. **エージェントの主な失敗モードは品質の侵食。** 重複の増加、再利用の欠落、複雑度の集中、リファクタリングの減少が起きる。
   プロンプトでは劣化の傾きが変わらない ([SlopCodeBench 2603.24755](https://arxiv.org/abs/2603.24755),
   [GitClear 2026](https://www.gitclear.com/the_ai_code_quality_maintainability_gap))。
   セッション差分 (`stop delta`) と `--diff-only` の価値はここにある。
3. **主なギャップ**
   - 所有権と truck factor
   - diff 単位の JIT リスク
   - clone の片側だけ変更した警告
   - test smell
   - 名前付きのアーキテクチャスメル
   - 意味解析バックエンド (oxc_semantic / SCIP)
   - 永続的なファクトキャッシュ

## 1. エージェント向けコード文脈

| 研究 / OSS | 要点 | agent-lens への示唆 |
| --- | --- | --- |
| [Aider repo map](https://aider.chat/2023/10/22/repomap.html) | tree-sitter の def/ref をパーソナライズド PageRank で順位付けし、トークン予算内に詰める | 既存の call graph と hubs で実装できる |
| [Agentless](https://arxiv.org/abs/2407.01489) (FSE'25) | file → function → line の階層的局所化と、シグネチャだけの skeleton 表示 | skeleton 出力モードを作る |
| [SWE-agent ACI](https://arxiv.org/abs/2405.15793) (NeurIPS'24) | 編集コマンドに lint ガードを入れると 15.0→18.0% | PostToolUse で即時フィードバックを返す設計の根拠 |
| [RepoGraph](https://arxiv.org/abs/2410.14684) (ICLR'25) / [LocAgent](https://arxiv.org/abs/2503.09089) (ACL'25) / [CodexGraph](https://aclanthology.org/2025.naacl-long.7/) | def/ref・call・import・inherit のグラフで局所化が大きく改善 | `graph-query` / `impact` の方向と一致 |
| [Codebase-Memory](https://arxiv.org/abs/2603.27277) (2026) | tree-sitter の KG を MCP で提供。品質は 83% (探索型は 92%) だがトークンは約 1/10 | 最も近い競合。トークン効率が価値の中心 |
| [CodeAnchor](https://arxiv.org/abs/2606.26979) (2026) | 任意ツールは無視され grep が使われる。トポロジを受動的に注入すると Pass@1 +3.4pp、分散は半減 | push 型 (hook) が有効 |
| [Evaluating AGENTS.md](https://arxiv.org/abs/2602.11988) (2026) | 汎用のリポジトリ概要は成功率を上げず、コストが 20% 以上増える | SessionStart の summary は短く具体的にし、効果を測る |
| [CodeNib](https://arxiv.org/abs/2607.25431) / [Devil in the Interface](https://arxiv.org/abs/2608.11386) (2026) | 構造化インターフェースで一貫性が最大 4.7 倍、トークンは 50–87% 減 | 出力形式を設計対象として扱う |
| [Serena](https://github.com/oraios/serena), [code-graph-rag](https://github.com/vitali87/code-graph-rag) | LSP や tree-sitter ベースの MCP サーバ | LSP は常にトークンの得になるわけではない |

**生成コードの品質**

- [GitClear 2025](https://www.gitclear.com/ai_assistant_code_quality_2025_research): 5 行以上の重複ブロックが 8 倍、moved code が 39.9% 減。
- [Cursor DiD (MSR'26)](https://arxiv.org/abs/2511.04427): 導入後に静的解析警告が +29.7%、複雑度が +40.7% で、どちらも持続する。
- [More Code, Less Reuse (MSR'26)](https://arxiv.org/abs/2601.21276): エージェントの PR は再利用の機会を逃すが、レビュアーの評価は高い。
- [Beyond Bug Fixes](https://arxiv.org/abs/2601.20109) / [Debt Behind the AI Boom](https://arxiv.org/abs/2603.28592): 問題の大半は smell で、24% が HEAD まで残る。
- [CodeTaste](https://arxiv.org/abs/2603.04177): エージェントは指示されたリファクタリングはこなすが、自分では見つけない。具体的な対象を渡すことが有効。
- ループ内の静的解析フィードバックが効くという報告:
  - [2508.14419](https://arxiv.org/abs/2508.14419): Pylint/Bandit との反復で可読性の問題が 80% 超から 11% に減る。
  - [IRIS (ICLR'25)](https://arxiv.org/abs/2405.17238)
  - [静的解析を報酬に使う RL](https://arxiv.org/abs/2605.17174)

**出力の設計**

- [Anthropic: Writing tools for agents](https://www.anthropic.com/engineering/writing-tools-for-agents): 次を推奨している。
  - 既定値つきの truncation / pagination
  - 簡潔版と詳細版の切り替え
  - 絞り込みを促す truncation メッセージ
- [Context rot](https://www.trychroma.com/research/context-rot): 上限よりずっと手前で性能が劣化する。
- [TOON](https://arxiv.org/abs/2603.03306): 表形式データは JSON よりトークンが 30–40% 少ない。

## 2. クローン / 類似度

agent-lens は TSED (APTED)、token、PDG (WL kernel)、LSH、types / blocks、`--paired-by` drift、test-redundancy を実装済み。
jscpd / CPD / dupl などの OSS より Type-3 への対応が広い。

- **TSED** ([ACL'24](https://arxiv.org/abs/2404.08817)): 実装済み。
  **SED-struct 下界** ([2609.03078](https://arxiv.org/abs/2609.03078)) は LSH と APTED の間に挟める安価な枝刈りフィルタ。
- **NIL** ([FSE'21](https://github.com/kusumotolab/NIL)): N-gram で候補を出し、token LCS で検証する。挿入を含む gapped Type-3 に強い。
  現状の token Jaccard は順序を無視している。
- **Siamese** ([EMSE'19](https://github.com/UCL-CREST/Siamese)) / **ECScan** ([2502.19219](https://arxiv.org/abs/2502.19219)):
  頻出ボイラープレートの重みを下げる (TF-IDF / 情報量)。block clone のノイズ対策になる。
- **学習ベース** (UniXcoder, [Moumoula FSE'25](https://arxiv.org/abs/2408.04430), [Chochlov 2025](https://arxiv.org/abs/2510.15480), [Are We There Yet? 2606.25272](https://arxiv.org/abs/2606.25272)):
  - Type-3 で構造手法を安定して上回るわけではない。
  - Type-4 は分布シフトに弱い。
  - 明確に勝るのは言語をまたぐクローンだけ。
  - 単一バイナリに入れるなら [model2vec-rs](https://github.com/MinishLab/model2vec-rs) の静的埋め込みを feature gate の裏に置く。
- **ベンチマーク**: BigCloneBench の semantic ラベルはサンプルの 93% が誤り ([2505.04311](https://arxiv.org/abs/2505.04311))。
  評価には [GPTCloneBench](https://github.com/srlabUsask/GPTCloneBench) を使う。
- **クローン管理**:
  - Type-3 clone の 17% が欠陥を含み、inconsistent change / late propagation はバグの兆候になる
    ([1611.08005](https://arxiv.org/abs/1611.08005), [Barbour ICSM'11](https://seal-queensu.github.io/publications/pdf/ICSME-Liliane-2011.pdf))。
  - AI 生成コードの Type-1/2 clone 率は最大 7.5% ([FSE'25](https://dl.acm.org/doi/10.1145/3729397))。
- **テスト**:
  - [DTM 2609.04205](https://arxiv.org/abs/2609.04205) は AST 類似度を使う決定的なテスト最小化で、agent-lens の設計と同じ方向。
  - test smell の検出器は Java / Python / JS にはある ([PyNose](https://arxiv.org/abs/2108.04639), [smelly-test](https://github.com/marabesi/smelly-test))。**Rust 向けは見つからなかった。**
  - LLM が生成したテストでは Assertion Roulette と Magic Number が多い ([2410.10628](https://arxiv.org/abs/2410.10628))。

## 3. メトリクス・アーキテクチャ・リポジトリマイニング

**メトリクスの妥当性**

- Cognitive Complexity は理解にかかる時間や主観評価とは相関するが、理解の正しさとは弱い相関しかない
  ([Muñoz Barón ESEM'20](https://arxiv.org/abs/2007.12520))。読解コストの代理指標として示すのが妥当。
- LOC と比べて優位性は小さい ([Lavazza JSS'22](https://dl.acm.org/doi/10.1016/j.jss.2022.111561))。
  LOC を併記する。
- MI はサイズでほぼ決まる ([van Deursen 2014](https://avandeursen.com/2014/08/29/think-twice-before-using-the-maintainability-index/))。
  参考値の扱いに下げる。
- 閾値はベンチマークの分位点や相対閾値で決める ([Alves ICSM'10](https://dl.acm.org/doi/10.1109/ICSM.2010.5609747),
  [RTTool](https://aserebre.win.tue.nl/ICSME2014TOOL.pdf))。現状は絶対値のカットオフ。

**履歴からの予測**

- プロセスメトリクスはコードメトリクスより予測力が高く安定している
  ([Rahman & Devanbu ICSE'13](https://dl.acm.org/doi/10.5555/2486788.2486846), [Majumder EMSE'22](https://arxiv.org/abs/2008.09569))。
- 所有権と minor contributor ([Bird FSE'11](https://www.microsoft.com/en-us/research/publication/dont-touch-my-code-examining-the-effects-of-ownership-on-software-quality/)) と
  truck factor ([Avelino ICPC'16](https://arxiv.org/abs/1604.06766)): **agent-lens には未実装** (git author は日付にしか使っていない)。
- [Code Red (TechDebt'22)](https://arxiv.org/abs/2203.04374): code health が低いと欠陥が約 15 倍。
  [Code for Machines (2026)](https://doi.org/10.1145/3793655.3793722): 健全なコードほど LLM のリファクタリングが壊れにくい。
  agent-lens の存在意義を直接支える結果。
- JIT 欠陥予測 ([Kamei TSE'13](https://posl.ait.kyushu-u.ac.jp/~kamei/publications/Kamei_TSE2013.pdf), [JITLine](https://arxiv.org/abs/2103.07068)):
  - 変更の特徴量 (規模、分散、エントロピー、経験) だけで工数 20% で欠陥の 35% を検出できる。
  - LLM の fine-tune 系は理解が怪しい ([ReDef](https://arxiv.org/abs/2509.09192))。
  - 説明可能な特徴量ベースで十分。

**アーキテクチャスメル**

- DV8 / hotspot patterns ([Mo, Cai, Kazman](https://par.nsf.gov/servlets/purl/10194572)) と
  Arcan / Designite の分類: unstable dependency、hub-like、unstable interface、god component、modularity violation、crossing、
  propagation cost / decoupling level。
- 現状は `hidden-coupling` が modularity violation に相当する。他は個別の指標があるだけで、名前付きのスメルになっていない。

**アーキテクチャ復元**

- 入力にする依存関係の精度はアルゴリズムの選択と同じくらい重要 ([Lutellier TSE'18](https://www.researchgate.net/publication/278404686))。
- LLM 支援では、構造的なクラスタを作り、命名は LLM に任せる形が主流 ([ArchAgent 2601.13007](https://arxiv.org/abs/2601.13007))。
  エビデンス付きのクラスタを出力してエージェントに命名させる。

**比較対象の OSS**

- CodeScene / [code-maat](https://github.com/adamtornhill/code-maat) / [git-truck](https://github.com/git-truck/git-truck): 所有権の軸
- tach / import-linter / dependency-cruiser / go-arch-lint: **宣言したルールでの境界検査** (agent-lens は推論のみ)
- [rust-code-analysis](https://github.com/mozilla/rust-code-analysis), lizard: メトリクスのクロスチェック

## 4. 解析基盤 (名前解決・call graph・インクリメンタル)

**名前解決**

- [stack-graphs](https://github.com/github/stack-graphs) は **2025-09 にアーカイブ済み** (RUSTSEC-2026-0303)。採用しない。
- [SCIP](https://github.com/sourcegraph/scip): コンパイラ精度の def/ref を得る最安の経路。
  - scip-typescript / scip-python / `rust-analyzer scip` / scip-go がある。
  - Rust の `scip` crate で読み込める。
  - インデックス作成に分単位かかるので analyze 向き。hook には向かない。
- [oxc_semantic / oxc_resolver](https://docs.rs/oxc_semantic): 既に依存している crate 群で、プロセス内・ミリ秒で動く。
  [knip v6](https://knip.dev/blog/knip-v6) は TS バックエンドをこれに置き換えた。**TS では最も費用対効果が高い**。
- Python: ruff の `ruff_python_semantic` (スコープと束縛)。[ty](https://github.com/astral-sh/ty) の意味モデルは API が安定してから。
- `ra_ap_*` (rust-analyzer) は毎週のバージョン変動とビルドの重さから、組み込みは避ける。
  gopls は外部プロセスとしてしか使えない。

**call graph の精度** (参考値)

- Python:
  - [PyCG](https://arxiv.org/abs/2103.00587): precision 99% / recall 70%
  - [HeaderGen](https://github.com/secure-software-engineering/HeaderGen): 95% / 95%
- JS ([比較研究 2405.07206](https://arxiv.org/abs/2405.07206)):
  - パーサのみの npm-cg: 91% / 68%
  - 名前マッチの Closure: 81% / 89%
  - [Jelly](https://github.com/cs-au-dk/jelly) は approximate interpretation で recall を 76→88% に上げた。
- agent-lens の gated な名前解決は npm-cg 程度と推定されるが、**未計測**。
- Go: CHA/RTA/VTA ([x/tools](https://pkg.go.dev/golang.org/x/tools/go/callgraph/vta)) があり、[deadcode](https://go.dev/blog/deadcode) は RTA ベースで `-whylive` を持つ。
- Rust: trait dispatch が構文解析だけでは弱点 ([Rupta CC'24](https://dl.acm.org/doi/10.1145/3640537.3641574))。

**インクリメンタル化**

- [salsa](https://github.com/salsa-rs/salsa) はデーモン化するまでは過剰。
- 当面の最適解は、(言語, adapter 版, blake3(content)) をキーにしたファイル単位ファクトのディスクキャッシュと、実行ごとの軽いグローバル結合。
  stack-graphs や Glean も同じ分割を使っている。
- Datalog ([ascent](https://github.com/s-arash/ascent)) は到達性やレイヤールールが増えてからで十分。

## 実装方針 (優先度順)

価値と工数の比で並べた。「hook 直結」は PreToolUse / PostToolUse / Stop にそのまま載せられるもの。

| # | 施策 | 根拠 | 工数 | hook 直結 |
| --- | --- | --- | --- | --- |
| 1 | **Clone-aware change guard**: diff が clone class や `--paired-by` ペアの片側だけを変えたとき、変更されていない兄弟を報告する | inconsistent change / late propagation、AI 生成の重複 | 低〜中 | ○ (PostToolUse, Stop) |
| 2 | **Reuse hint (PreToolUse)**: 新しく書く関数の名前・シグネチャ・本体で `search` と `similarity` を引き、既存の候補を最大 3 件注入する | More Code Less Reuse、GitClear、CodeAnchor の push 型 | 中 | ○ |
| 3 | **`analyze ownership`**: 貢献者数、minor contributor、最大所有者の比率、truck factor。`risk` / `hotspot` の軸にも加える | Bird'11、Avelino'16、CodeScene との最大の差 | 低 | △ |
| 4 | **Diff JIT risk**: Kamei の特徴量、複雑度の差分、co-change の「触り忘れ」を合成した説明可能なスコアを `footprint` に統合する | Kamei'13、JITLine | 中 | ○ (Stop) |
| 5 | **Erosion / verbosity 指標**: SlopCodeBench 互換 (CC>10 の関数に集まる CC×√SLOC の割合、clone 行の比率) を `baseline` と `stop delta` に加え、劣化の傾きを示す | SlopCodeBench、Cursor DiD | 低 | ○ (Stop) |
| 6 | **oxc_resolver / oxc_semantic を lens-ts に導入**: tsconfig paths・exports map・re-export を解決し、参照を束縛する | knip v6 | 低〜中 | — |
| 7 | **call graph 精度ベンチ**: PyCG micro-benchmark と SWARM-CG で `ResolutionMethod` ごとの P/R を測る | 以降の意味解析強化を計測可能にする | 低 | — |
| 8 | **`analyze test-smells`**: assertion roulette、assert のないテスト、sleep、条件分岐、magic number、ignored。4 言語対応 | Rust 向けツールは存在しない。LLM テストに多い | 中 | △ |
| 9 | **名前付きアーキテクチャスメル**: unstable dependency / hub-like / unstable interface / god component を既存グラフの join で出し、PageRank で重大度を付ける。propagation cost を baseline 指標に加える | Arcan、Designite、DV8 | 中 | — |
| 10 | **永続ファクトキャッシュ**: blake3 キーでファイル単位の CallShape / ImportShape を保存する | hook のレイテンシ | 中 | ○ |
| 11 | **閾値の較正**: リポジトリ内の分位点と相対閾値を baseline に保存し、LOC を併記、MI は参考値に格下げする | Alves'10、Lavazza'22 | 低 | △ |
| 12 | **類似度の精度と速度**: IDF でラベルの重みを下げ、TED 下界フィルタと NIL 式の LCS token 法を入れる | Siamese、ECScan、SED-struct、NIL | 低〜中 | — |
| 13 | **出力の工夫**: `concise` と `detailed` の切り替え、絞り込みを促す truncation 通知、表形式出力 (TOON / CSV) | Anthropic tools guide、TOON | 低 | ○ |
| 14 | **任意の意味解析バックエンド**: SCIP を取り込み、Rust の trait と Go の interface の辺を精密化し、`ResolutionMethod` に出所を記録する | SCIP | 中 | — |
| 15 | (実験) static embedding による `--method embed` を cargo feature で入れる。言語をまたぐクローンと Type-4 の候補生成用 | model2vec | 中〜高 | — |

**設計原則** (調査から導いたもの)

- 構文のみの高速経路を既定にする。意味解析は `SyntaxFact::Unknown` を `Known` に格上げする追加層として足す。
- 構造化した情報は hook で push する。汎用的な概要は流さず、編集箇所に紐づく具体的な対象だけを出す。
- 効果を自分で計測する。フックの有無でのトークン量と成功率を SWE-bench 系の少数タスクで A/B 比較する
  (AGENTS.md 研究が示すとおり、概要の注入はコストになりうる)。
