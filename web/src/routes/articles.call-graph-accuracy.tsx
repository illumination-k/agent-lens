import { createFileRoute } from "@tanstack/react-router";

import { CallGraphAccuracyArticle } from "../components/CallGraphAccuracyArticle";
import { PAGES, articleJsonLd, breadcrumbJsonLd, pageHead } from "../seo";

export const Route = createFileRoute("/articles/call-graph-accuracy")({
  component: CallGraphAccuracyArticle,
  head: () =>
    pageHead(PAGES.callGraphAccuracy, [
      articleJsonLd(PAGES.callGraphAccuracy, "2026-09-28"),
      breadcrumbJsonLd([PAGES.home, PAGES.callGraphAccuracy]),
    ]),
});
