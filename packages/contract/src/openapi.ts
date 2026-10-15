/**
 * Builds the OpenAPI 3.1 document from the route table and the schemas.
 * `openapi.json` is this function's output, committed so the Rust backend and
 * anyone else can check themselves against it without running TypeScript.
 */
import { z } from "zod";
import { IDEMPOTENCY_HEADER } from "./idempotency";
import { registry } from "./registry";
import { type Caller, routes } from "./routes";
import { WebhookEvent } from "./schemas";
import {
  WEBHOOK_ATTEMPT_HEADER,
  WEBHOOK_EVENT_ID_HEADER,
  WEBHOOK_EVENT_TYPE_HEADER,
  WEBHOOK_SIGNATURE_HEADER,
  WEBHOOK_TOLERANCE_SECONDS,
  WEBHOOK_USER_AGENT,
} from "./webhooks";

type JsonSchema = Record<string, unknown>;

const COMPONENTS = "#/components/schemas/";

/**
 * Objects are described as they're read (`io: "input"`), so they stay open to
 * fields added later: adding a field is a compatible change, and clients
 * ignore fields they don't know.
 */
const OPTIONS = { io: "input", unrepresentable: "any" } as const;

/** Every schema in the registry as JSON Schema, keyed by its id, referring to each other as components. */
function convertRegistry(): Record<string, JsonSchema> {
  const { schemas } = z.toJSONSchema(registry, {
    ...OPTIONS,
    uri: (id) => `${COMPONENTS}${id}`,
  }) as unknown as { schemas: Record<string, JsonSchema> };
  return Object.fromEntries(
    Object.entries(schemas).map(([id, { $schema: _, $id: __, id: _id, ...json }]) => [id, json]),
  );
}

const SCRATCH = "__inline";

/**
 * JSON Schema for one zod schema, referring to named components instead of
 * inlining them. An unnamed schema is converted through the registry too,
 * under a scratch id, because converting it alone would read metadata from a
 * single registry and lose either the component names or the descriptions.
 */
function schemaOf(schema: z.ZodType): JsonSchema {
  const id = registry.get(schema)?.id;
  if (id) return { $ref: `${COMPONENTS}${id}` };
  registry.add(schema, { id: SCRATCH });
  try {
    const json = convertRegistry()[SCRATCH];
    if (!json) throw new Error("schema did not convert");
    return json;
  } finally {
    registry.remove(schema);
  }
}

/** Every named schema, as a component. */
function components(): Record<string, JsonSchema> {
  return Object.fromEntries(Object.entries(convertRegistry()).sort(([a], [b]) => a.localeCompare(b)));
}

function envelope(data: JsonSchema | null, paginated: boolean): JsonSchema {
  return {
    type: "object",
    required: ["data", "meta", "error"],
    properties: {
      data: data ?? { type: "null" },
      meta: paginated ? { $ref: `${COMPONENTS}Meta` } : { anyOf: [{ $ref: `${COMPONENTS}Meta` }, { type: "null" }] },
      error: { type: "null" },
    },
  };
}

const errorEnvelope = {
  type: "object",
  required: ["data", "meta", "error"],
  properties: { data: { type: "null" }, meta: { type: "null" }, error: { $ref: `${COMPONENTS}Error` } },
};

function parameters(route: (typeof routes)[number]) {
  const path = Object.entries(route.params).map(([name, schema]) => ({
    name,
    in: "path",
    required: true,
    schema: schemaOf(schema),
    ...(registry.get(schema)?.id?.endsWith("Id") ? { description: ID_PARAM } : {}),
  }));
  const query = route.query
    ? Object.entries(route.query.shape).map(([name, schema]) => ({
        name,
        in: "query",
        required: !(schema as z.ZodType).safeParse(undefined).success,
        schema: schemaOf(schema as z.ZodType),
      }))
    : [];
  // Every POST takes an idempotency key (decision 0016); PUT, PATCH and DELETE are safe to repeat as they are.
  // A provider's callback is the exception: its own event ID is the key.
  const idempotency =
    route.method === "post" && !route.callers.includes("provider")
      ? [{ $ref: "#/components/parameters/IdempotencyKey" }]
      : [];
  return [...path, ...query, ...idempotency];
}

const SECURITY: Record<Caller, Record<string, string[]>> = {
  console: { consoleSession: [] },
  api_key: { apiKey: [] },
  connect: {},
  public: {},
  provider: {},
};

/** Console calls to a workspace route also name its organization (decision 0023); API keys carry their own. */
function security(route: (typeof routes)[number]) {
  const workspaceRoute = route.path.startsWith("/v1/workspaces/");
  return route.callers.map((caller) =>
    caller === "console" && workspaceRoute ? { ...SECURITY.console, organization: [] } : SECURITY[caller],
  );
}

const ID_PARAM = "A malformed ID, or an ID of another type, answers 404 like a missing one.";

