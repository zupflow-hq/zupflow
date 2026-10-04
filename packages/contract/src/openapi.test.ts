import { readFileSync, writeFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { buildOpenApi } from "./openapi";
import { routes } from "./routes";

const FILE = new URL("../openapi.json", import.meta.url);
const rendered = `${JSON.stringify(buildOpenApi(), null, 2)}\n`;

describe("openapi.json", () => {
  it("is up to date with the contract (run `pnpm --filter @zupflow/contract generate`)", () => {
    if (process.env.GENERATE_OPENAPI) writeFileSync(FILE, rendered);
    expect(readFileSync(FILE, "utf8")).toBe(rendered);
  });

  it("names every operation once and resolves every reference", () => {
    const ids = routes.map((r) => r.operationId);
    expect(new Set(ids).size).toBe(ids.length);
    const doc = buildOpenApi();
    const refs = [...rendered.matchAll(/"\$ref": "#\/components\/schemas\/([^"]+)"/g)].map((m) => m[1]);
    for (const ref of refs) expect(doc.components.schemas, ref).toHaveProperty(ref as string);
  });
});
