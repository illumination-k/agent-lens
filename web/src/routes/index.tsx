import { createFileRoute } from "@tanstack/react-router";

import { LandingPage } from "../components/LandingPage";
import { FAQ } from "../content";
import { PAGES, faqJsonLd, pageHead, softwareApplicationJsonLd, webSiteJsonLd } from "../seo";

export const Route = createFileRoute("/")({
  component: LandingPage,
  head: () => pageHead(PAGES.home, [softwareApplicationJsonLd(), webSiteJsonLd(), faqJsonLd(FAQ)]),
});
