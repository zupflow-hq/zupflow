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
                              KeyService, Cache, ObjectStore, EventBus, JobQueue, Identity,
                              ModelCatalog, the organizations, workspaces, API key, app, installation, tool
                              and agent stores
  crates/zupflow-basic/       The basic implementations: local keys, an in-memory cache, local files,
                              an in-process event bus, local sign-in, the configured model catalog
  crates/zupflow-sqlite/      The basic database: SQLite, its migrations, the stores, the job queue
  crates/zupflow-oidc/        Console sign-in through any OpenID Connect provider
  crates/zupflow-outbound/    The safe outbound HTTP path: public HTTPS only, no redirects, bounded
  crates/zupflow-mcp/         A minimal MCP client: lists a server's tools through the safe path
  crates/zupflow-contract/    Development only: the server and fixture the contract suite runs against
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

It listens on `127.0.0.1:8080` (`ZUPFLOW_LISTEN`), keeps its data in one
SQLite file (`ZUPFLOW_DATABASE`, default `zupflow.db`, created readable by its
owner only) and logs JSON (`ZUPFLOW_LOG`, a tracing filter). An invalid, empty
or non-UTF-8 value stops startup rather than falling back.

Console sign-in defaults to `ZUPFLOW_IDENTITY=local`: no sign-in, every Console
request is one built-in owner (`owner@local.invalid`). Because that trusts
anyone who can reach the port, the server refuses to start with it on
anything but a loopback address, and answers only requests addressed to
`localhost`, `127.0.0.1` or `[::1]` and its port, so a web page can't reach it
through DNS rebinding. Browser requests must come from an allowed origin: the
server's own, or one listed in `ZUPFLOW_ALLOWED_ORIGINS` (comma-separated,
like `http://localhost:5173`). Request bodies must be `application/json`.

For a team, `ZUPFLOW_IDENTITY=oidc` signs the Console in through any OpenID
Connect provider: set `ZUPFLOW_OIDC_ISSUER` (the `iss` of its tokens) and
`ZUPFLOW_OIDC_AUDIENCE` (the Console's client ID), and optionally
`ZUPFLOW_OIDC_JWKS_URL` to skip discovery. The server then verifies the
provider's ID token as the bearer and may listen on any address. The
open-source Console doesn't sign in with OIDC yet, and the server has no CORS
yet: both arrive when the Console switches from the mock to the backend.

The server's own secret, `ZUPFLOW_SECRET_KEY` (32 bytes, base64:
`openssl rand -base64 32`), keys what must not be guessable from the database:
API keys (`zf_test_…`, `zf_live_…`) are stored as an HMAC under it, and so are
idempotency fingerprints, and the key that seals apps' OAuth client secrets is
derived from it. With `local` it's optional: the server generates one on first
start and keeps it in the database. With `oidc` it's required, so it lives
outside the database. Changing it invalidates every API key and every stored
client secret.

`ZUPFLOW_PUBLIC_URL` is the origin providers and browsers reach the server at
(default `http://localhost:{port}`). An app's OAuth redirect URI is built on it:
`{ZUPFLOW_PUBLIC_URL}/connect/oauth/callback`, one per deployment, registered
once per OAuth client.

So far it serves `/health`, `/ready`, `/v1/me`, organizations, members,
workspaces, API keys, catalog and workspace apps with their client
configurations, the apps available to a workspace, installations (read,
renamed, disconnected and deleted), and Connect's settings and sessions with
the hosted page's read, cancel and a token app's credentials, checked against
the app's inputs and verified at its `verify_url` through the safe outbound
path. An OAuth app's consent starts from the hosted page with a single-use
`state` and PKCE; the provider sends the browser to
`/connect/oauth/callback`, which exchanges the code with the client's secret
(HTTP Basic), identifies the account at the app's `identity_url`, seals the
tokens for the installation and sends the browser back to the hosted page;
every other path answers the API's `404` envelope, and an unsupported method
on a served path the `405` one. It also serves the models an agent can use,
and agents and their grants: a grant names tools of one installation of the
same workspace, and only tools that installation's registry holds and doesn't
hold as gone (`422 unknown_tool`, `422 tool_gone`); a tool that goes later
stays in the grant, refused when called.

The models are the deployment's to list, in `ZUPFLOW_MODELS`: a JSON array
of `{"id", "name", "provider", "description"}` objects in the order to offer
them (`description` optional; IDs of letters, digits and `. - _ : / @`, each
once; any other field refused). Unset, or `[]`, offers none, and creating an
agent is refused until one is listed. Nothing calls a model yet.

Once an installation connects, a background worker in the same process lists
its app's tools from the app's MCP server (Streamable HTTP, through the safe
outbound path, with the installation's credential) and records them as that
installation's own in the tool registry: the first definition of each tool is
pinned, a later one that differs is kept beside it as changed, for review, and
a listing that repeats the credential is refused. Jobs wait in the
database's queue, so a restart picks them up again.

### The contract suite against the backend

```bash
pnpm contract:backend
```

Runs the shared contract suite (the same cases the mock passes) against the
backend: it builds the development-only `zupflow-contract-server`, restores
the reference fixture into a throwaway database, serves it on loopback and
points the suite at it. Cases that need an operation the backend doesn't serve
yet are skipped, each naming the operation. CI runs it after `pnpm check`.

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
- **OAuth consent** in the mock sends the browser to
  `/connect/:reference/simulated-consent` instead of a real provider; the
  backend sends it to the app's `authorize_url`.

## License

[Apache-2.0](LICENSE).

## Brand

Ink `#10191D` and cyan `#35C8F5`. Sandbox and Production are first-class colors
(amber and cyan): an environment is always visible before anyone connects an
account or starts a run. See `packages/brand/USAGE.txt`.
