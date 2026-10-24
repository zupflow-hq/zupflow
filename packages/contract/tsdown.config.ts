import { defineConfig } from "tsdown";

// The published build: one ES module per entry with its declarations. In the
// workspace, `exports` points at the sources; `publishConfig.exports` points
// at these files.
export default defineConfig({
  entry: ["src/index.ts", "src/idempotency.ts", "src/ids.ts", "src/schemas.ts", "src/webhooks.ts"],
  format: "esm",
  dts: true,
  clean: true,
  platform: "neutral",
});
