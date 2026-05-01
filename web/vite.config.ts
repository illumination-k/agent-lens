import { tanstackStart } from "@tanstack/react-start/plugin/vite";
import react from "@vitejs/plugin-react-swc";
import { defineConfig } from "vite";

const repositoryName = process.env.GITHUB_REPOSITORY?.split("/")[1] ?? "agent-lens";

export default defineConfig({
  base: process.env.GITHUB_ACTIONS ? `/${repositoryName}/` : "/",
  plugins: [
    tanstackStart({
      prerender: {
        autoSubfolderIndex: true,
        crawlLinks: true,
        enabled: true,
        failOnError: true,
      },
    }),
    react(),
  ],
});
