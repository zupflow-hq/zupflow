/**
 * Every ID Zupflow shows is `{prefix}_{body}`: a prefix naming the object type
 * and a 22-character base62 body. Treat IDs as opaque: compare and pass them,
 * never parse, build or decode them.
 *
 * This is the one registry of prefixes. A prefix is never changed, reused or
 * shared; adding an object type that the API exposes means adding it here.
 */
import { z } from "zod";
import { registry } from "./registry";

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
  model_connection: "mcn",
  model: "mdl",
  channel_app_manager: "chmgr",
  channel_app: "chapp",
  channel_connection: "chcon",
  agent_channel: "agch",
  channel_conversation: "conv",
  channel_message: "chmsg",
  channel_person: "person",
  memory_type: "mtype",
  memory: "memo",
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
export type ModelConnectionId = Id<"model_connection">;
export type ModelId = Id<"model">;
export type ChannelAppManagerId = Id<"channel_app_manager">;
export type ChannelAppId = Id<"channel_app">;
export type ChannelConnectionId = Id<"channel_connection">;
export type AgentChannelId = Id<"agent_channel">;
export type ChannelConversationId = Id<"channel_conversation">;
export type ChannelMessageId = Id<"channel_message">;
export type ChannelPersonId = Id<"channel_person">;
export type MemoryTypeId = Id<"memory_type">;
export type MemoryId = Id<"memory">;

/** The 22-character base62 body every ID shares. */
const BODY = /^[0-9A-Za-z]{22}$/;

const schemas = new Map<IdKind, z.ZodType>();
const componentName = (kind: IdKind) => `${kind.replace(/(^|_)([a-z])/g, (_, __, c: string) => c.toUpperCase())}Id`;

/** The schema of one ID type: its prefix, then a base62 body. One shared, named schema per type. */
export function idSchema<K extends IdKind>(kind: K): z.ZodType<Id<K>> {
  let schema = schemas.get(kind);
  if (!schema) {
    schema = z
      .templateLiteral([`${ID_PREFIXES[kind]}_`, z.string().regex(BODY)])
      .register(registry, { id: componentName(kind) });
    schemas.set(kind, schema);
  }
  return schema as unknown as z.ZodType<Id<K>>;
}

/** Any Zupflow ID: one of the registered prefixes, then a base62 body. */
export const AnyIdSchema = z
  .templateLiteral([z.enum(Object.values(ID_PREFIXES) as [IdPrefix, ...IdPrefix[]]), "_", z.string().regex(BODY)])
  .register(registry, { id: "AnyId" }) as unknown as z.ZodType<AnyId>;
