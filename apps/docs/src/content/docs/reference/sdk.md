---
title: TypeScript SDK
description: The typed client, its transport, errors and webhook helpers.
sidebar:
  order: 2
---

`@zupflow/sdk` is a typed client over the HTTP API. Every type comes from `@zupflow/contract`, so it always matches the server.

## Create a client

```ts
import { createClient, createHttpTransport } from "@zupflow/sdk";

export const zupflow = createClient(
  createHttpTransport({
    baseUrl: "https://zupflow.example.com",
    getToken: async () => process.env.ZUPFLOW_API_KEY ?? null,
    retries: 2, // requests that got no response at all
  }),
);
```

`getToken` runs per request, so a rotated key is picked up without a restart.

## Calling it

Resources are grouped by what they act on. Workspace-scoped calls take the workspace ID first and carry it in the path.

```ts
const { items: workspaces } = await zupflow.workspaces.list(organizationId, { environment: "sandbox" });
const run = await zupflow.runs.create(workspaceId, agentId, { input: "Hi" });
await zupflow.runs.cancel(workspaceId, run.id);
```

| Group | Covers |
| --- | --- |
| `organizations`, `members`, `invitations` | Your organization and its people. |
| `workspaces` | Your customers' workspaces. |
| `organizationApps`, `workspaceApps`, `toolPolicy` | The app catalog, configurations and tool levels. |
| `installations`, `connectSessions` | Connected accounts and Connect. |
| `agents`, `grants` | Agents and what they may use. |
| `runs`, `organizationRuns` | Runs, their events, approvals and cancelling. |
| `modelConnections`, `models` | Model providers. |
| `apiKeys`, `webhooks`, `connectSettings` | Developer settings. |

## Idempotency

Every `POST` sends an `Idempotency-Key`. By default the SDK makes a fresh one per call and reuses it on its own retries. To make a retry safe across process restarts, store your own key with the work and pass it:

```ts
await zupflow.runs.create(workspaceId, agentId, { input }, { idempotencyKey: job.id });
```

## Errors

- `ZupflowApiError`: the server answered with an error. It carries the HTTP status and the error's `code`, like `model_not_configured`.
- `ZupflowNetworkError`: no answer at all, after the retries.

```ts
import { ZupflowApiError } from "@zupflow/sdk";

try {
  await zupflow.runs.create(workspaceId, agentId, { input });
} catch (error) {
  if (error instanceof ZupflowApiError && error.code === "agent_paused") {
    // tell the person the agent is paused
  }
  throw error;
}
```

## Webhooks

`verifyWebhook(rawBody, signatureHeader, secret)` checks a delivery and returns its typed event; `signWebhook(rawBody, secrets)` produces a signature for tests. See [Webhooks](/concepts/webhooks/#verifying-a-delivery).
