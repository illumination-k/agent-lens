/**
 * Head metadata for the static site.
 *
 * Every route asks this module for its `<head>` tags instead of spelling out
 * its own, so canonical URLs, Open Graph, Twitter cards, and JSON-LD are built
 * one way and `seo.test.ts` can assert them without rendering React. The
 * sitemap is derived from the same `PAGES` table, so a new route cannot ship
 * with a stale `public/sitemap.xml` — the test compares the two.
 */

/**
 * Absolute origin the deployed site answers on.
 *
 * Canonical, Open Graph, and sitemap URLs must be absolute, and a relative
 * `import.meta.env.BASE_URL` cannot become one at build time — so the project
 * Pages URL is spelled out here. It is also the only place to change when the
 * site moves to a custom domain.
 */
export const SITE_URL = "https://illumination-k.github.io/agent-lens";
export const SITE_NAME = "agent-lens";
export const REPOSITORY_URL = "https://github.com/illumination-k/agent-lens";
export const RELEASES_URL = `${REPOSITORY_URL}/releases`;
export const LICENSE_URL = `${REPOSITORY_URL}/blob/main/LICENSE`;
export const README_URL = `${REPOSITORY_URL}#readme`;

const OG_IMAGE = "og-image.png";

export type PageSeo = {
  /** Route path, as the router spells it. */
  path: string;
  title: string;
  description: string;
};

export const PAGES = {
  home: {
    path: "/",
    title: "agent-lens — code analysis built for coding agents",
    description:
      "Single-binary Rust CLI that shows coding agents what one open file cannot: duplicate functions, tangled modules, complexity landmines, blast radius.",
  },
  analyze: {
    path: "/analyze",
    title: "Function graph viewer — agent-lens",
    description:
      "Explore the static call graph agent-lens extracts from a codebase: filter by module and weight nodes by calls, fan-in, complexity, or maintainability.",
  },
} as const satisfies Record<string, PageSeo>;

/** Absolute URL of a page, with the trailing slash the prerender emits. */
export function canonicalUrl(path: string): string {
  const trimmed = path.replace(/^\/+/, "").replace(/\/+$/, "");
  return trimmed === "" ? `${SITE_URL}/` : `${SITE_URL}/${trimmed}/`;
}

/** Absolute URL of a file served from `public/`. */
export function assetUrl(file: string): string {
  return `${SITE_URL}/${file.replace(/^\/+/, "")}`;
}

type MetaTag =
  | { title: string }
  | { name: string; content: string }
  | {
      property: string;
      content: string;
    };

type LinkTag = { rel: string; href: string };

type ScriptTag = { type: string; children: string };

export type HeadTags = {
  meta: MetaTag[];
  links: LinkTag[];
  scripts: ScriptTag[];
};

/**
 * The whole `<head>` payload for a route: title, description, robots, the
 * Open Graph / Twitter pair every social preview reads, the canonical link,
 * and the structured data blocks as `application/ld+json` scripts.
 */
export function pageHead(page: PageSeo, structuredData: object[] = []): HeadTags {
  const url = canonicalUrl(page.path);
  return {
    meta: [
      { title: page.title },
      { name: "description", content: page.description },
      { name: "robots", content: "index, follow" },
      { property: "og:type", content: "website" },
      { property: "og:site_name", content: SITE_NAME },
      { property: "og:title", content: page.title },
      { property: "og:description", content: page.description },
      { property: "og:url", content: url },
      { property: "og:image", content: assetUrl(OG_IMAGE) },
      { property: "og:image:width", content: "1200" },
      { property: "og:image:height", content: "630" },
      { name: "twitter:card", content: "summary_large_image" },
      { name: "twitter:title", content: page.title },
      { name: "twitter:description", content: page.description },
      { name: "twitter:image", content: assetUrl(OG_IMAGE) },
    ],
    links: [{ rel: "canonical", href: url }],
    scripts: structuredData.map((data) => ({
      type: "application/ld+json",
      children: JSON.stringify(data),
    })),
  };
}

/**
 * The two lines every structured-data block opens with. Factored out because
 * `analyze similarity` scored the builders below at 96% against each other
 * while the only thing they genuinely shared was this envelope.
 */
function jsonLd(type: string, body: object): object {
  return { "@context": "https://schema.org", "@type": type, ...body };
}

/** The tool itself: what a "software" rich result is built from. */
export function softwareApplicationJsonLd(): object {
  return jsonLd("SoftwareApplication", {
    name: SITE_NAME,
    alternateName: "agent lens",
    applicationCategory: "DeveloperApplication",
    applicationSubCategory: "Static code analysis",
    operatingSystem: "Linux, macOS",
    url: canonicalUrl(PAGES.home.path),
    description: PAGES.home.description,
    softwareHelp: README_URL,
    codeRepository: REPOSITORY_URL,
    programmingLanguage: "Rust",
    license: LICENSE_URL,
    isAccessibleForFree: true,
    offers: { "@type": "Offer", price: "0", priceCurrency: "USD" },
  });
}

export function webSiteJsonLd(): object {
  return jsonLd("WebSite", {
    name: SITE_NAME,
    url: canonicalUrl(PAGES.home.path),
    description: PAGES.home.description,
  });
}

export type FaqEntry = { question: string; answer: string };

export function faqJsonLd(entries: readonly FaqEntry[]): object {
  return jsonLd("FAQPage", {
    mainEntity: entries.map((entry) => ({
      "@type": "Question",
      name: entry.question,
      acceptedAnswer: { "@type": "Answer", text: entry.answer },
    })),
  });
}

export function breadcrumbJsonLd(trail: readonly PageSeo[]): object {
  return jsonLd("BreadcrumbList", {
    itemListElement: trail.map((page, index) => ({
      "@type": "ListItem",
      position: index + 1,
      name: page.title,
      item: canonicalUrl(page.path),
    })),
  });
}

/**
 * The `public/sitemap.xml` body. Checked against the committed file by
 * `seo.test.ts` rather than written at build time, so the file stays a plain
 * static asset and a forgotten route fails the test suite instead of shipping.
 */
export function buildSitemap(pages: readonly PageSeo[]): string {
  const entries = pages
    .map((page) => `  <url>\n    <loc>${canonicalUrl(page.path)}</loc>\n  </url>`)
    .join("\n");
  return `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
${entries}
</urlset>
`;
}
