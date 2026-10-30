/**
 * The API reference, read at build time from the OpenAPI document that
 * `@zupflow/contract` generates. Only plain data leaves this module, so the
 * reference page ships no schema library to the browser.
 */
import generated from "@zupflow/contract/openapi.json";

type Schema = {
  $ref?: string;
  format?: string;
  pattern?: string;
  minimum?: number;
  const?: unknown;
  type?: string | string[];
  enum?: unknown[];
  items?: Schema;
  anyOf?: Schema[];
  oneOf?: Schema[];
  properties?: Record<string, Schema>;
  required?: string[];
  description?: string;
};

type Parameter = { $ref?: string; name: string; in: string; required?: boolean; schema?: Schema; description?: string };

type Operation = {
  operationId: string;
  summary: string;
  parameters?: Parameter[];
  requestBody?: { content: { "application/json": { schema: Schema } } };
  responses: Record<string, { content?: { "application/json": { schema: Schema } } }>;
  "x-zupflow-callers"?: string[];
  "x-zupflow-shows-secret-once"?: boolean;
};

export type Field = { name: string; type: string; required: boolean; description?: string };

export type Endpoint = {
  id: string;
  group: string;
  method: string;
  path: string;
  summary: string;
  callers: string[];
  showsSecretOnce: boolean;
  pathParams: Field[];
  query: Field[];
  body: Field[];
  bodyType?: string;
  status: number;
  response?: string;
  /** A `curl` request built from the operation's schemas. */
  request: string;
  /** An example response body, the envelope included, as formatted JSON. */
  example?: string;
};

/** The parts of the document this page reads; JSON's inferred literal types are too narrow to check against. */
const openapi = generated as unknown as {
  info: { version: string };
  components: { schemas: Record<string, Schema> };
  paths: Record<string, Record<string, Operation>>;
};

const schemas = openapi.components.schemas;
const refName = (ref: string) => ref.split("/").pop() ?? ref;

/** A short, readable type: a component's name, a literal union, `T[]`, `T | null`. */
export function typeLabel(schema: Schema | undefined): string {
  if (!schema) return "unknown";
  if (schema.$ref) return refName(schema.$ref);
  const union = schema.anyOf ?? schema.oneOf;
  if (union) return union.map(typeLabel).join(" | ");
  if (schema.enum) {
    const values = schema.enum.map((v) => JSON.stringify(v));
    return values.length > 4 ? `${values.slice(0, 4).join(" | ")} | …` : values.join(" | ");
  }
  if (schema.type === "array") return `${typeLabel(schema.items)}[]`;
  if (Array.isArray(schema.type)) return schema.type.join(" | ");
  return schema.type ?? "unknown";
}

function fieldsOf(schema: Schema | undefined): { fields: Field[]; type?: string } {
  if (!schema) return { fields: [] };
  const resolved = schema.$ref ? schemas[refName(schema.$ref)] : schema;
  const type = schema.$ref ? refName(schema.$ref) : undefined;
  if (!resolved?.properties) return { fields: [], type: type ?? typeLabel(schema) };
  const required = new Set(resolved.required ?? []);
  return {
    type,
    fields: Object.entries(resolved.properties).map(([name, field]) => ({
      name,
      type: typeLabel(field),
      required: required.has(name),
      description: field.description,
    })),
  };
}

const GROUPS: Record<string, string> = {
  "sign-in-settings": "Identity",
  me: "Identity",
  organizations: "Organizations",
  usage: "Organizations",
  members: "Members",
  "audit-events": "Audit log",
  workspaces: "Workspaces",
  apps: "Apps",
  "available-apps": "Apps",
  "tool-policy": "Tool policy",
  installations: "Installations",
  "connect-sessions": "Connect",
  connect: "Connect",
  agents: "Agents",
  runs: "Runs",
  "model-connections": "Model providers",
  models: "Model providers",
  "default-models": "Model providers",
  "memory-kinds": "Memory",
  "memory-types": "Memory",
  "default-memory-type": "Memory",
  "connect-settings": "Connect",
  "api-keys": "API keys",
  webhooks: "Webhooks",
};

/** The resource a path is about: the first segment after its organization or workspace. */
function groupOf(path: string): string {
  const segments = path.split("/").filter((s) => s && s !== "v1" && !s.startsWith("{"));
  const scoped = segments[0] === "organizations" || segments[0] === "workspaces";
  const key = (scoped && segments[1]) || segments[0] || "";
  if (key === "apps" && segments.includes("tool-levels")) return "Tool policy";
  if (key === "agents" && segments.at(-1) === "runs") return "Runs";
  return GROUPS[key] ?? key;
}

function fieldOf(parameter: Parameter): Field {
  return {
    name: parameter.name,
    type: typeLabel(parameter.schema),
    required: parameter.required ?? false,
    description: parameter.description ?? parameter.schema?.description,
  };
}

// Examples, made from the schemas at build time. Deterministic: the same
// object type always gets the same ID, so a path's `ws_…` matches the
// response's.

const ALPHABET = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

function idBody(seed: string, length: number): string {
  let h = 2166136261;
  let out = "034";
  for (let i = 0; out.length < length; i++) {
    h = Math.imul(h ^ seed.charCodeAt(i % seed.length) ^ i, 16777619) >>> 0;
    out += ALPHABET[h % ALPHABET.length];
  }
  return out.slice(0, length);
}

