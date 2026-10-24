import { defineConfig } from "tsdown";

// The published build. `@zupflow/contract` stays an import, resolved from the
// published package, so both share one set of schemas.
export default defineConfig({
  entry: ["src/index.ts"],
  format: "esm",
  dts: true,
  clean: true,
  platform: "neutral",
  external: [/^@zupflow\/contract/],
});
