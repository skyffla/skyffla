import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

const configuredBase = process.env.VITE_BASE_PATH;
const repository = process.env.GITHUB_REPOSITORY?.split("/")[1];
const base =
  configuredBase ??
  (process.env.GITHUB_ACTIONS === "true" && repository ? `/${repository}/` : "/");

export default defineConfig({
  base,
  plugins: [react()],
});
