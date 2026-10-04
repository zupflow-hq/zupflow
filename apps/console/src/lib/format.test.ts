import { describe, expect, it } from "vitest";
import { shortId } from "./format";

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
