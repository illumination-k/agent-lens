/// <reference types="vite/client" />

import { HeadContent, Outlet, Scripts, createRootRoute } from "@tanstack/react-router";
import type { ReactNode } from "react";

import "../styles.css";

// Only what every page shares. Title, description, canonical, Open Graph, and
// the JSON-LD blocks are per-route and come from `seo.ts` — a title declared
// here would be one more thing to keep in sync with the pages that override it.
export const Route = createRootRoute({
  component: RootComponent,
  head: () => ({
    links: [{ rel: "icon", type: "image/svg+xml", href: `${import.meta.env.BASE_URL}favicon.svg` }],
    meta: [
      { charSet: "utf-8" },
      { name: "viewport", content: "width=device-width, initial-scale=1" },
      { name: "theme-color", content: "#0b1622" },
      { name: "author", content: "illumination-k" },
    ],
  }),
});

function RootComponent() {
  return (
    <RootDocument>
      <Outlet />
    </RootDocument>
  );
}

function RootDocument({ children }: Readonly<{ children: ReactNode }>) {
  return (
    <html lang="en">
      <head>
        <HeadContent />
      </head>
      <body>
        {children}
        <Scripts />
      </body>
    </html>
  );
}
