import { WEBHOOK_DELIVERY_TYPES, WEBHOOK_EVENT_TYPES } from "@zupflow/sdk";
import { describe, expect, it } from "vitest";
import { eventGroups } from "./event-groups";

describe("webhook event groups", () => {
  it("group every event type an endpoint subscribes to by what it's about, in order", () => {
    const groups = eventGroups(WEBHOOK_EVENT_TYPES);
    expect(groups.map((g) => g.label)).toEqual([
      "Installations",
      "Connect sessions",
      "Runs",
      "Apps",
      "Channels",
      "Memory",
    ]);
    expect(groups.find((g) => g.key === "memory")?.types).toEqual([
      "memory.saved",
      "memory.updated",
      "memory.forgotten",
    ]);
    expect(groups.flatMap((g) => g.types)).toHaveLength(WEBHOOK_EVENT_TYPES.length);
    expect(new Set(groups.flatMap((g) => g.types))).toEqual(new Set(WEBHOOK_EVENT_TYPES));
  });

  it("put the test event delivery history filters by in a group of its own", () => {
    const groups = eventGroups(WEBHOOK_DELIVERY_TYPES);
    expect(groups.find((g) => g.key === "webhook")).toEqual({
      key: "webhook",
      label: "Test events",
      types: ["webhook.test"],
    });
    expect(groups.flatMap((g) => g.types)).toHaveLength(WEBHOOK_DELIVERY_TYPES.length);
  });

  it("name a group no label covers after its resource", () => {
    expect(eventGroups(["usage_report.ready", "channel_thread.closed"])).toEqual([
      { key: "usage_report", label: "Usage report", types: ["usage_report.ready"] },
      { key: "channel", label: "Channels", types: ["channel_thread.closed"] },
    ]);
  });
});
