# @zupflow/contract

The [Zupflow](https://docs.zupflow.com) API contract: a zod schema for every request and response, every route, the prefixed ID types, and the OpenAPI document (`@zupflow/contract/openapi.json`).

Most apps want [`@zupflow/sdk`](https://www.npmjs.com/package/@zupflow/sdk), which uses this package and re-exports its types. Use the contract directly to validate payloads or to generate a client in another language.

```ts
import { WebhookEvent } from "@zupflow/contract/schemas";

const event = WebhookEvent.parse(JSON.parse(body));
```

[API reference](https://docs.zupflow.com/reference/api/) · [Source](https://github.com/zupflow-hq/zupflow/tree/main/packages/contract) · Apache-2.0
