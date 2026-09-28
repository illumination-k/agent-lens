import type { CSSProperties, ReactNode } from "react";

import { PROGRESS, TARGETS, ratio, type TargetRow } from "../articleData";

/**
 * Charts for the call-graph accuracy article.
 *
 * Plain HTML rows with percentage-positioned marks rather than SVG: a row can
 * reflow its label above the track on a phone, where a scaled SVG would shrink
 * its text past reading. Tooltips are CSS (`data-tip` on a focusable mark), so
 * they work in the prerendered page before hydration.
 */

type Tick = { at: number; label: string };

function Axis({ ticks }: { ticks: readonly Tick[] }) {
  return (
    <div className="chart-row chart-axis" aria-hidden="true">
      <span />
      <div className="chart-track">
        {ticks.map((tick) => (
          <span key={tick.label} className="chart-tick" style={pos(tick.at)}>
            {tick.label}
          </span>
        ))}
      </div>
    </div>
  );
}

function Grid({ ticks }: { ticks: readonly Tick[] }) {
  return (
    <>
      {ticks.map((tick) => (
        <span key={tick.label} className="chart-grid" style={pos(tick.at)} aria-hidden="true" />
      ))}
    </>
  );
}

function Legend({ items }: { items: readonly { label: string; className: string }[] }) {
  return (
    <ul className="chart-legend">
      {items.map((item) => (
        <li key={item.label}>
          <span className={`chart-key ${item.className}`} aria-hidden="true" />
          {item.label}
        </li>
      ))}
    </ul>
  );
}

function Figure({
  caption,
  children,
  label,
}: {
  caption: ReactNode;
  children: ReactNode;
  label: string;
}) {
  return (
    <figure className="chart" aria-label={label}>
      {children}
      <figcaption>{caption}</figcaption>
    </figure>
  );
}

/** `left` as a percentage of the track, clamped to it. */
function pos(fraction: number): CSSProperties {
  return { left: `${(Math.min(1, Math.max(0, fraction)) * 100).toFixed(2)}%` };
}

const UNIT_TICKS: readonly Tick[] = [0, 0.25, 0.5, 0.75, 1].map((at) => ({
  at,
  label: at === 0 ? "0" : at.toFixed(2),
}));

/** Precision and static recall per project, as two bars from a zero baseline. */
export function AccuracyChart() {
  return (
    <Figure
      label="Precision and static recall per project"
      caption="Precision: share of resolved agent-lens edges the oracle confirms. Static recall: share of the oracle's static-dispatch edges agent-lens resolved."
    >
      <Legend
        items={[
          { label: "Precision", className: "s1" },
          { label: "Static recall", className: "s2" },
        ]}
      />
      {TARGETS.map((row) => (
        <div className="chart-row" key={row.target}>
          <span className="chart-label">
            {row.target}
            <span className="dim"> · {row.oracle}</span>
          </span>
          <div className="chart-track bars">
            <Grid ticks={UNIT_TICKS} />
            <Bar
              className="s1"
              value={row.tp / row.resolved}
              tip={`${row.target} precision ${ratio(row.tp, row.resolved)} (${row.tp} / ${row.resolved})`}
            />
            <Bar
              className="s2"
              value={row.found / row.staticPairs}
              tip={`${row.target} static recall ${ratio(row.found, row.staticPairs)} (${row.found} / ${row.staticPairs})`}
            />
          </div>
        </div>
      ))}
      <Axis ticks={UNIT_TICKS} />
    </Figure>
  );
}

function Bar({ className, tip, value }: { className: string; tip: string; value: number }) {
  return (
    <div className="chart-bar-line">
      <span
        className={`chart-bar ${className}`}
        style={{ width: `${(value * 100).toFixed(2)}%` }}
        tabIndex={0}
        data-tip={tip}
        aria-label={tip}
      />
      <span className="chart-value">{value.toFixed(3)}</span>
    </div>
  );
}

