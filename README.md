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
                              ModelCatalog, the organizations, workspaces, API key, app, installation, tool,
                              agent, run and webhook stores
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

With no `VITE_ZUPFLOW_API_URL` set, `pnpm dev` serves every request from the
mock API in `apps/console/src/mocks` through a service worker. Data is kept in
this browser's localStorage; **Reset demo data** in the user menu reseeds it.

The mock exists only in the dev server and tests. A build (`pnpm build`, in
any `--mode`) leaves it out, service worker included, and calls the API at
`VITE_ZUPFLOW_API_URL` (by default `/api` on the Console's own origin);
`scripts/check-build.mjs` fails the build if any of the mock is left in. A
preview build that runs on the mock must say so twice: `VITE_ZUPFLOW_MOCK=1
pnpm --filter @zupflow/console build --allow-mock`.

Every build carries a Content-Security-Policy in `index.html`: scripts only
from the Console's own files (no inline script), requests only to itself,
`VITE_ZUPFLOW_API_URL`'s origin and `VITE_ZUPFLOW_OIDC_ORIGINS` (the sign-in
provider's issuer, and its token endpoint's origin if different; set it at
build time for `oidc`, or sign-in is blocked), no plugins, no `<base>`, forms
to itself only. `check-build.mjs` checks it is there. A meta tag can't forbid
framing, so whatever serves the Console must also send `Content-Security-Policy:
frame-ancestors 'none'` and `X-Frame-Options: DENY`, as the dev and preview
servers do.

### The Console against the backend

Start the backend (below), then point the Console at it and tell the backend
where the Console is served from:

```bash
ZUPFLOW_CONSOLE_ORIGIN=http://localhost:5180 pnpm --filter @zupflow/server dev
VITE_ZUPFLOW_API_URL=http://localhost:8080 pnpm dev
```

The Console asks the API how to sign in (`GET /v1/sign-in-settings`). With
`local` nobody signs in: the Console is the server's built-in owner, and says
so in the user menu. With `oidc` it signs in at the provider with the
authorization code flow and PKCE, at the redirect URI
`{Console origin}/sign-in/callback`, and sends the provider's ID token, which
the API verifies. The provider's client for the Console must be a public one
(no secret) that allows that redirect URI and answers its token endpoint
cross-origin. The ID token is kept in the tab's `sessionStorage` until it
expires, and a refresh token, if the provider issues one, in memory only;
signing out forgets both but doesn't end the provider's own session. A reload
or a new tab drops the refresh token, so that session lasts until its ID token
expires. Script running on the Console's origin could read the ID token from
`sessionStorage`; the CSP is what keeps foreign script off it. The
alternatives, memory only (sign in again on every reload) or a backend session
cookie, trade that for convenience or a server-side session.

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
provider's ID token as the bearer and may listen on any address.
`GET /v1/sign-in-settings`, which needs no credential, tells the Console the
mode, and for `oidc` the issuer and client ID.

`ZUPFLOW_CONSOLE_ORIGIN` is the origin the Console is served from when it
isn't the API's own (like `http://localhost:5180`). The guard admits it, and
the server answers CORS for it and for no other origin: its preflights, and
every answer it receives, errors included. CORS never allows credentials; the
Console sends its token itself. Unset, no origin gets CORS. Like every
`ZUPFLOW_*` setting, an invalid value (not exactly one origin) stops startup.

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

So far it serves `/health`, `/ready`, `/v1/sign-in-settings`, `/v1/me`, organizations, members,
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

Tool permissions (decision 0040) are served too: the organization's tool
policy (`manual` mode, and the level for tools nobody has confirmed a level
for, `approve` or `deny`) and its level per catalog app and tool name
(`allow`, `approve`, `deny`). Every member and the organization's API keys
read them; owners and admins change them from the Console. Discovery suggests
a level from each new tool's hints (read-only: `allow`, otherwise `approve`;
destructive flagged), unconfirmed; only a confirmed level is in force. A grant
of a catalog app's installation is held inside the levels in force
(`422 tool_denied`, `422 approval_required_by_policy`); tightening a level
rewrites no grant. Owners, admins and API keys accept a changed tool's latest
definition, exactly as reviewed (`409 stale_definition` otherwise), which pins
it. `ConfiguredPolicy` in `zupflow-basic` is the built-in `ApprovalPolicy`
that decides each call a run makes; it fails closed, requiring approval when
its configuration can't be read.

The models are the deployment's to list, in `ZUPFLOW_MODELS`: a JSON array
of `{"id", "name", "provider", "description"}` objects in the order to offer
them (`description` optional; IDs of letters, digits and `. - _ : / @`, each
once; any other field refused). Unset, or `[]`, offers none, and creating an
agent is refused until one is listed. No model provider ships with Zupflow
(`ModelProvider` is a trait, with a conformance suite), and none answers runs
unless one is configured: by default every run fails `model_unavailable`.
For local work, `ZUPFLOW_MODEL_PROVIDER=scripted` answers runs with the
scripted model in `zupflow-basic`, which serves the listed IDs and calls a
tool for each line `call <tool> <json-object>` of a run's input (otherwise it
answers `You said: …`), so runs are deterministic. Never use it in a
deployment: it turns what a run's input says into tool calls. Any other value
stops startup; the server logs which applies.

