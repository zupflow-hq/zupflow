import { describe, expect, it } from "vitest";
import { canonicalizeEnvironmentUrl, environmentRewrite } from "./environment-url";

const input = (href: string) => {
  const out = environmentRewrite.input?.({ url: new URL(href, "http://console.test") });
  return out ? new URL(out).toString().replace("http://console.test", "") : undefined;
};
const output = (href: string) => {
  const out = environmentRewrite.output?.({ url: new URL(href, "http://console.test") });
  return out ? new URL(out).toString().replace("http://console.test", "") : undefined;
};

describe("environment in the path", () => {
  it("reads the Sandbox prefix into the router's env param", () => {
    expect(input("/northwind/sandbox/workspaces")).toBe("/northwind/workspaces?env=sandbox");
    expect(input("/northwind/sandbox")).toBe("/northwind?env=sandbox");
    expect(input("/northwind/sandbox/w/123/agents?tab=access")).toBe("/northwind/w/123/agents?tab=access&env=sandbox");
  });

  it("leaves Production and non-organization paths alone", () => {
    expect(input("/northwind/workspaces")).toBeUndefined();
    expect(input("/connect/abc")).toBeUndefined();
    expect(input("/sign-in")).toBeUndefined();
  });

  it("writes Sandbox back as a prefix and drops the query parameter", () => {
    expect(output("/northwind/workspaces?env=sandbox")).toBe("/northwind/sandbox/workspaces");
    expect(output("/northwind?env=sandbox")).toBe("/northwind/sandbox");
    expect(output("/northwind/w/123/agents?tab=access&env=sandbox")).toBe("/northwind/sandbox/w/123/agents?tab=access");
  });

  it("never shows env for Production", () => {
    expect(output("/northwind/workspaces?env=production")).toBe("/northwind/workspaces");
    expect(output("/northwind/workspaces")).toBeUndefined();
  });

  it("round-trips", () => {
    const external = "/northwind/sandbox/runs?status=failed";
    expect(output(input(external) ?? "")).toBe(external);
  });
});

describe("canonicalizing older links", () => {
  it("moves ?env=sandbox into the path and drops ?env=production", () => {
    for (const [from, to] of [
      ["/northwind/runs?env=sandbox&status=failed", "/northwind/sandbox/runs?status=failed"],
      ["/northwind/apps?env=production", "/northwind/apps"],
    ]) {
      window.history.replaceState(null, "", from);
      canonicalizeEnvironmentUrl();
      expect(window.location.pathname + window.location.search).toBe(to);
    }
  });
});
