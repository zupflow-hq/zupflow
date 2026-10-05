# Zupflow

Zupflow is a standalone, open-source backend for adding personal agents to your
product. Create an isolated workspace for each of your customers, connect their
accounts, give an agent access to selected tools, and run it inside your own
experience.

This repository is the monorepo for the platform: the developer **Console**,
the TypeScript **SDK**, the API **contract**, and the Rust **backend**, which
is being built against that contract. Until it serves the API, the Console runs
against an in-browser mock of it.

```
apps/
  console/        Developer Console — Vite, React 19, TanStack Router + Query, shadcn/ui
packages/
  contract/       @zupflow/contract — zod schemas, route table, generated openapi.json
  contract-tests/ @zupflow/contract-tests — the black-box suite every server must pass
  sdk/            @zupflow/sdk — HTTP transport, typed resource client, verifyWebhook
  brand/          B1 Precision brand kit (SVG/PNG/ICO) and usage notes
server/           The Rust backend — a Cargo workspace
  crates/zupflow-core/        Shared types and the external-resource traits: typed IDs, API errors, scopes,
                              KeyService, Cache, ObjectStore, EventBus, JobQueue
  crates/zupflow-basic/       The basic implementations: local keys, an in-memory cache, local files,
                              an in-process event bus
  crates/zupflow-sqlite/      The basic database: SQLite, its migrations, the database-backed job queue
  crates/zupflow-conformance/ The suites every implementation of a trait must pass
  crates/zupflow-server/      The API server (`zupflow` binary), on axum
```

## Getting started

Requires Node 22+ and pnpm 10.

```bash
pnpm install
pnpm dev
```

The Console opens on <http://localhost:5180>. Choose **Continue as Jordan Lee**
to explore the seeded Northwind organization (a fictional product that embeds
Zupflow), or **Create a new account** to walk through onboarding from an empty
organization.

With no `VITE_ZUPFLOW_API_URL` set, every request is served by the mock API in
`apps/console/src/mocks` through a service worker. Data is kept in this
browser's localStorage; **Reset demo data** in the user menu reseeds it.

### The backend

Requires Rust (the version is pinned in `server/rust-toolchain.toml`; rustup
picks it up).

```bash
pnpm --filter @zupflow/server dev   # or: cd server && cargo run --bin zupflow
```

It listens on `127.0.0.1:8080` (`ZUPFLOW_LISTEN`) and logs JSON
(`ZUPFLOW_LOG`, a tracing filter). An invalid, empty or non-UTF-8 value stops
startup rather than falling back. So far it serves `/health` and `/ready`; every
other path answers the API's `404` envelope, and an unsupported method on a
served path the `405` one.

## Checks

```bash
pnpm check        # Biome lint + format, typecheck, tests and build for every package, Rust included
pnpm test         # Vitest and cargo test
pnpm format       # Apply Biome fixes
```

CI runs `pnpm check` on every push and pull request.

## How the Console is built

```
@zupflow/sdk (transport + typed client)
  → features/queries.ts (TanStack Query hooks; cache keys and invalidation)
    → routes/** (pages)
```

- Pages never call `fetch` or the SDK directly; they use the hooks in
  `src/features/queries.ts`.
- Wire contracts stay `snake_case`, exactly as the API serves them.
- Workspace-scoped calls carry the workspace ID in the path, and every
  workspace cache key starts with `["ws", workspaceId]`.
- Generic controls come from shadcn/ui (`src/components/ui`, generated — don't
  hand-edit). Console-specific building blocks live in `src/components/app`.
- Every page handles loading (layout-stable skeletons), empty, error,
  permission-denied and destructive-confirmation states, and works at 375 px.
- The Console shows one environment at a time. Sandbox pages live under
  `/{org}/sandbox/…` and Production pages have no prefix; a router rewrite
  (`src/lib/environment-url.ts`) maps the prefix to the environment, and
  `src/features/environment.ts` reads it; older `?env=` links are rewritten to
  the path form on load. Workspaces are listed and searched
  on the server, never loaded in full.

### The mock API

`src/mocks` is a faithful stand-in for the backend, not a fixture file. It
serves the contract in `packages/contract` (zod schemas, the route table and
the generated `openapi.json`) and enforces the API's
rules: membership before existence, environment-matched configuration with no
fallback, single-use Connect sessions, exact return-URL allowlists,
workspace-coherent grants, approvals only from API keys, and no Console
actions in Production. It also simulates runs. Those rules are pinned by
the shared contract suite in `packages/contract-tests`, which reaches a server
only over HTTP and runs against the mock in `src/mocks/handlers.test.ts`; the
backend runs the same suite by setting `ZUPFLOW_CONTRACT_URL` (see
`packages/contract-tests/src/remote.test.ts`). Cases that need the mock's
database stay in `handlers.test.ts`.

Two pieces exist only because there is no backend yet, and are labelled as such:

- **Sign-in** is simulated in `src/features/auth/session.ts`, the seam where a
  real build will plug in an auth adapter: `local` (no sign-in; the server
  must listen on loopback only) or `oidc` (any OpenID Connect provider).
- **OAuth consent** sends the browser to `/connect/:reference/simulated-consent`
  instead of a real provider.

## License

[Apache-2.0](LICENSE).

## Brand

Ink `#10191D` and cyan `#35C8F5`. Sandbox and Production are first-class colors
(amber and cyan): an environment is always visible before anyone connects an
account or starts a run. See `packages/brand/USAGE.txt`.