Runs are served: started (`403 console_read_only_in_production` for a
Console session in Production, `422 agent_paused`), listed per workspace and
across the organization, read with their timelines, and cancelled. A worker
in the same process takes each run through the run engine: every turn, the
model is offered the agent's granted tools that neither the floor nor the
policy refuses; each call it asks for is decided again (floor, then policy)
and recorded with its policy, mode and rule; allowed calls go to the
installation's MCP server through the safe outbound path with its credential
injected last. A call that needs approval pauses the run until the
organization's backend decides with an API key (`POST …/runs/{run}/approvals`;
a Console session gets `403 approval_requires_api_key`); a denial goes back
to the model as the call's result. A call is recorded as sent before it is
sent, and decided again on what holds then (a call that now needs approval
waits for it; one now refused isn't sent): one cut off by a restart is
settled as unknown and never sent again. A denial's optional `reason` goes
back to the model. A run no job takes forward any more (its last job died)
is failed `run_unavailable` by the worker's periodic sweep.
Each event is announced on the in-process event bus, to the workspace and to
its environment.

Usage is served too: runs, failed runs and tool calls per UTC day in one
environment (`?environment=`, required; `days` 1 to 90, 14 by default), every
day listed; an API key reads its own environment's only. A workspace's
`runs_last_24h` counts the runs started in it in the last 24 hours.

Webhook endpoints are served (decision 0017): every member and the
organization's API keys list them; owners, admins and API keys add, change,
delete and test them, a key only in its own environment (another
environment's endpoint is `404`, adding one there `422
environment_mismatch`). URLs are public HTTPS, checked when saved and again,
after DNS, at every delivery. Each endpoint's signing secret
(`whsec_test_…`, `whsec_live_…`) is sealed under its own
`webhook_signing_secret` context and shown once, in the creating response;
responses show its last four characters. The test button sends a real, signed
`webhook.test` event and answers the endpoint's status (`0` when nothing came
back), kept as the endpoint's `last_delivery`.

Events are written with the change that causes them: the run store queues an
announcement in the transaction that appends `run.completed`, `run.failed`,
`run.cancelled` or `approval.requested` (sent as `run.waiting_for_approval`),
and the tool registry in the one where a listing changes a tool
(`app.tools_changed`). A worker in the same process records each event once,
lean (type, IDs, environment, the workspace's `external_id`, the object's
status; never content), with a delivery for every enabled endpoint of the
event's organization and environment that subscribes to its type. Each
delivery is signed (`Zupflow-Signature: t=…,v1=…`, HMAC-SHA256 over
`"<t>.<raw body>"`, with `Zupflow-Event-Id`, `Zupflow-Event-Type` and
`Zupflow-Delivery-Attempt`) and sent through the safe outbound path; any
`2xx` within 10 seconds is success, anything else is tried again about 1
minute, 5 minutes, 30 minutes, 2, 6, 12 and 24 hours later (with jitter):
eight attempts, at least once, unordered, the same event ID every time. An
endpoint that has failed every attempt for three days is disabled; enabling
it again doesn't replay what it missed.

Limits, per server process: one loop records events, so a slow endpoint never
holds one up, and eight more send deliveries, so at most eight are in flight
at once. An endpoint has at most one delivery in flight across every worker
(a hold on its row, released after the attempt or 30 seconds); a delivery that
finds its endpoint busy waits about five seconds without using an attempt.
Only the endpoint's own answers are attempts: when Zupflow can't send (a store
or the key service is down), the job backs off from 5 seconds, doubling to an
hour, and the test button answers `503 unavailable`. A delivery's job allows
twelve such claims beyond its eight attempts; one that runs out settles its
delivery as failed, leaving the endpoint's record alone. An announcement is
claimed up to 30 times, about 20 hours of backing off, and is then buried and
logged as lost.

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

To click through the Console on the same fixture, serve it for the Console's
origin: `zupflow-contract-server serve <database> --console
http://localhost:5181` answers CORS for that origin and signs a request
without a token in as the owner, as `local` does; then run the Console with
`PORT=5181 VITE_ZUPFLOW_API_URL=http://127.0.0.1:<port>`, the port it prints.

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

- **Sign-in** is simulated by the mock's auth adapter
  (`src/features/auth/mock-adapter.ts`), which `src/features/auth/session.ts`
  loads only in builds with the mock. Against an API, the session uses the
  `local` or `oidc` adapter the API names.
- **OAuth consent** in the mock sends the browser to
  `/connect/:reference/simulated-consent` instead of a real provider; the
  backend sends it to the app's `authorize_url`.

## License

[Apache-2.0](LICENSE).

## Brand

Ink `#10191D` and cyan `#35C8F5`. Sandbox and Production are first-class colors
(amber and cyan): an environment is always visible before anyone connects an
account or starts a run. See `packages/brand/USAGE.txt`.
