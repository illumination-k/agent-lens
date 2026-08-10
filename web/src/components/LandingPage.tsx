import { Link } from "@tanstack/react-router";
import type { ReactNode } from "react";

import "../landing.css";
import {
  ANALYZER_GROUPS,
  BLIND_SPOTS,
  FAQ,
  INSTALL_COMMAND,
  INSTALL_OPTIONS,
  LANGUAGES,
  PILLARS,
  SAMPLE_REPORT,
  TAGLINE,
} from "../content";
import { README_URL, RELEASES_URL, REPOSITORY_URL } from "../seo";

const NAV = [
  { href: "#why", label: "Why" },
  { href: "#analyzers", label: "Analyzers" },
  { href: "#languages", label: "Languages" },
  { href: "#install", label: "Install" },
  { href: "#faq", label: "FAQ" },
];

/**
 * The site's front door.
 *
 * Prerendered to static HTML, so the copy below is what a crawler reads —
 * headings, section landmarks, and the FAQ text that the route's `FAQPage`
 * JSON-LD mirrors.
 */
export function LandingPage() {
  return (
    <div className="landing">
      <SiteHeader />
      <main>
        <Hero />
        <BlindSpotSection />
        <PillarSection />
        <AnalyzerSection />
        <ReportSample />
        <LanguageSection />
        <InstallSection />
        <FaqSection />
      </main>
      <SiteFooter />
    </div>
  );
}

/**
 * One `<section>` of the page: anchor id, heading, optional standfirst, body.
 *
 * Every section below is that same shape, and `agent-lens analyze similarity`
 * said so about the first draft — the section functions scored 86% against
 * each other because the scaffolding was longer than the copy inside it.
 */
function Band({
  children,
  id,
  intro,
  title,
}: {
  children: ReactNode;
  id: string;
  intro?: ReactNode;
  title: string;
}) {
  const titleId = `${id}-title`;
  return (
    <section className="band" id={id} aria-labelledby={titleId}>
      <div className="section-head">
        <h2 id={titleId}>{title}</h2>
        {intro !== undefined && <p>{intro}</p>}
      </div>
      {children}
    </section>
  );
}

function SiteHeader() {
  return (
    <header className="site-header">
      <a className="wordmark" href="#top">
        agent-lens
      </a>
      <nav aria-label="Sections">
        {NAV.map((item) => (
          <a key={item.href} href={item.href}>
            {item.label}
          </a>
        ))}
        <Link to="/analyze">Live demo</Link>
        <a href={REPOSITORY_URL}>GitHub</a>
      </nav>
    </header>
  );
}

function Hero() {
  return (
    <section className="hero" id="top" aria-labelledby="hero-title">
      <p className="eyebrow">Rust CLI · MIT · pre-alpha</p>
      <h1 id="hero-title">{TAGLINE}</h1>
      <p className="lede">
        <strong>agent-lens</strong> is a single-binary code analysis CLI for coding agents. It
        answers the questions Claude Code and Codex cannot answer from the file they have open —{" "}
        <em>what already duplicates this?</em>, <em>how tangled is this module?</em>,{" "}
        <em>what breaks if I change it?</em> — and emits JSON or compact Markdown built for a
        context window instead of a terminal.
      </p>
      <Snippet command={INSTALL_COMMAND} label="Install command" />
      <div className="cta">
        <a className="button primary" href="#install">
          Install it
        </a>
        <Link className="button" to="/analyze">
          Open the function graph
        </Link>
        <a className="button ghost" href={REPOSITORY_URL}>
          Read the source
        </a>
      </div>
    </section>
  );
}

function BlindSpotSection() {
  return (
    <Band
      id="why"
      title="Agents decide on partial context"
      intro="An agent reads the file it is editing. What it does not read is the rest of the repository — so it forks a function that already exists, grows the module that is already a bottleneck, and refactors the one function nobody should touch without a plan."
    >
      <ul className="blind-spots">
        {BLIND_SPOTS.map((item) => (
          <li key={item}>{item}</li>
        ))}
      </ul>
      <p className="note">
        The stance is enforced in code, not by convention: <code>println!</code>,{" "}
        <code>eprintln!</code>, <code>unwrap()</code>, and <code>expect()</code> are all clippy{" "}
        <code>deny</code>. Stdout carries protocol payloads and reports; everything else goes to
        stderr through <code>tracing</code>, so a stray <code>dbg!</code> cannot corrupt a hook
        response.
      </p>
    </Band>
  );
}

