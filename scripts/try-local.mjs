#!/usr/bin/env node
// Sets up a local server for a first run, with no provider account, and
// starts one. Talks to a server started with `pnpm --filter @zupflow/server
// dev` (local sign-in, scripted model), the same way the Console and your
// backend do:
//
// 1. As the built-in owner (local sign-in): an organization, a model
//    connection the scripted providers accept, a model made Sandbox's
//    default, and a Sandbox API key.
// 2. With that API key, as your backend would: a workspace, an agent and a
//    run, then the run's events.
//
// Running it again reuses what's there and replaces the API key, whose
// secret is shown once. ZUPFLOW_URL picks the server (http://localhost:8080
// by default).

import { randomUUID } from "node:crypto";

const base = (process.env.ZUPFLOW_URL ?? "http://localhost:8080").replace(/\/$/, "");
const ORG_SLUG = "acme-local";
const KEY_NAME = "Local first run";
// The scripted providers accept any key of the right shape.
const SCRIPTED_KEY = "sk-local-scripted-0000000000";
const MODEL = "claude-sonnet-4-5";

async function call(method, path, { body, organization, apiKey } = {}) {
  const headers = { "Content-Type": "application/json" };
  if (organization) headers["Zupflow-Organization"] = organization;
  if (apiKey) headers.Authorization = `Bearer ${apiKey}`;
  if (method === "POST") headers["Idempotency-Key"] = randomUUID();
  let response;
  try {
    response = await fetch(`${base}${path}`, { method, headers, body: body && JSON.stringify(body) });
  } catch {
    fail(`Can't reach ${base}. Start the server first: pnpm --filter @zupflow/server dev`);
  }
  const text = await response.text();
  const envelope = text ? JSON.parse(text) : { data: null, error: null };
  if (!response.ok) {
    const error = envelope.error ?? {};
    throw Object.assign(new Error(`${method} ${path}: ${error.code} ${error.message}`), { code: error.code });
  }
  return envelope.data;
}

const items = (data) => (Array.isArray(data) ? data : (data?.items ?? []));

function fail(message) {
  console.error(`\n${message}\n`);
  process.exit(1);
}

function step(message) {
  console.log(`• ${message}`);
}

// 1. The organization's setup, as its owner.
let me;
try {
  me = await call("GET", "/v1/me");
} catch (error) {
  fail(
    `${error.message}\nThis script needs a server with local sign-in (ZUPFLOW_IDENTITY=local), as the dev script starts.`,
  );
}
let organization = me.organizations.find((o) => o.slug === ORG_SLUG);
if (!organization) {
  organization = await call("POST", "/v1/organizations", { body: { name: "Acme (local)", slug: ORG_SLUG } });
}
const org = organization.id;
const asOwner = { organization: org };
step(`Organization ${organization.name} (${org})`);

const connections = await call("GET", `/v1/organizations/${org}/model-connections`, asOwner);
let connection = items(connections).find((c) => c.environment === "sandbox" && c.label === "Local (scripted)");
if (!connection) {
  try {
    connection = await call("POST", `/v1/organizations/${org}/model-connections`, {
      ...asOwner,
      body: { environment: "sandbox", kind: "anthropic", label: "Local (scripted)", api_key: SCRIPTED_KEY },
    });
  } catch (error) {
    if (error.code === "model_providers_not_configured") {
      fail(
        "The server reaches no model providers. Start it with ZUPFLOW_MODEL_PROVIDER=scripted, as the dev script does.",
      );
    }
    throw error;
  }
}
const models = await call("GET", `/v1/organizations/${org}/models?environment=sandbox`, asOwner);
let model = items(models).find((m) => m.route?.connection_id === connection.id && m.provider_model_id === MODEL);
if (!model) {
  model = await call("POST", `/v1/organizations/${org}/models`, {
    ...asOwner,
    body: { connection_id: connection.id, provider_model_id: MODEL },
  });
}
await call("PUT", `/v1/organizations/${org}/default-models/sandbox`, { ...asOwner, body: { model_id: model.id } });
step(`Sandbox's default model: ${model.name}, answered by the scripted model`);

// A key's secret is shown once, so each run revokes the last one and makes a fresh one.
const keys = await call("GET", `/v1/organizations/${org}/api-keys`, asOwner);
for (const key of items(keys).filter((k) => k.name === KEY_NAME && !k.revoked_at)) {
  await call("POST", `/v1/organizations/${org}/api-keys/${key.id}/revoke`, asOwner);
}
const { secret: apiKey } = await call("POST", `/v1/organizations/${org}/api-keys`, {
  ...asOwner,
  body: { name: KEY_NAME, environment: "sandbox" },
});
step("A Sandbox API key for your backend");

// 2. Your backend's part, with the API key.
const asBackend = { apiKey };
const workspaces = await call("GET", `/v1/organizations/${org}/workspaces?q=cus_demo`, asBackend);
let workspace = items(workspaces).find((w) => w.external_id === "cus_demo");
if (!workspace) {
  workspace = await call("POST", `/v1/organizations/${org}/workspaces`, {
    ...asBackend,
    body: { name: "Demo customer", slug: "demo", environment: "sandbox", external_id: "cus_demo" },
  });
}
step(`Workspace ${workspace.name} (${workspace.id})`);

const agents = await call("GET", `/v1/workspaces/${workspace.id}/agents`, asBackend);
let agent = items(agents).find((a) => a.name === "Planner");
if (!agent) {
  agent = await call("POST", `/v1/workspaces/${workspace.id}/agents`, {
    ...asBackend,
    body: {
      name: "Planner",
      description: "Plans the team's week.",
      instructions: "You help the team plan their week. Be brief.",
      model_id: null,
    },
  });
}
step(`Agent ${agent.name} (${agent.id})`);

let run = await call("POST", `/v1/workspaces/${workspace.id}/agents/${agent.id}/runs`, {
  ...asBackend,
  body: { input: "Hello, Zupflow!" },
});
step(`Run ${run.id} started`);
for (let i = 0; i < 60 && !["completed", "failed", "cancelled"].includes(run.status); i++) {
  await new Promise((resolve) => setTimeout(resolve, 500));
  run = await call("GET", `/v1/workspaces/${workspace.id}/runs/${run.id}`, asBackend);
}
const events = await call("GET", `/v1/workspaces/${workspace.id}/runs/${run.id}/events`, asBackend);

console.log(`\nRun ${run.status}: ${run.output ?? run.error?.message ?? ""}`);
console.log(
  `Events: ${items(events)
    .map((e) => e.type)
    .join(" → ")}`,
);
console.log(`
Your API key (Sandbox, shown once):
  export ZUPFLOW_URL=${base}
  export ZUPFLOW_API_KEY=${apiKey}

Start another run:
  curl -X POST $ZUPFLOW_URL/v1/workspaces/${workspace.id}/agents/${agent.id}/runs \\
    -H "Authorization: Bearer $ZUPFLOW_API_KEY" \\
    -H "Content-Type: application/json" \\
    -H "Idempotency-Key: $(uuidgen)" \\
    -d '{"input":"What can you do?"}'

The scripted model echoes your input; a line "call <tool> <json>" calls a tool.
Watch it in the Console: VITE_ZUPFLOW_API_URL=${base} pnpm dev, then http://localhost:5180
`);