const PROGRESS_LO = 0.2;
const PROGRESS_TICKS: readonly Tick[] = [0.2, 0.4, 0.6, 0.8, 1].map((value) => ({
  at: (value - PROGRESS_LO) / (1 - PROGRESS_LO),
  label: value.toFixed(1),
}));

function progressPos(value: number): number {
  return (value - PROGRESS_LO) / (1 - PROGRESS_LO);
}

/** First run against each oracle vs today, one panel per metric. */
export function ProgressChart() {
  const panels = [
    { title: "Precision", before: "precisionBefore", after: "precisionAfter" },
    { title: "Static recall", before: "recallBefore", after: "recallAfter" },
  ] as const;
  return (
    <Figure
      label="Precision and static recall, first oracle run vs today"
      caption="Same two pinned targets per language, before and after the resolver fixes each oracle's disagreement list led to. The axis starts at 0.2."
    >
      <Legend
        items={[
          { label: "First run against the oracle", className: "before" },
          { label: "Today", className: "s1" },
        ]}
      />
      <div className="chart-panels">
        {panels.map((panel) => (
          <div key={panel.title}>
            <p className="chart-panel-title">{panel.title}</p>
            {PROGRESS.map((row) => {
              const before = Number(row[panel.before]);
              const after = Number(row[panel.after]);
              const lo = Math.min(before, after);
              const hi = Math.max(before, after);
              const tip = `${row.language} ${panel.title.toLowerCase()}: ${row[panel.before]} → ${row[panel.after]} (${row.targets})`;
              return (
                <div className="chart-row" key={row.language}>
                  <span className="chart-label">{row.language}</span>
                  <div
                    className="chart-track dumbbell"
                    tabIndex={0}
                    data-tip={tip}
                    aria-label={tip}
                  >
                    <Grid ticks={PROGRESS_TICKS} />
                    <span
                      className="chart-link"
                      style={{
                        ...pos(progressPos(lo)),
                        width: `${((progressPos(hi) - progressPos(lo)) * 100).toFixed(2)}%`,
                      }}
                    />
                    <span className="chart-dot before" style={pos(progressPos(before))} />
                    <span className="chart-dot s1" style={pos(progressPos(after))} />
                  </div>
                </div>
              );
            })}
            <Axis ticks={PROGRESS_TICKS} />
          </div>
        ))}
      </div>
    </Figure>
  );
}

const LOG_LO = -2; // 10 ms
const LOG_HI = 2.5; // ~316 s
const TIME_TICKS: readonly Tick[] = [
  { s: 0.01, label: "10 ms" },
  { s: 0.1, label: "100 ms" },
  { s: 1, label: "1 s" },
  { s: 10, label: "10 s" },
  { s: 100, label: "100 s" },
].map((tick) => ({ at: logPos(tick.s), label: tick.label }));

function logPos(seconds: number): number {
  return (Math.log10(seconds) - LOG_LO) / (LOG_HI - LOG_LO);
}

const F1_LO = 0.5;
const F1_TICKS: readonly Tick[] = [0.5, 0.6, 0.7, 0.8, 0.9, 1].map((value) => ({
  at: f1Pos(value),
  label: value.toFixed(1),
}));

// Headroom above 1.0, so the oracles' row and its label clear the legend.
const F1_HI = 1.06;

function f1Pos(value: number): number {
  return (value - F1_LO) / (F1_HI - F1_LO);
}

/** Harmonic mean of precision and static recall: one accuracy number per project. */
export function f1(row: Pick<TargetRow, "tp" | "resolved" | "found" | "staticPairs">): number {
  const precision = row.tp / row.resolved;
  const recall = row.found / row.staticPairs;
  return (2 * precision * recall) / (precision + recall);
}

/** `left` / `bottom` of a point in the plot, both as percentages. */
function xy(x: number, y: number): CSSProperties {
  return { left: `${(x * 100).toFixed(2)}%`, bottom: `${(y * 100).toFixed(2)}%` };
}

