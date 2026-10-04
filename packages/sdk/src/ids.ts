/**
 * Every ID Zupflow shows is `{prefix}_{body}`: a prefix naming the object type
 * and a 22-character body. Treat IDs as opaque: compare and pass them, never
 * parse, build or decode them.
 *
 * This is the one registry of prefixes. A prefix is never changed, reused or
 * shared; adding an object type that the API exposes means adding it here.
 */
export const ID_PREFIXES = {
  organization: "org",
  workspace: "ws",
  user: "usr",
  member: "mem",
  app: "app",
  installation: "inst",
  connect_session: "cs",
  agent: "agt",
  grant: "grt",
  run: "run",
  run_event: "rev",
  tool_call: "call",
  session: "ses",
  api_key: "key",
  webhook_endpoint: "we",
  webhook_event: "evt",
  webhook_delivery: "whd",
  audit_event: "aud",
} as const;

export type IdKind = keyof typeof ID_PREFIXES;
export type IdPrefix = (typeof ID_PREFIXES)[IdKind];

/** An ID of one object type, e.g. `Id<"run">` is `run_…`. */
export type Id<K extends IdKind> = `${(typeof ID_PREFIXES)[K]}_${string}`;

/** Any Zupflow ID, for fields that may name several object types. */
export type AnyId = `${IdPrefix}_${string}`;

export type OrganizationId = Id<"organization">;
export type WorkspaceId = Id<"workspace">;
export type UserId = Id<"user">;
export type MemberId = Id<"member">;
export type AppId = Id<"app">;
export type InstallationId = Id<"installation">;
export type ConnectSessionId = Id<"connect_session">;
export type AgentId = Id<"agent">;
export type GrantId = Id<"grant">;
export type RunId = Id<"run">;
export type RunEventId = Id<"run_event">;
export type ToolCallId = Id<"tool_call">;
export type SessionId = Id<"session">;
export type ApiKeyId = Id<"api_key">;
export type WebhookEndpointId = Id<"webhook_endpoint">;
export type WebhookEventId = Id<"webhook_event">;
export type WebhookDeliveryId = Id<"webhook_delivery">;
export type AuditEventId = Id<"audit_event">;
