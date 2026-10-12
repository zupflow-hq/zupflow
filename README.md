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
  crates/zupflow-taskboard/   Development only: Taskboard, a fake MCP app with its own OAuth, for end-to-end runs
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

Nobody joins an organization without accepting an invitation: after sign-in,
pending invitations are listed on the onboarding screen and under
**Invitations** in the user menu, each with **Accept** and **Decline**. Jordan
has one from Fernhill; signing up as `mei@northwind.example` shows Northwind's.

An invitation goes to an address, and stands only while whoever sent it (or
last changed its role) can still grant that role: removing or demoting them
withdraws it. Inviting an address whose invitation expired sends it again.
An account keeps the address it was created with: if the sign-in provider
later sends another one, invitations don't follow it, and only a sign-in that
verifies the original address can answer them (the backend answers `403
email_unverified` otherwise; the mock doesn't simulate verification).

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

So far it serves `/health`, `/ready`, `/v1/sign-in-settings`, `/v1/me`, the
signed-in person's invitations (accepted or declined only by the person whose
verified email was invited), organizations, members,
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
on a served path the `405` one.

The hosted Connect page itself is served too, at
`{ZUPFLOW_PUBLIC_URL}/connect/{reference}`: a small page of its own in
`server/crates/zupflow-server/connect-page` (one stylesheet, one script, built
into the binary), with none of the Console's code. The server renders each
state with everything escaped; the script sends the token form, the OAuth
start and the cancel to the hosted Connect API on the same origin. The request
guard admits that public origin for hosted Connect's paths only (`/connect/…`
and `/v1/connect/sessions/…`), never for a Console or workspace route, and
answers it no CORS. The page's Content-Security-Policy allows only its own
files (pinned by integrity hashes), one inline style by its hash, requests and
forms to itself, and no script sinks; it may be framed only by the session's
environment's `allowed_origins` (by nobody when there are none; `http://localhost`
only in Sandbox), and only they hear
`postMessage({ type: "zupflow.connect.finished", session_id, status })` when it
finishes, from an opener or a parent frame. The message is a hint, not a
result: an embedder checks its origin, then reads the session through the API
(`GET /v1/workspaces/{ws}/connect-sessions/{session}`) before believing it,
and keeps polling, since a window the provider's sign-in passed through may
have lost its opener. The Console does exactly that, and hears the message only
when its origin is among the allowed ones. The page sends no referrer, is never
cached (its files are, for a year, each at an address naming its version), and
the link's reference, a bearer secret, is never logged or written into the
page. A link that names no session, one that expired, and one used more than
five minutes past its expiry all answer the same generic `404` page, and the
hosted API's `GET /v1/connect/sessions/{reference}` the same `404` as no
session; until then a used link shows its outcome, so the OAuth callback can
send the browser back to it. A session whose app is gone shows the generic page
too. When it finishes in a popup, the page tells the opener and closes; in a
tab, a completed session sends the person to its return URL, told
`zupflow_session` (replacing any it carried), only while that URL is still on
the allowlist.

It also serves the models an agent can use,
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

The audit log is served too: owners and admins read their organization's
events from the Console, newest first, paged and filtered by workspace on the
server. Each change the mock records writes its event in the same transaction,
naming who made it (a member, an API key, or Connect), what changed and where;
never a secret. A deleted workspace's events stay (decision: keep history);
they go only with their organization.

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

The run engine is tested under faults too (`server/crates/zupflow-server/tests/run_faults.rs`,
with the kit in `tests/common/faults.rs`): real workers are stopped or crashed
at named points around every run-store write, the model's answer and the
outbound `tools/call`; a test clock runs leases out; two workers race on one
run. What happened is read from the fake MCP server's own ledger of the
requests it received (with the credential each carried) and the answers the
model produced, never only from the runtime's records. Tests marked
`#[ignore = "…"]` reproduce holes today's engine still has, each failing on
the assertion that names it: `cargo test -p zupflow-server --test run_faults
-- --ignored`.

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

