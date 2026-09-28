import { Link } from "@tanstack/react-router";

import "../landing.css";
import "../article.css";
import { MEASURED_AT, MEASURED_COMMIT, PROGRESS, TARGETS, ratio, totals } from "../articleData";
import { REPOSITORY_URL } from "../seo";
import { AccuracyChart, ProgressChart, SpeedChart, seconds, speedup } from "./ArticleCharts";

const DOC_URL = `${REPOSITORY_URL}/blob/main/docs/callgraph-accuracy.md`;
const SCRIPTS_URL = `${REPOSITORY_URL}/tree/main/scripts/callgraph-accuracy`;

const ALL = totals(TARGETS);

/**
 * How accurate and how fast `agent-lens analyze function-graph` is, measured
 * against a type checker per language. Prerendered, so the prose and tables
 * below are what a crawler indexes.
 */
export function CallGraphAccuracyArticle() {
  return (
    <div className="landing">
      <header className="site-header">
        <Link className="wordmark" to="/">
          agent-lens
        </Link>
        <nav aria-label="Site">
          <Link to="/">Home</Link>
          <Link to="/analyze">Live demo</Link>
          <a href={REPOSITORY_URL}>GitHub</a>
        </nav>
      </header>
      <main>
        <article className="article">
          <header className="article-head">
            <p className="kicker">Benchmark · {MEASURED_AT}</p>
            <h1>How accurate is a call graph built without a type checker?</h1>
            <p className="lede">
              <code>agent-lens analyze function-graph</code> resolves calls from syntax alone. We
              scored it against a real type checker for each language — go-vta, pyright,
              rust-analyzer, and tsc — on eight open-source projects. Resolved edges are right{" "}
              <strong>{pct(ALL.tp, ALL.resolved)}</strong> of the time, it finds{" "}
              <strong>{pct(ALL.found, ALL.staticPairs)}</strong> of statically dispatched calls, and
              it does so <strong>{Math.round(speedup(ALL))}× faster</strong> than the oracles.
            </p>
          </header>

          <dl className="stats-strip">
            <div>
              <dt>Precision</dt>
              <dd>{ratio(ALL.tp, ALL.resolved)}</dd>
            </div>
            <div>
              <dt>Static recall</dt>
              <dd>{ratio(ALL.found, ALL.staticPairs)}</dd>
            </div>
            <div>
              <dt>agent-lens, 8 repos</dt>
              <dd>{seconds(ALL.agentLensSeconds)}</dd>
            </div>
            <div>
              <dt>Oracles, 8 repos</dt>
              <dd>{seconds(ALL.oracleSeconds)}</dd>
            </div>
          </dl>

          <section aria-labelledby="why-title">
            <h2 id="why-title">Why measure it</h2>
            <p>
              Half of agent-lens stands on one data structure. <code>hubs</code>,{" "}
              <code>impact</code>, <code>layers</code>, <code>risk</code>, <code>reach</code> and
              the <Link to="/analyze">function graph viewer</Link> all read the same static call
              graph, built by a resolver that parses each file (syn for Rust, oxc for TypeScript,
              ruff's parser for Python, tree-sitter for Go) and never runs a type checker, a build,
              or a language server. That is what keeps the binary fast and dependency-free — and it
              is also a guess. A graph that invents edges tells an agent that a harmless change has
              a blast radius; one that drops edges tells it dead code is safe to delete.
            </p>
            <p>
              So the guess needs a number. Hand-labelled suites such as PyCG's micro-benchmark are
              small and use their own edge definitions, so instead each language gets a{" "}
              <em>mechanical oracle</em>: the type checker the ecosystem already trusts. Humans only
              look at the pairs where the two sides disagree.
            </p>
          </section>

          <section aria-labelledby="method-title">
            <h2 id="method-title">How it is scored</h2>
            <ol className="steps">
              <li>
                <strong>Pin</strong> small, well-tested projects by full commit SHA, two per
                language.
              </li>
              <li>
                <strong>Run the oracle</strong>: go-vta over the packages and their tests; pyright's
                language server asked for the definition at every call site; rust-analyzer's call
                hierarchy; the TypeScript checker walking every call expression.
              </li>
              <li>
                <strong>Normalise</strong> both sides to function → function pairs: the innermost
                named function around the call site, and the function that runs. Builtins, stdlib
                and third-party code are out; constructors map to <code>__init__</code> /{" "}
                <code>constructor</code>.
              </li>
              <li>
                <strong>Score</strong>. Precision is the share of agent-lens <em>resolved</em> edges
                the oracle confirms. Recall is over the oracle's <em>static</em> dispatch edges — a
                direct call or a method on a concrete type — since interface and trait calls are not
                expected to resolve from syntax; agent-lens reports those as candidate sets instead.
              </li>
              <li>
                <strong>Adjudicate</strong> disagreements in a TOML file keyed by qualified name, so
                a verdict survives line shifts and is reused on every run.
              </li>
            </ol>
            <p className="note">
              Everything below counts every unadjudicated agent-lens-only pair as a false positive,
              so the precision is a lower bound. Full edge definition, oracle contracts and scoring
              rules: <a href={DOC_URL}>docs/callgraph-accuracy.md</a>; the harness:{" "}
              <a href={SCRIPTS_URL}>scripts/callgraph-accuracy</a>.
            </p>
          </section>

          <section aria-labelledby="accuracy-title">
            <h2 id="accuracy-title">Accuracy</h2>
            <p>
              Measured {MEASURED_AT} on agent-lens <code>{MEASURED_COMMIT}</code> with{" "}
              <code>mise run callgraph-accuracy</code>.
            </p>
            <AccuracyChart />
            <details className="table-view">
              <summary>Show as table</summary>
              <div className="table-scroll">
                <table className="matrix">
                  <thead>
                    <tr>
                      <th scope="col">Project</th>
                      <th scope="col">Oracle</th>
                      <th scope="col">Precision</th>
                      <th scope="col">Static recall</th>
                      <th scope="col">Left as candidates</th>
                    </tr>
                  </thead>
                  <tbody>
                    {TARGETS.map((row) => (
                      <tr key={row.target}>
                        <th scope="row">
                          {row.target} <span className="dim">{row.version}</span>
                        </th>
                        <td>{row.oracle}</td>
                        <td className="num">
                          {ratio(row.tp, row.resolved)}{" "}
                          <span className="dim">
                            ({row.tp} / {row.resolved})
                          </span>
                        </td>
                        <td className="num">
                          {ratio(row.found, row.staticPairs)}{" "}
                          <span className="dim">
                            ({row.found} / {row.staticPairs})
                          </span>
                        </td>
                        <td className="num">{row.onlyCandidate}</td>
                      </tr>
                    ))}
                    <tr className="total">
                      <th scope="row">All eight (micro-averaged)</th>
                      <td />
                      <td className="num">
                        {ratio(ALL.tp, ALL.resolved)}{" "}
                        <span className="dim">
                          ({ALL.tp} / {ALL.resolved})
                        </span>
                      </td>
                      <td className="num">
                        {ratio(ALL.found, ALL.staticPairs)}{" "}
                        <span className="dim">
                          ({ALL.found} / {ALL.staticPairs})
                        </span>
                      </td>
                      <td className="num">
                        {TARGETS.reduce((sum, row) => sum + row.onlyCandidate, 0)}
                      </td>
                    </tr>
                  </tbody>
                </table>
              </div>
            </details>
            <p>
              "Left as candidates" are static calls agent-lens saw but would not commit to: it
              emitted an ambiguous edge listing the possible callees, the right one among them. That
              is the resolver's deliberate trade: when a name is shared, report the set rather than
              guess, so precision stays high and an agent can still follow the candidates.
            </p>
          </section>

          <section aria-labelledby="progress-title">
            <h2 id="progress-title">What the oracles found</h2>
            <p>
              The point of an oracle is the disagreement list, not the headline. Reading it turned
              up concrete resolver bugs in every language; each fix landed with a metamorphic test
              and the numbers moved accordingly (first run against each oracle vs today, same
              targets):
            </p>
            <ProgressChart />
            <details className="table-view">
              <summary>Show as table</summary>
              <div className="table-scroll">
                <table className="matrix">
                  <thead>
                    <tr>
                      <th scope="col">Language</th>
                      <th scope="col">Targets</th>
                      <th scope="col">Precision</th>
                      <th scope="col">Static recall</th>
                    </tr>
                  </thead>
                  <tbody>
                    {PROGRESS.map((row) => (
                      <tr key={row.language}>
                        <th scope="row">{row.language}</th>
                        <td>{row.targets}</td>
                        <td className="num">
                          {row.precisionBefore} → {row.precisionAfter}
                        </td>
                        <td className="num">
                          {row.recallBefore} → <strong>{row.recallAfter}</strong>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            </details>
            <ul className="findings">
              <li>
                <strong>Go.</strong> Calls inside func literals were dropped; a method on a call's
                result (<code>NewDecoder(r).Decode(v)</code>) was read as the package-level function
                of that name; <code>wg.Add(1)</code> could bind to a free function <code>Add</code>.
              </li>
              <li>
                <strong>Python.</strong> The biggest jump. <code>import more_itertools as mi</code>{" "}
                over a <code>from .more import *</code> barrel left almost every test call
                unresolved; <code>from itertools import count</code> was bound to an in-repo{" "}
                <code>count</code>; <code>A()</code> did not reach <code>A.__init__</code>;{" "}
                <code>super().m()</code> could bind to the caller's own override.
              </li>
              <li>
                <strong>TypeScript.</strong> Functions inside a <code>namespace</code> were named
                without it, <code>new C()</code> recorded no call site, and calls through a local
                closure (<code>const parse = (s) =&gt; …</code>) fell through to name matching.
              </li>
              <li>
                <strong>Rust.</strong> Calls in macro arguments (<code>assert_eq!(f(x), …)</code>,{" "}
                <code>format!("{"{}"}", g())</code>) were invisible, and two <code>V::fmt</code> in
                one file (<code>Display</code> and <code>Debug</code>) lost all their edges. Making
                macro arguments visible is also why log's precision <em>fell</em>: it exposed calls
                to methods a macro generates, which have no node to resolve to.
              </li>
            </ul>
          </section>

          <section aria-labelledby="speed-title">
            <h2 id="speed-title">Speed</h2>
            <p>
              Wall-clock for the whole repository, warm caches, on a 4-vCPU Xeon @ 2.10GHz: median
              of 10 runs of <code>agent-lens analyze function-graph --format json</code>, median of
              3 runs of each oracle. The Go oracle is timed as a prebuilt binary, so its compile is
              excluded; pyright and rust-analyzer include starting the server and indexing, which is
              what a caller pays.
            </p>
            <SpeedChart />
            <details className="table-view">
              <summary>Show as table</summary>
              <div className="table-scroll">
                <table className="matrix">
                  <thead>
                    <tr>
                      <th scope="col">Project</th>
                      <th scope="col">Graph</th>
                      <th scope="col">agent-lens</th>
                      <th scope="col">Oracle</th>
                      <th scope="col">Speedup</th>
                    </tr>
                  </thead>
                  <tbody>
                    {TARGETS.map((row) => (
                      <tr key={row.target}>
                        <th scope="row">{row.target}</th>
                        <td className="num dim">
                          {row.nodes} fn · {row.edges} calls
                        </td>
                        <td className="num">{seconds(row.agentLensSeconds)}</td>
                        <td className="num">
                          {seconds(row.oracleSeconds)} <span className="dim">{row.oracle}</span>
                        </td>
                        <td className="num">{Math.round(speedup(row))}×</td>
                      </tr>
                    ))}
                    <tr className="total">
                      <th scope="row">All eight</th>
                      <td />
                      <td className="num">{seconds(ALL.agentLensSeconds)}</td>
                      <td className="num">{seconds(ALL.oracleSeconds)}</td>
                      <td className="num">{Math.round(speedup(ALL))}×</td>
                    </tr>
                  </tbody>
                </table>
              </div>
            </details>
            <p>
              Every project finishes in under 160 ms, which is what lets the graph analyzers run
              inside a hook or on every agent turn. The gap is smallest against go-vta and tsc
              (11–50×), which type-check a module in a single process, and largest against pyright
              and rust-analyzer (450–760×), where the oracle is a language server answering one
              request per call site. Construction cost on synthetic 1,024-function corpora for all
              four languages is gated in CI by the <code>function_graph</code> Criterion benchmark,
              so a slowdown fails the pull request that causes it.
            </p>
          </section>

          <section aria-labelledby="limits-title">
            <h2 id="limits-title">Limits</h2>
            <ul className="findings">
              <li>
                <strong>Dynamic dispatch is not resolved.</strong> Interface, trait and virtual
                calls are under 3% recalled as resolved edges in Go, Rust and TypeScript; they show
                up as candidate sets, which contain the right callee 73–100% of the time.
              </li>
              <li>
                <strong>Types still matter.</strong> The remaining static misses are method chains
                on returned values (<code>ok(1).andThen(…)</code>), methods inherited from a base
                class, and re-exports through <code>pub use m::*</code> — things only a type or a
                class hierarchy answers.
              </li>
              <li>
                <strong>Eight projects is a sample.</strong> The discovery runs behind each fix
                covered 20 more repositories (listed in the doc), where precision held at 0.97–0.99
                per language; they are not in the pinned set and none of their disagreements are
                adjudicated.
              </li>
            </ul>
            <p className="note">
              Reproduce with <code>mise run callgraph-accuracy [target…]</code>. It needs network
              access for the checkouts, the Go module proxy, PyPI and npm; output lands in{" "}
              <code>target/callgraph-accuracy/</code>.
            </p>
          </section>
        </article>
      </main>
      <footer className="site-footer">
        <p>
          <strong>agent-lens</strong> — MIT licensed, pre-alpha, built in Rust.
        </p>
        <nav aria-label="Project links">
          <Link to="/">Home</Link>
          <Link to="/analyze">Function graph</Link>
          <a href={DOC_URL}>Benchmark method</a>
          <a href={REPOSITORY_URL}>Repository</a>
        </nav>
      </footer>
    </div>
  );
}

function pct(numerator: number, denominator: number): string {
  return `${((numerator / denominator) * 100).toFixed(1)}%`;
}
