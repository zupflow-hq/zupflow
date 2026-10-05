import { describe, expect, it } from "vitest";
import { WEBHOOK_DELIVERY_TYPES, WEBHOOK_EVENT_TYPES, WebhookEvent } from "./schemas";

/** Every key an event's `data` may carry, per object: what routes it, never content. */
const ALLOWED_DATA: Record<string, readonly string[]> = {
  installation: ["object", "id", "app_id", "status"],
  connect_session: ["object", "id", "status", "installation_id", "failure_code"],
  run: ["object", "id", "agent_id", "status"],
  webhook_endpoint: ["object", "id"],
};

describe("webhook events", () => {
  it("gives every subscribable type a payload, plus the test button's", () => {
    expect([...WEBHOOK_DELIVERY_TYPES].sort()).toEqual([...WEBHOOK_EVENT_TYPES, "webhook.test"].sort());
  });

  it("is lean: data carries only the keys allowed for its object", () => {
    for (const variant of WebhookEvent.options) {
      const shape = variant.shape.data.shape as Record<string, unknown>;
      const object = (variant.shape.data.shape.object as { value: string }).value;
      const allowed = ALLOWED_DATA[object];
      expect(allowed, `${variant.shape.type.value}: an allow-list for ${object}`).toBeDefined();
      expect(Object.keys(shape), variant.shape.type.value).toEqual(allowed);
    }
  });

  it("narrows each event's status to what the event means", () => {
    const base = {
      id: "evt_034HXHH2LZFzvoaSAhfGVC",
      created_at: "2026-10-04T21:04:11Z",
      environment: "sandbox",
      organization_id: "org_034CmCMi0M7gup3a0htwn5",
      workspace: { id: "ws_034FpGDIRLk8Of6gikx8DG", external_id: null },
    };
    const run = { object: "run", id: "run_034HXHH2LZFzvoaSAhfGVD", agent_id: "agt_034HXHH2LZFzvoaSAhfGVC" };
    expect(
      WebhookEvent.safeParse({ ...base, type: "run.completed", data: { ...run, status: "completed" } }).success,
    ).toBe(true);
    expect(WebhookEvent.safeParse({ ...base, type: "run.completed", data: { ...run, status: "queued" } }).success).toBe(
      false,
    );
    expect(WebhookEvent.safeParse({ ...base, type: "webhook.test", data: run }).success).toBe(false);
  });
});
