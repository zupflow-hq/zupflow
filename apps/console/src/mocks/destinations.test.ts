import { describe, expect, it } from "vitest";
import { isPublicHost } from "./destinations";

const host = (url: string) => new URL(url).hostname;

describe("isPublicHost", () => {
  it("refuses what the backend refuses", () => {
    for (const url of [
      "https://localhost/",
      "https://localhost../",
      "https://a.local../",
      "https://api.localhost/",
      "https://printer.local/",
      "https://metadata.google.internal/",
      "https://169.254.169.254/",
      "https://127.1/",
      "https://2130706433/",
      "https://10.0.0.1/",
      "https://100.64.0.1/",
      "https://[::1]/",
      "https://[::ffff:10.0.0.1]/",
      "https://[fd00::1]/",
      "https://[fe80::1]/",
      "https://[64:ff9b::a00:1]/",
      "https://[2002:a00:1::1]/",
      "https://[2001::1]/",
    ]) {
      expect(isPublicHost(host(url)), url).toBe(false);
    }
  });

  it("allows public hosts, including names that merely start like private ones", () => {
    for (const url of [
      "https://mcp.linear.app/",
      "https://10.example.com/",
      "https://localhost.example.com/",
      "https://8.8.8.8/",
      "https://[2606:4700:4700::1111]/",
      "https://[::ffff:8.8.8.8]/",
    ]) {
      expect(isPublicHost(host(url)), url).toBe(true);
    }
  });
});
