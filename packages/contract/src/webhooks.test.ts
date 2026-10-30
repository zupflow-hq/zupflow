import { describe, expect, it } from "vitest";
import { WEBHOOK_DELIVERY_TYPES, WEBHOOK_EVENT_TYPES, WebhookEvent } from "./schemas";

/** Every key an event's `data` may carry, per object: what routes it, never content. */
const ALLOWED_DATA: Record<string, readonly string[]> = {
  installation: ["object", "id", "app_id", "status"],
  connect_session: ["object", "id", "status", "installation_id", "target", "failure_code"],
  run: ["object", "id", "agent_id", "status"],
  webhook_endpoint: ["object", "id"],
  channel_message: ["object", "id", "connection_id", "conversation_id"],
  channel_app_manager: ["object", "id", "status"],
  channel_app: ["object", "id", "agent_id", "status"],
  channel_connection: ["object", "id", "app_id", "status"],
  channel_conversation: ["object", "id", "connection_id", "paused_until"],
  memory: ["object", "id", "agent_id", "memory_type_id", "origin"],
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

  it("names a memory saved, changed or forgotten, and who wrote its text, never the text", () => {
    const base = {
      id: "evt_034HXHH2LZFzvoaSAhfGVC",
      created_at: "2026-10-04T21:04:11Z",
      environment: "production",
      organization_id: "org_034CmCMi0M7gup3a0htwn5",
      workspace: { id: "ws_034FpGDIRLk8Of6gikx8DG", external_id: "tenant_1" },
    };
    const memory = {
      object: "memory",
      id: "memo_034HXHH2LZFzvoaSAhfGVE",
      agent_id: "agt_034HXHH2LZFzvoaSAhfGVC",
      memory_type_id: "mtype_034HXHH2LZFzvoaSAhfGVF",
      origin: { type: "api_key", api_key_id: "key_034HXHH2LZFzvoaSAhfGVG" },
    };
    const parses = (type: string, data: unknown) => WebhookEvent.safeParse({ ...base, type, data }).success;
    for (const type of ["memory.saved", "memory.updated", "memory.forgotten"]) {
      expect(parses(type, memory), type).toBe(true);
      // The text is the memory's to read, never the event's: a sender's is dropped.
      const read = WebhookEvent.parse({ ...base, type, data: { ...memory, text: "Prefers short answers." } });
      expect(read.data, type).toEqual(memory);
      const { memory_type_id: _type, ...unnamed } = memory;
      expect(parses(type, unnamed), `${type} without its type`).toBe(false);
    }
    // Each origin, by its type.
    for (const origin of [
      {
        type: "run",
        run_id: "run_034HXHH2LZFzvoaSAhfGVD",
        tool_call_id: "call_034HXHH2LZFzvoaSAhfGVD",
        after_app_content: true,
      },
      { type: "member", user_id: "usr_034HXHH2LZFzvoaSAhfGVD" },
    ]) {
      expect(parses("memory.saved", { ...memory, origin }), origin.type).toBe(true);
    }
    expect(
      parses("memory.saved", { ...memory, origin: { type: "member", api_key_id: memory.origin.api_key_id } }),
    ).toBe(false);
  });
});