/**
 * Speed against accuracy, one pair of points per project: agent-lens where it
 * lands, the oracle at F1 1.0 (it is the reference) and its own wall-clock.
 * Up and to the left is better; the joining line is the trade.
 */
export function ParetoChart() {
  return (
    <Figure
      label="Wall-clock against F1 per project, agent-lens and its type-checker oracle"
      caption="x: wall-clock for the whole repository, log scale (each gridline is 10×). y: F1 of precision and static recall against the oracle; the oracle is the reference, so it sits at 1.0 by definition. Each line joins one project's pair. Up and to the left is better."
    >
      <Legend
        items={[
          { label: "agent-lens function-graph", className: "s1" },
          { label: "Type-checker oracle", className: "s2" },
        ]}
      />
      <div className="scatter">
        <span className="scatter-y-title" aria-hidden="true">
          F1
        </span>
        <div className="scatter-plot">
          {TIME_TICKS.map((tick) => (
            <span key={tick.label} className="chart-grid" style={pos(tick.at)} aria-hidden="true" />
          ))}
          {F1_TICKS.map((tick) => (
            <span
              key={tick.label}
              className="scatter-hgrid"
              style={{ bottom: `${(tick.at * 100).toFixed(2)}%` }}
              aria-hidden="true"
            >
              <span className="scatter-ytick">{tick.label}</span>
            </span>
          ))}
          <svg
            className="scatter-links"
            viewBox="0 0 100 100"
            preserveAspectRatio="none"
            aria-hidden="true"
          >
            {TARGETS.map((row) => (
              <line
                key={row.target}
                x1={logPos(row.agentLensSeconds) * 100}
                y1={100 - f1Pos(f1(row)) * 100}
                x2={logPos(row.oracleSeconds) * 100}
                y2={100 - f1Pos(1) * 100}
                vectorEffect="non-scaling-stroke"
              />
            ))}
          </svg>
          {TARGETS.map((row) => {
            const tip = `${row.target}: agent-lens ${seconds(row.agentLensSeconds)}, F1 ${f1(row).toFixed(3)} (precision ${ratio(row.tp, row.resolved)}, static recall ${ratio(row.found, row.staticPairs)})`;
            return (
              <span
                key={`${row.target}-al`}
                className="chart-dot s1 scatter-dot"
                style={xy(logPos(row.agentLensSeconds), f1Pos(f1(row)))}
                tabIndex={0}
                data-tip={tip}
                aria-label={tip}
              />
            );
          })}
          {TARGETS.map((row) => {
            const tip = `${row.target}: ${row.oracle} ${seconds(row.oracleSeconds)} (${Math.round(speedup(row))}× slower than agent-lens)`;
            return (
              <span
                key={`${row.target}-oracle`}
                className="chart-dot s2 scatter-dot"
                style={xy(logPos(row.oracleSeconds), f1Pos(1))}
                tabIndex={0}
                data-tip={tip}
                aria-label={tip}
              />
            );
          })}
          <span className="scatter-note" style={xy(logPos(0.012), f1Pos(0.76))}>
            agent-lens
            <br />
            27–154 ms
          </span>
          <span className="scatter-note side" style={xy(logPos(0.043), f1Pos(0.595))}>
            rust-lang/log (macros)
          </span>
          <span className="scatter-note top" style={xy(logPos(12), f1Pos(1))}>
            type-checker oracles · 1.5–101 s
          </span>
        </div>
        <div className="scatter-xaxis" aria-hidden="true">
          {TIME_TICKS.map((tick) => (
            <span key={tick.label} className="chart-tick" style={pos(tick.at)}>
              {tick.label}
            </span>
          ))}
        </div>
      </div>
    </Figure>
  );
}

export function seconds(value: number): string {
  return value < 1 ? `${Math.round(value * 1000)} ms` : `${value.toFixed(1)} s`;
}

export function speedup(row: Pick<TargetRow, "agentLensSeconds" | "oracleSeconds">): number {
  return row.oracleSeconds / row.agentLensSeconds;
}
