# @zupflow/sdk

The TypeScript SDK for [Zupflow](https://docs.zupflow.com), the backend for adding AI agents to your product: a typed client for every API endpoint, and webhook verification.

```bash
npm install @zupflow/sdk
```

```ts
import { createClient, createHttpTransport } from "@zupflow/sdk";

const zupflow = createClient(
  createHttpTransport({
    baseUrl: process.env.ZUPFLOW_URL!,
    getToken: async () => process.env.ZUPFLOW_API_KEY!,
  }),
);

const workspace = await zupflow.workspaces.create(organizationId, {
  name: "Acme Corp",
  slug: "acme",
  environment: "sandbox",
  external_id: "cus_1042",
});
```

Methods return the response's `data` directly; lists return `{ items, pagination }`.

Verify a webhook before you trust it:

```ts
import { verifyWebhook } from "@zupflow/sdk";

const verified = await verifyWebhook(
  rawBody, // the body exactly as received
  request.headers.get("zupflow-signature"),
  process.env.ZUPFLOW_WEBHOOK_SECRET!,
);
switch (verified.event.type) {
  case "run.completed":
    // ...
}
```

- [Quickstart](https://docs.zupflow.com/quickstart/)
- [API reference](https://docs.zupflow.com/reference/api/)
- [Source](https://github.com/zupflow-hq/zupflow/tree/main/packages/sdk)

Apache-2.0