const WEBHOOK_HEADERS = [
  {
    name: WEBHOOK_SIGNATURE_HEADER,
    in: "header",
    required: true,
    schema: { type: "string" },
    description: `\`t=<unix seconds>,v1=<lowercase hex HMAC-SHA256>\` over \`"<t>.<raw body>"\` with the endpoint's signing secret; two \`v1\` values during a rotation. Verify against the raw body in constant time and reject a timestamp more than ${WEBHOOK_TOLERANCE_SECONDS} seconds from your clock. The SDK's \`verifyWebhook\` does all of it.`,
  },
  {
    name: WEBHOOK_EVENT_ID_HEADER,
    in: "header",
    required: true,
    schema: { type: "string" },
    description: "The event's `id`: the same on every attempt and resend. Skip IDs you've already handled.",
  },
  { name: WEBHOOK_ATTEMPT_HEADER, in: "header", required: true, schema: { type: "integer", minimum: 1 } },
  { name: "User-Agent", in: "header", required: true, schema: { type: "string", const: WEBHOOK_USER_AGENT } },
];

/** Each event type as an OpenAPI 3.1 webhook: what Zupflow POSTs to a subscribed endpoint. */
function webhooks() {
  return Object.fromEntries(
    WebhookEvent.options.map((variant) => {
      const type = variant.shape.type.value;
      return [
        type,
        {
          post: {
            operationId: `webhook_${type.replace(/\./g, "_")}`,
            summary: type === "webhook.test" ? "Sent by the test button; can't be subscribed to" : `\`${type}\``,
            parameters: [
              ...WEBHOOK_HEADERS,
              {
                name: WEBHOOK_EVENT_TYPE_HEADER,
                in: "header",
                required: true,
                schema: { type: "string", const: type },
              },
            ],
            requestBody: { required: true, content: { "application/json": { schema: schemaOf(variant) } } },
            responses: {
              "2XX": {
                description:
                  "Received. Answer within 10 seconds, then fetch current state; any other answer or a timeout is retried.",
              },
            },
          },
        },
      ];
    }),
  );
}

export function buildOpenApi() {
  const paths: Record<string, Record<string, unknown>> = {};
  for (const route of routes) {
    const { status, data, list, paginated, unwrapped } = route.response;
    const dataSchema = data ? (list ? { type: "array", items: schemaOf(data) } : schemaOf(data)) : null;
    const operation: Record<string, unknown> = {
      operationId: route.operationId,
      summary: route.summary,
      security: security(route),
      "x-zupflow-callers": route.callers,
      parameters: parameters(route),
      responses: {
        [status]:
          status === 204
            ? { description: "Done" }
            : {
                description: status === 202 ? "Accepted: a job finishes it" : "OK",
                content: {
                  "application/json": {
                    schema: unwrapped && dataSchema ? dataSchema : envelope(dataSchema, Boolean(paginated)),
                  },
                },
              },
        default: { $ref: "#/components/responses/Error" },
      },
    };
    if (route.showsSecretOnce) operation["x-zupflow-shows-secret-once"] = true;
    if (unwrapped) operation["x-zupflow-unwrapped"] = true;
    if (route.body) {
      operation.requestBody = { required: true, content: { "application/json": { schema: schemaOf(route.body) } } };
    }
    const methods = paths[route.path] ?? {};
    methods[route.method] = operation;
    paths[route.path] = methods;
  }
  return {
    openapi: "3.1.0",
    info: {
      title: "Zupflow API",
      version: "0.1.0",
      description:
        "Generated from @zupflow/contract. Every response is an envelope `{ data, meta, error }`; IDs are prefixed, opaque strings.",
      license: { name: "Apache-2.0", identifier: "Apache-2.0" },
    },
    servers: [{ url: "https://api.zupflow.example" }],
    components: {
      securitySchemes: {
        consoleSession: {
          type: "http",
          scheme: "bearer",
          description: "A Console session. Workspace routes also need the `organization` header.",
        },
        organization: {
          type: "apiKey",
          in: "header",
          name: "Zupflow-Organization",
          description:
            "The organization a Console call acts in (`org_…`). Required with a Console session on workspace routes; 400 `organization_required` without it.",
        },
        apiKey: {
          type: "http",
          scheme: "bearer",
          description: "An environment-bound API key (`zf_live_…`, `zf_test_…`).",
        },
      },
      parameters: {
        IdempotencyKey: {
          name: IDEMPOTENCY_HEADER,
          in: "header",
          required: false,
          schema: { type: "string", minLength: 1, maxLength: 255, pattern: "^[\\x20-\\x7E]+$" },
          description: [
            "Makes a retry safe: 1–255 printable ASCII characters, scoped to the caller that sent it, kept for 24 hours.",
            "The same key and request replays the saved response; the same key with a different request answers",
            "`422 idempotency_key_reused`; a key still in flight answers `409 idempotency_request_in_progress`;",
            "a malformed key answers `400 invalid_idempotency_key`. Routes marked `x-zupflow-shows-secret-once`",
            "never save their body: a retry answers `409 idempotency_key_completed`.",
          ].join(" "),
        },
      },
      responses: {
        Error: {
          description: "An error: branch on `error.code`, never on `error.message`.",
          content: { "application/json": { schema: errorEnvelope } },
        },
      },
      schemas: components(),
    },
    paths,
    webhooks: webhooks(),
  };
}
