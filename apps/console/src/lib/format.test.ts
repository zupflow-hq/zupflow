import { describe, expect, it } from "vitest";
import { appKeyFrom, shortId } from "./format";

describe("shortId", () => {
  it("keeps the prefix and the random tail, which differs between IDs made together", () => {
    expect(shortId("run_034UcfT3q3x10VxjKfXLB3")).toBe("run_…KfXLB3");
    expect(shortId("run_034UcfT3q3x10VxjKfXLB3")).not.toBe(shortId("run_034UcfT3pyKBdZvxYty3aT"));
  });

  it("leaves anything that isn't a full ID alone", () => {
    expect(shortId("user_1")).toBe("user_1");
    expect(shortId("abc")).toBe("abc");
  });
});

describe("appKeyFrom", () => {
  it("suggests a snake-case key that starts with a letter and fits 24 characters", () => {
    expect(appKeyFrom("Google Workspace")).toBe("google_workspace");
    expect(appKeyFrom("  Café -- Orders! ")).toBe("cafe_orders");
    expect(appKeyFrom("3D Printer")).toBe("d_printer");
    expect(appKeyFrom("A very long application name indeed")).toBe("a_very_long_application");
    expect(appKeyFrom("123")).toBe("");
  });
});