Connect outcomes and installation lifecycle events are recorded with their
state changes, as immutable lean bodies and queued deliveries in the same
transaction. A successful new connection or reconnect emits
`installation.connected` and `connect_session.completed`; cancellation,
expiry and terminal failure emit `connect_session.failed`. Failed reconnects
have no result `installation_id`. Disconnecting emits
`installation.disconnected` once; repeating it changes nothing and records no
second audit. Deleting an installation emits its own disconnected event,
including when it was already disconnected. Credential failures likewise
record `installation.reconnect_required` with their transition.

A background sweep persists at most 100 expired pending sessions each second,
using their expiry instant for the event time. Concurrent completions,
cancellations and sweeps cannot finish a session twice. Lifecycle events keep
the original status, app, workspace external ID and event ID even after a
reconnect, a workspace edit or installation deletion. Credentials, account
details, session references and provider responses never enter an event.

Run and tool events use announcements queued in their transition's transaction:
`run.completed`, `run.failed`, `run.cancelled`, `approval.requested` (sent as
`run.waiting_for_approval`) and `app.tools_changed`. A worker records each event
once, with a delivery for every enabled endpoint of its organization and
environment that subscribes to its type. Events are lean: type, IDs,
environment, the workspace's `external_id` and the object's status. Each
delivery is signed (`Zupflow-Signature: t=…,v1=…`, HMAC-SHA256 over
`"<t>.<raw body>"`, with `Zupflow-Event-Id`, `Zupflow-Event-Type` and
`Zupflow-Delivery-Attempt`) and sent through the safe outbound path; any
`2xx` within 10 seconds is success, anything else is tried again about 1
minute, 5 minutes, 30 minutes, 2, 6, 12 and 24 hours later (with jitter):
eight attempts, at least once, unordered, the same event ID every time. An
endpoint that has failed every attempt for three days is disabled; enabling
it again doesn't replay what it missed.

Each endpoint has paginated delivery history at
`GET /v1/organizations/{org}/webhooks/{hook}/deliveries`, filtered by
`workspace_id`, `state` and `event_type`. A delivery's own path returns its
lean event, and `/attempts` lists its attempts with status, a safe error class,
duration and timing. Response bodies, request headers and signing secrets are
never included. Older deliveries may have only a summary;
`attempt_history_complete` identifies those gaps without inventing attempts.

Owners, admins and an API key for the endpoint's environment can resend a
delivery with `POST …/deliveries/{delivery}/resend`, including from the Console
in Production. It returns `201` with a new pending delivery and a fresh retry
budget. The original event ID, exact body and previous history stay intact;
consumers still deduplicate by event ID. Pending deliveries, disabled endpoints
and events the endpoint no longer subscribes to answer `409`. Test events need
no subscription. An `Idempotency-Key` replays the same response; each newly
created resend records one `webhook.delivery_resent` audit event.

The same managers can rotate an endpoint's signing secret with
`POST /v1/organizations/{org}/webhooks/{hook}/roll-secret`, including in
Production. Its `200` response contains `{ endpoint, signing_secret }`; save
the new secret when it appears. The endpoint's public
`previous_signing_secret_expires_at` gives the end of the 24-hour overlap,
or `null` when no previous secret is active. Until that deadline, deliveries
carry two `v1` signatures over the same timestamp and raw body, so a receiver
can move to the new secret without dropping deliveries. Afterwards only the
new secret signs. Another roll during overlap returns `409
rotation_in_progress`; a repeated successful `Idempotency-Key` returns `409
idempotency_key_completed` and never reveals the secret again. Rotation
preserves event IDs and delivery history and records one
`webhook.secret_rolled` audit event.

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

An installation's OAuth access token is refreshed before discovery or a tool
call when it expires within 30 seconds. Refresh preserves the original
authorization's audience: `mcp_oauth` sends its saved `resource`, while
configured `oauth2` omits it. A `401` refreshes once and retries
once; a `403`, timeout, `5xx` or uncertain tool outcome never triggers replay.
A durable claim serializes each credential version before exchanging its
single-use refresh token. The winner seals the rotated tokens as a new
version, saving them with the new expiry atomically; other workers read that
version. A claim left by a crash is never reused: after a minute it requires
a reconnect, as does a failed refresh or a second `401`. That status change,
its audit event and its `installation.reconnect_required` webhook outbox
commit together. Reconnecting clears the status. An `invalid_client` response
from the token endpoint forgets only the refused dynamic registration so the
next Connect registers again. Browser callback errors affect only their own
session. A configured OAuth client's configuration changing also requires a
reconnect before refresh, preserving which client the old token belongs to.