function PillarSection() {
  return (
    <Band
      id="features"
      title="Three surfaces, one binary"
      intro="No service, no account, no telemetry — a static binary and a config file."
    >
      <div className="cards">
        {PILLARS.map((pillar) => (
          <article className="card" key={pillar.title}>
            <h3>{pillar.title}</h3>
            <p>{pillar.body}</p>
            <code className="inline-command">{pillar.command}</code>
          </article>
        ))}
      </div>
    </Band>
  );
}

function AnalyzerSection() {
  return (
    <Band
      id="analyzers"
      title="Eighteen analyzers"
      intro={
        <>
          Each one is a subcommand: <code>agent-lens analyze &lt;tool&gt; &lt;path&gt;</code>. JSON
          on stdout by default, <code>--format md</code> for the compact ranking, <code>--top</code>{" "}
          and <code>--min-score</code> to trim it, <code>--diff-only</code> to score just the
          functions your working tree touched.
        </>
      }
    >
      {ANALYZER_GROUPS.map((group) => (
        <div className="analyzer-group" key={group.title}>
          <h3>{group.title}</h3>
          <p className="group-blurb">{group.blurb}</p>
          <dl className="analyzer-list">
            {group.analyzers.map((analyzer) => (
              <div key={analyzer.name}>
                <dt>
                  <code>{analyzer.name}</code>
                </dt>
                <dd>{analyzer.summary}</dd>
              </div>
            ))}
          </dl>
        </div>
      ))}
    </Band>
  );
}

function ReportSample() {
  return (
    <Band
      id="output"
      title="Output an agent can act on"
      intro={
        <>
          Dense, ranked, and addressable by <code>file:line</code> — the Markdown form is meant to
          be pasted into a prompt whole.
        </>
      }
    >
      <pre className="sample">
        <code>{SAMPLE_REPORT}</code>
      </pre>
      <p className="note">
        A real run over this repository's own sources: agent-lens is pointed at itself on every
        change, and findings about its own code count as findings. Every report is also available as
        JSON — which is what the <Link to="/analyze">function graph viewer</Link> on this site
        renders.
      </p>
    </Band>
  );
}

function LanguageSection() {
  return (
    <Band
      id="languages"
      title="Languages"
      intro="A language-neutral core plus per-language adapters: the metrics are shared, so a new language is one adapter crate rather than a reimplementation."
    >
      <table className="matrix">
        <thead>
          <tr>
            <th scope="col">Language</th>
            <th scope="col">Parser</th>
            <th scope="col">Analyzer coverage</th>
          </tr>
        </thead>
        <tbody>
          {LANGUAGES.map((row) => (
            <tr key={row.language}>
              <th scope="row">{row.language}</th>
              <td>
                <code>{row.parser}</code>
              </td>
              <td>{row.coverage}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <p className="note">
        <code>unreachable</code> and <code>visibility</code> need extracted export status, which
        TypeScript and Python do not carry — so those two are wired through the Rust and Go adapters
        only.
      </p>
    </Band>
  );
}

function InstallSection() {
  return (
    <Band
      id="install"
      title="Install"
      intro="Pick one. The install script verifies the release SHA-256 and fails closed if it cannot."
    >
      <div className="cards">
        {INSTALL_OPTIONS.map((option) => (
          <article className="card" key={option.title}>
            <h3>{option.title}</h3>
            <p>{option.note}</p>
            <Snippet command={option.command} label={`${option.title} command`} />
          </article>
        ))}
      </div>
      <p className="note">
        Then wire it into your agent with <code>agent-lens hook setup</code>, or start with{" "}
        <code>agent-lens help --md</code> for the whole command surface as one Markdown document.
        Pre-built binaries for every tag live on the <a href={RELEASES_URL}>releases page</a>.
      </p>
    </Band>
  );
}

function FaqSection() {
  return (
    <Band id="faq" title="Questions">
      <div className="faq">
        {FAQ.map((entry) => (
          <details key={entry.question}>
            <summary>{entry.question}</summary>
            <p>{entry.answer}</p>
          </details>
        ))}
      </div>
    </Band>
  );
}

function SiteFooter() {
  return (
    <footer className="site-footer">
      <p>
        <strong>agent-lens</strong> — MIT licensed, pre-alpha, built in Rust.
      </p>
      <nav aria-label="Project links">
        <a href={REPOSITORY_URL}>Repository</a>
        <a href={README_URL}>Documentation</a>
        <a href={RELEASES_URL}>Releases</a>
        <Link to="/analyze">Function graph</Link>
      </nav>
    </footer>
  );
}

function Snippet({ command, label }: { command: string; label: string }) {
  return (
    <pre className="snippet" aria-label={label}>
      <code>{command}</code>
    </pre>
  );
}
