import { defineConfig } from "oxlint";

export default defineConfig({
  categories: {
    correctness: "error",
    suspicious: "warn",
  },
  plugins: ["react", "typescript"],
  rules: {
    "react/react-in-jsx-scope": "off",
  },
  ignorePatterns: ["dist", "node_modules", "src/routeTree.gen.ts"],
});