OAuth callbacks check a discovered server's `iss` exactly, before exchanging
a code or trusting an error; a server advertising RFC 9207 must send it.
Configured `oauth2` templates accept an optional `issuer` in the API and the
Console's authentication editor. Set the provider's exact public HTTPS issuer
without credentials, a query or a fragment. Every callback must then contain
that exact `iss`, including errors; letter case and trailing slashes matter.
The issuer is sealed with the authorization attempt, so editing the template
does not change what an in-progress sign-in trusts. Omitted or `null` keeps
legacy providers working without `iss`, and refuses a callback that supplies
it. A bad or already-used `state` shows a generic HTML error with Connect's
security headers and no session details.

Once an installation connects, a background worker in the same process lists
its app's tools from the app's MCP server (Streamable HTTP, through the safe
outbound path, with the installation's credential) and records them as that
installation's own in the tool registry: the first definition of each tool is
pinned, a later one that differs is kept beside it as changed, for review, and
a listing that repeats the credential is refused. The listing and suggested
catalog tool levels commit in the same transaction. Jobs wait in the
database's queue, so a restart picks them up again.

### The contract suite against the backend

```bash
CARGO_INCREMENTAL=0 pnpm contract:backend
```

Runs the shared contract suite (the same cases the mock passes) against the
backend: it builds the development-only `zupflow-contract-server`, restores
the reference fixture into a throwaway database, serves it on loopback and
points the suite at it. Cases that need an operation the backend doesn't serve
yet are skipped, each naming the operation. CI runs it after `pnpm check`.

For the shared suite, Taskboard journeys and hosted Connect browser tests in
one command (install Chromium as described below first):

```bash
pnpm test:integration
```

This runs the suites serially, with incremental Rust builds disabled and the
usual `server/target` directory. Each starts from a fresh temporary database,
uses the same reference fixture and stops its local processes afterwards.
The fixture has two fictional organizations, sibling workspaces, both
environments and owner, developer, viewer and outsider personas. IDs and
secrets are fresh; tests find the same logical scenarios by slug (apps by key). No provider
account or deployed service is needed.

The reference task app is **Taskboard** (`taskboard`), with a static configured
`oauth2` template under `https://taskboard.example`. It is ready in Sandbox and
deliberately incomplete in Northwind's Production environment. Its Sandbox
registry includes a current `list_tasks`, changed text for `create_task`,
changed hints for `delete_task`, and a synthetic historical `export_tasks`
marked gone. These static records use sample credentials and tool schemas;
live Taskboard journeys create a separate `mcp_oauth` app, `taskboard_live`.

Webhook fixtures include pending, succeeded, failed and skipped deliveries,
plus a test event, with frozen attempt histories for repeatable filters and
pagination checks. They queue no work, so starting workers cannot advance
those scenarios.

To click through the Console on the same fixture, serve it for the Console's
origin. Create a disposable database after the command above has built the
development binary:

```bash
fixture_dir=$(mktemp -d)
server/target/debug/zupflow-contract-server reset "$fixture_dir/contract.sqlite"
server/target/debug/zupflow-contract-server serve "$fixture_dir/contract.sqlite" \
  --console http://localhost:5181
```

`reset` is a development command that replaces that database's fixture,
never a backend HTTP endpoint. `serve --console` answers CORS for that origin
and signs a request without a token in as the owner, as `local` does; then run the Console with
`PORT=5181 VITE_ZUPFLOW_API_URL=http://127.0.0.1:<port>`, the port it prints.
Stop the server and remove the temporary directory when finished. For live
Taskboard calls and workers, use the click-through script below instead.

### Taskboard: a fake MCP app, end to end

Taskboard (`server/crates/zupflow-taskboard`, development only) is a fake MCP
app with its own OAuth, built to what an `mcp_oauth` app's sign-in expects
(decision 0039) and strict about it: protected-resource and authorization
server metadata, dynamic registration of public clients, the authorization
code flow with PKCE (`S256`) and a consent page that allows or denies, a
required `state`, `resource` naming its MCP endpoint, single-use codes,
expiring access tokens and rotating refresh tokens (a reused one revokes its
family). Its MCP endpoint (Streamable HTTP) wants a live bearer token and a
session, and serves three tools, one per hint of decision 0040: `list_tasks`
(read-only), `create_task` (writes) and `delete_task` (destructive). Each
registered client has its own board, in memory; the account is a fictional
Robin Park.

It runs on loopback under the reserved name `taskboard.example`, with a
certificate made at start for that name alone. The safe outbound path never
reaches loopback, so the contract server reaches it only when built for it
and told to. Built with its opt-in `local-apps` feature (`cargo build -p
zupflow-contract --features zupflow-contract/local-apps`),
`zupflow-contract-server serve <database> --local-app
taskboard.example:<port>=<certificate.pem>` builds its outbound client with
`SafeHttpClient::for_local_apps` (`zupflow-outbound`'s `local-apps`
feature), which pins each named app to `127.0.0.1` and its port without
asking DNS, trusts only the certificates given, still speaks HTTPS alone and
reaches nothing else. Only names under `.example` are accepted, so a local
app can never stand in for a real host. Nothing turns the feature on by
default, and CI fails if `zupflow-server`'s dependency tree ever has it; the
`zupflow` binary never builds that client. A plain `cargo build` (or
`cargo build --release`) in `server/` leaves the development-only crates out
(`default-members`). With `--local-app`, the contract server also answers
runs with the scripted model and runs the background workers, as `zupflow`
does.

`pnpm check` runs it end to end (`zupflow-contract/tests/taskboard_end_to_end.rs`,
which needs `--features zupflow-contract/local-apps`, as the check passes): it starts Taskboard and the contract server, adds
Taskboard as a catalog app in Sandbox, connects it through hosted Connect
(denied once, then allowed; a tampered or replayed callback is refused and
exchanges nothing), waits for its tools, grants an agent the tools
its levels allow, and runs it: `list_tasks` runs with the installation's
token, `create_task` waits for an approval from an API key, a denied
`delete_task` is never sent. An expired token refreshes and the run succeeds;
concurrent discovery and execution share one rotation. A revoked refresh token
marks reconnect required with its audit event and webhook, a reconnect clears
it, forgotten registrations are registered again, and foreign or missing
advertised issuers are refused without exchanging a code.
Another journey checks webhook delivery history against Taskboard's actual
signed requests: successful and failed tests, then run events across sibling
workspaces, organizations and environments. It resends a failed test after the
receiver recovers, then scoped run events, verifying identical event bytes and
valid signatures, fresh delivery attempts, unchanged source history and one
audit event under idempotent replay. Test-event resends exercise the real job
queue even though test events cannot be subscribed to.
A rotation journey races two HTTP rolls, checks the secret is returned only
once and the overlap deadline stays fixed, then verifies both signatures on
test deliveries and queued resends of events from sibling workspaces. It
also checks that another organization's endpoint and the other environment
keep their own secrets and history. Exact expiry is covered by controlled
clock tests; the HTTP journey needs no long wait or clock-control route. A
lifecycle journey checks successful and denied OAuth, cancellation,
reconnect, repeated disconnect and deletion against actual signed requests,
then resends an unchanged connected event after deleting the installation.
Store tests cover transaction rollback, races, immutable snapshots and
exact session expiry without a clock-control HTTP route.

To click through it yourself:

```bash
CARGO_INCREMENTAL=0 scripts/taskboard-dev.sh
```

It serves Taskboard (on port 7443, `TASKBOARD_PORT`) and the contract server
for a Console at `http://localhost:5181` (`CONSOLE_ORIGIN`), with Taskboard
already in Northwind's catalog as **Taskboard live** (`taskboard_live`), ready in
Sandbox, and prints the command to start the Console. Your browser must reach
`taskboard.example` at `127.0.0.1` and accept its certificate: the script prints a Chromium command that does
both for a profile of its own (add `--no-proxy-server` if your browser goes
through a proxy), or add `127.0.0.1 taskboard.example` to `/etc/hosts` and
accept the certificate warning once. Then, in a Sandbox workspace, **Add
installation**, choose **Taskboard live**, and allow access on Taskboard's page.
Runs are answered by the scripted model: `call create_task {"title":"Hello"}` calls
a tool. Taskboard's home page expires or revokes every token, to see what an
installation does then. Taskboard keeps nothing across restarts: a client the
backend registered with an earlier run is unknown to it. When refreshing
reports `invalid_client`, Zupflow forgets that registration and the next
Connect registers again.

### Hosted Connect browser tests

```bash
pnpm --filter @zupflow/contract-tests exec playwright install chromium
pnpm test:browser
# Run all journeys in one viewport and theme while developing:
pnpm test:browser --project=mobile-dark
```

On Linux, use `playwright install --with-deps chromium` to install the browser's
system dependencies too, as CI does. The suite builds the development binaries
in `server/target` with incremental compilation disabled, then runs 19 journeys
in Chromium at desktop (1280 × 800) and mobile (375 × 812) sizes, each in light
and dark mode: 76 cases. It exercises the backend's hosted page and JavaScript,
with a fresh SQLite fixture and browser context for every case. OAuth journeys
also start a fresh Taskboard server, reached over HTTPS with its own pinned
certificate; token verification uses the contract server's deterministic
provider fixture. No deployed service or provider account is needed.

Coverage includes token validation and retries, stale tabs, cancellation,
unknown and expired links, OAuth consent and denial, callback replay, state and
issuer tampering, expiry during consent, reconnecting the same installation,
and organization, workspace and environment boundaries. It also checks popup
messages, allowed and refused frames, return-URL allowlists, keyboard focus,
layout width, theme, security headers, script errors and browser storage.
Popup cancellation and denial use the page's Close window button. Those checks
handle Playwright's target-closed click race while still requiring a close event
and a responsive opener; unrelated click failures remain failures.
The browser's scope is Chromium; these checks do not establish Firefox or
Safari compatibility or compare pixel snapshots.

Expiry uses the development-only command
`zupflow-contract-server expire-connect <database> <organization> <workspace> <session>`.
It moves one still-pending session's deadline into the past only when all three
typed IDs match, leaving the normal API and expiry worker to record its outcome.
The suite checks wrong scopes, malformed IDs and already-finished sessions.
This adds no clock-control or reset HTTP endpoint.

Connect references, OAuth state and codes can appear in browser diagnostics.
Run this suite through `pnpm test:browser`: its reporter and output filter emit
only fixed case names, outcomes and allowlisted failure-step labels. Script,
security-policy and credential-leak checks have distinct labels. Traces, videos
and screenshots are disabled;
temporary profiles, databases and any automatically generated error context
stay in an owner-only temporary directory that the runner removes on completion,
failure or a handled interrupt. CI uploads no browser artifacts. Investigate a
failure using fixed, source-controlled diagnostic labels rather than logging
URLs, errors, request data or the DOM.

## Checks

```bash
pnpm check        # Biome lint + format, typecheck, tests and build for every package, Rust included
pnpm test         # Vitest and cargo test
pnpm format       # Apply Biome fixes
```

CI runs `pnpm check`, the shared backend contract suite and the hosted Connect
browser suite on every push and pull request.

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

API keys created through the mock authenticate in their own organization and
environment, including Production Connect sessions, runs and approval decisions.
Their secrets appear only in the creating response; the local mock stores an
HMAC under its instance key, plus public display metadata. Historical seeded
keys are display records with no usable secret. The shared suite creates fresh
keys through the API and exercises the same authentication, isolation,
revocation, idempotency and approval cases against both servers.

The reference fixture has a pending approval in each environment; Sandbox's is
the `qa-playground` Echo bot waiting on `echo_write`. Approving or denying it
through an API key resumes the mock's existing simulated execution. The Console
continues to observe approvals.

The mock keeps two local-development simulations, labelled as such in the UI:

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
