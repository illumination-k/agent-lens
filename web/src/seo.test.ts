import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import { FAQ } from "./content";
import {
  assetUrl,
  breadcrumbJsonLd,
  buildSitemap,
  canonicalUrl,
  faqJsonLd,
  PAGES,
  pageHead,
  SITE_URL,
  softwareApplicationJsonLd,
  type PageSeo,
} from "./seo";

const ALL_PAGES: PageSeo[] = Object.values(PAGES);

function metaContent(page: PageSeo, key: "name" | "property", value: string): string | undefined {
  for (const tag of pageHead(page).meta) {
    if (key in tag && (tag as Record<string, string>)[key] === value) {
      return (tag as Record<string, string>).content;
    }
  }
  return undefined;
}

function structuredData(page: PageSeo, data: object[]): unknown[] {
  return pageHead(page, data).scripts.map((script) => JSON.parse(script.children));
}

describe("canonicalUrl", () => {
  it.each([
    ["/", `${SITE_URL}/`],
    ["", `${SITE_URL}/`],
    ["/graph", `${SITE_URL}/graph/`],
    ["graph/", `${SITE_URL}/graph/`],
  ])("normalises %s to a trailing-slash absolute URL", (path, expected) => {
    expect(canonicalUrl(path)).toBe(expected);
  });
});

describe("assetUrl", () => {
  it("keeps a file URL free of the trailing slash pages carry", () => {
    expect(assetUrl("og-image.png")).toBe(`${SITE_URL}/og-image.png`);
    expect(assetUrl("/og-image.png")).toBe(`${SITE_URL}/og-image.png`);
  });
});

describe("PAGES", () => {
  it.each(ALL_PAGES)("keeps $path within the length a result snippet shows", (page) => {
    expect(page.title.length).toBeLessThanOrEqual(70);
    expect(page.description.length).toBeGreaterThanOrEqual(110);
    expect(page.description.length).toBeLessThanOrEqual(160);
  });

  it("gives every page its own title and description", () => {
    expect(new Set(ALL_PAGES.map((page) => page.title)).size).toBe(ALL_PAGES.length);
    expect(new Set(ALL_PAGES.map((page) => page.description)).size).toBe(ALL_PAGES.length);
  });
});

describe("pageHead", () => {
  it.each(ALL_PAGES)("points $path at itself as canonical", (page) => {
    const url = canonicalUrl(page.path);
    expect(pageHead(page).links).toEqual([{ rel: "canonical", href: url }]);
    expect(metaContent(page, "property", "og:url")).toBe(url);
  });

  it.each(ALL_PAGES)("mirrors the title and description of $path into the social tags", (page) => {
    expect(pageHead(page).meta[0]).toEqual({ title: page.title });
    expect(metaContent(page, "name", "description")).toBe(page.description);
    expect(metaContent(page, "property", "og:title")).toBe(page.title);
    expect(metaContent(page, "property", "og:description")).toBe(page.description);
    expect(metaContent(page, "name", "twitter:title")).toBe(page.title);
    expect(metaContent(page, "name", "twitter:description")).toBe(page.description);
  });

  it("asks to be indexed and ships an absolute card image", () => {
    expect(metaContent(PAGES.home, "name", "robots")).toBe("index, follow");
    expect(metaContent(PAGES.home, "name", "twitter:card")).toBe("summary_large_image");
    expect(metaContent(PAGES.home, "property", "og:image")).toBe(`${SITE_URL}/og-image.png`);
  });

  it("emits no script tag when a page has no structured data", () => {
    expect(pageHead(PAGES.graph).scripts).toEqual([]);
  });

  it("serialises structured data as ld+json", () => {
    const [script] = pageHead(PAGES.home, [softwareApplicationJsonLd()]).scripts;
    expect(script?.type).toBe("application/ld+json");
    expect(JSON.parse(script?.children ?? "")).toMatchObject({
      "@type": "SoftwareApplication",
      url: canonicalUrl(PAGES.home.path),
    });
  });
});

describe("structured data", () => {
  it("declares every block against the schema.org context", () => {
    const blocks = structuredData(PAGES.home, [
      softwareApplicationJsonLd(),
      faqJsonLd(FAQ),
      breadcrumbJsonLd([PAGES.home, PAGES.graph]),
    ]);
    for (const block of blocks) {
      expect(block).toMatchObject({ "@context": "https://schema.org" });
    }
  });

  it("carries the rendered FAQ text verbatim, so the two cannot drift", () => {
    expect(faqJsonLd(FAQ)).toMatchObject({
      mainEntity: FAQ.map((entry) => ({
        "@type": "Question",
        name: entry.question,
        acceptedAnswer: { "@type": "Answer", text: entry.answer },
      })),
    });
  });

  it("numbers a breadcrumb trail from one", () => {
    expect(breadcrumbJsonLd([PAGES.home, PAGES.graph])).toMatchObject({
      itemListElement: [
        { position: 1, item: canonicalUrl(PAGES.home.path) },
        { position: 2, item: canonicalUrl(PAGES.graph.path) },
      ],
    });
  });
});

describe("public/sitemap.xml", () => {
  it("lists exactly the routes PAGES declares", () => {
    expect(readFileSync(new URL("../public/sitemap.xml", import.meta.url), "utf8")).toBe(
      buildSitemap(ALL_PAGES),
    );
  });

  it("is the sitemap public/robots.txt points crawlers at", () => {
    const robots = readFileSync(new URL("../public/robots.txt", import.meta.url), "utf8");
    expect(robots).toContain(`Sitemap: ${SITE_URL}/sitemap.xml`);
  });
});
