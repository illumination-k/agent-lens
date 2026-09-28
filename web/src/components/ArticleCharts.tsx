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
const LOG_HI = 3; // 1000 s
const SPEED_TICKS: readonly Tick[] = [
  { at: 0, label: "10 ms" },
  { at: 0.2, label: "100 ms" },
  { at: 0.4, label: "1 s" },
  { at: 0.6, label: "10 s" },
  { at: 0.8, label: "100 s" },
];

function logPos(seconds: number): number {
  return (Math.log10(seconds) - LOG_LO) / (LOG_HI - LOG_LO);
}

export function seconds(value: number): string {
  return value < 1 ? `${Math.round(value * 1000)} ms` : `${value.toFixed(1)} s`;
}

export function speedup(row: Pick<TargetRow, "agentLensSeconds" | "oracleSeconds">): number {
  return row.oracleSeconds / row.agentLensSeconds;
}

/** Wall-clock per project on a log axis: agent-lens and its oracle, joined. */
export function SpeedChart() {
  return (
    <Figure
      label="Wall-clock of agent-lens and each oracle per project, log scale"
      caption="Log scale: each gridline is 10× the one before. The label at the right is the speedup."
    >
      <Legend
        items={[
          { label: "agent-lens function-graph", className: "s1" },
          { label: "Type-checker oracle", className: "s2" },
        ]}
      />
      {TARGETS.map((row) => {
        const tip = `${row.target}: agent-lens ${seconds(row.agentLensSeconds)}, ${row.oracle} ${seconds(row.oracleSeconds)} (${Math.round(speedup(row))}× faster)`;
        const from = logPos(row.agentLensSeconds);
        const to = logPos(row.oracleSeconds);
        return (
          <div className="chart-row" key={row.target}>
            <span className="chart-label">
              {row.target}
              <span className="dim"> · {row.oracle}</span>
            </span>
            <div className="chart-track dumbbell" tabIndex={0} data-tip={tip} aria-label={tip}>
              <Grid ticks={SPEED_TICKS} />
              <span
                className="chart-link"
                style={{ ...pos(from), width: `${((to - from) * 100).toFixed(2)}%` }}
              />
              <span className="chart-dot s1" style={pos(from)} />
              <span className="chart-dot s2" style={pos(to)} />
              <span className="chart-end" style={pos(to)}>
                {Math.round(speedup(row))}×
              </span>
            </div>
          </div>
        );
      })}
      <Axis ticks={SPEED_TICKS} />
    </Figure>
  );
}