const SAMPLE_STRINGS: Record<string, string> = {
  name: "Acme Corp",
  slug: "acme",
  external_id: "cus_1042",
  email: "ada@example.com",
  title: "Plan my week",
  instructions: "You help the team plan and track their work.",
  input: "What's due this week?",
  url: "https://example.com/zupflow/webhooks",
  redirect_url: "https://example.com/settings/integrations",
  return_url: "https://example.com/settings/integrations",
  description: "Lists the issues assigned to you.",
  reason: "Looks right, go ahead.",
  key: "linear",
  app_key: "linear",
  tool: "list_issues",
  model: "claude-sonnet-4-5",
  provider: "anthropic",
  cursor: "eyJpZCI6IjAzNEgifQ",
  next_cursor: null as unknown as string,
};

const MAX_DEPTH = 6;

function example(schema: Schema | undefined, name = "", depth = 0): unknown {
  if (!schema || depth > MAX_DEPTH) return null;
  if (schema.$ref) {
    const ref = refName(schema.$ref);
    const target = schemas[ref];
    const prefix = target?.pattern?.match(/^\^([a-z]+)_\[/)?.[1];
    if (prefix) return `${prefix}_${idBody(ref, 22)}`;
    return example(target, name, depth + 1);
  }
  if (schema.const !== undefined) return schema.const;
  const union = schema.anyOf ?? schema.oneOf;
  if (union) {
    const present = union.filter((s) => s.type !== "null");
    return example(present[0] ?? union[0], name, depth);
  }
  if (schema.enum) return schema.enum[0];
  const type = Array.isArray(schema.type) ? (schema.type.find((t) => t !== "null") ?? "null") : schema.type;
  switch (type) {
    case "object": {
      if (!schema.properties) return {};
      const out: Record<string, unknown> = {};
      for (const [key, value] of Object.entries(schema.properties)) out[key] = example(value, key, depth + 1);
      return out;
    }
    case "array":
      return depth >= MAX_DEPTH ? [] : [example(schema.items, name, depth + 1)];
    case "integer":
    case "number":
      return name.endsWith("_count") || name === "total" ? 3 : Math.max(schema.minimum ?? 1, 1);
    case "boolean":
      return true;
    case "null":
      return null;
    case "string":
      if (schema.format === "date-time" || name.endsWith("_at")) return "2026-10-04T21:04:11.000Z";
      if (name in SAMPLE_STRINGS) return SAMPLE_STRINGS[name];
      if (schema.pattern === "^[a-z2-7]{6}$") return "k7qmxa";
      if (name.endsWith("_url") || name === "icon") return "https://example.com/asset";
      return name ? name.replaceAll("_", " ") : "string";
    default:
      return null;
  }
}

/** Only the required fields of a request body, so an example shows the least a call needs. */
function requestBody(schema: Schema | undefined): Record<string, unknown> | undefined {
  if (!schema) return undefined;
  const resolved = schema.$ref ? schemas[refName(schema.$ref)] : schema;
  if (!resolved?.properties) return undefined;
  const required = new Set(resolved.required ?? []);
  const keep = Object.entries(resolved.properties).filter(([key]) => required.has(key));
  const chosen = keep.length > 0 ? keep : Object.entries(resolved.properties).slice(0, 2);
  return Object.fromEntries(chosen.map(([key, value]) => [key, example(value, key, 1)]));
}

function curl(method: string, path: string, parameters: Parameter[], op: Operation): string {
  let url = path;
  for (const p of parameters.filter((p) => p.in === "path")) {
    url = url.replace(`{${p.name}}`, String(example(p.schema, p.name)));
  }
  const callers = op["x-zupflow-callers"] ?? [];
  const lines = [`curl ${method === "GET" ? "" : `-X ${method} `}https://zupflow.example.com${url}`];
  if (callers.includes("api_key")) lines.push(`  -H "Authorization: Bearer $ZUPFLOW_API_KEY"`);
  else if (callers.includes("console")) lines.push(`  -H "Authorization: Bearer $CONSOLE_TOKEN"`);
  const body = requestBody(op.requestBody?.content["application/json"].schema);
  if (method === "POST") lines.push(`  -H "Idempotency-Key: $(uuidgen)"`);
  if (body) {
    lines.push(`  -H "Content-Type: application/json"`);
    const json = JSON.stringify(body, null, 2).replaceAll("'", "'\\''");
    lines.push(`  -d '${json.split("\n").join("\n  ")}'`);
  }
  return lines.join(" \\\n");
}

export const endpoints: Endpoint[] = Object.entries(openapi.paths).flatMap(([path, operations]) =>
  Object.entries(operations).map(([method, op]) => {
    const parameters = (op.parameters ?? []).filter((p) => !p.$ref);
    const [status = "200", answer] = Object.entries(op.responses).find(([code]) => code !== "default") ?? [];
    const data = answer?.content?.["application/json"].schema.properties?.data;
    const body = fieldsOf(op.requestBody?.content["application/json"].schema);
    return {
      id: op.operationId,
      group: groupOf(path),
      method: method.toUpperCase(),
      path,
      summary: op.summary,
      callers: op["x-zupflow-callers"] ?? [],
      showsSecretOnce: op["x-zupflow-shows-secret-once"] ?? false,
      pathParams: parameters.filter((p) => p.in === "path").map(fieldOf),
      query: parameters.filter((p) => p.in === "query").map(fieldOf),
      body: body.fields,
      bodyType: body.type,
      status: Number(status),
      response: data ? typeLabel(data) : undefined,
      request: curl(method.toUpperCase(), path, parameters, op),
      example: answer?.content
        ? JSON.stringify(example(answer.content["application/json"].schema), null, 2)
        : undefined,
    };
  }),
);

export const apiVersion = openapi.info.version;
