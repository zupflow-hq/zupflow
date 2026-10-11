/**
 * Demo data: Northwind, a fictional product that embeds Zupflow. Acme installs
 * Google Workspace twice ("Marketing" and "Finance") and owns a private
 * publishing app; Beta Labs connects its own Google account; each has Sandbox
 * counterparts. Everything is relative to the moment the seed runs.
 */
import type {
  AccountConnection,
  ApiKeyId,
  AuditEvent,
  Environment,
  IdKind,
  OrganizationId,
  ToolLevel,
  User,
  UserId,
  WebhookDeliveryState,
  WebhookEndpoint,
  WebhookEvent,
  WorkspaceId,
} from "@zupflow/sdk";
import { CATALOG, type CatalogEntry } from "./catalog";
import { newId } from "./ids";
import type {
  AgentRow,
  AppRow,
  ConfigurationRow,
  ConnectSessionRow,
  Db,
  GrantRow,
  InstallationRow,
  OrganizationRow,
  RunRow,
  WorkspaceRow,
} from "./model";
import { setLevels, suggestLevels } from "./policy";
import { mulberry32, type Rng, uuid } from "./random";
import { advanceRun, enqueueRun, planRun, resolveApproval } from "./runner";
import { appIn, timelineOf } from "./scope";
import { listedFrom, recordListing } from "./tools";

export const DB_SCHEMA = 23;
export const DEMO_USER_ID = "usr_031DCoVdPC4Z4DgUS9oR60" satisfies UserId;

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

export function seed(nowMs: number = Date.now()): Db {
  const rng = mulberry32(20261003);
  const id = <K extends IdKind>(kind: K, at = nowMs) => newId(kind, { rng, at });
  const ago = (ms: number) => new Date(nowMs - ms).toISOString();

  const users: User[] = [
    { id: DEMO_USER_ID, name: "Jordan Lee", email: "jordan@northwind.example", avatar_url: null },
    { id: id("user", nowMs - 39 * DAY), name: "Priya Natarajan", email: "priya@northwind.example", avatar_url: null },
    { id: id("user", nowMs - 20 * DAY), name: "Sam Okafor", email: "sam@northwind.example", avatar_url: null },
    { id: id("user", nowMs - 2 * DAY), name: "Mei Tanaka", email: "mei@northwind.example", avatar_url: null },
    { id: id("user", nowMs - 15 * DAY), name: "Alex Rivera", email: "alex@northwind.example", avatar_url: null },
  ];
  const [jordan, priya, sam, mei, alex] = users as [User, User, User, User, User];

  const org = {
    id: id("organization", nowMs - 41 * DAY),
    name: "Northwind",
    slug: "northwind",
    created_at: ago(41 * DAY),
  };

  const memberships = [
    {
      id: id("member", nowMs - 41 * DAY),
      organization_id: org.id,
      user_id: jordan.id,
      role: "owner" as const,
      status: "active" as const,
      invited_at: org.created_at,
      invited_by: null,
      joined_at: org.created_at,
      last_active_at: ago(4 * MINUTE),
    },
    {
      id: id("member", nowMs - 39 * DAY),
      organization_id: org.id,
      user_id: priya.id,
      role: "admin" as const,
      status: "active" as const,
      invited_at: ago(39 * DAY),
      invited_by: jordan.id,
      joined_at: ago(39 * DAY),
      last_active_at: ago(3 * HOUR),
    },
    {
      id: id("member", nowMs - 20 * DAY),
      organization_id: org.id,
      user_id: sam.id,
      role: "developer" as const,
      status: "active" as const,
      invited_at: ago(20 * DAY),
      invited_by: jordan.id,
      joined_at: ago(19 * DAY),
      last_active_at: ago(2 * DAY),
    },
    {
      id: id("member", nowMs - 2 * DAY),
      organization_id: org.id,
      user_id: mei.id,
      role: "developer" as const,
      status: "invited" as const,
      invited_at: ago(2 * DAY),
      invited_by: jordan.id,
      joined_at: null,
      last_active_at: null,
    },
    {
      // Read-only: support sees what developers see, and changes nothing.
      id: id("member", nowMs - 15 * DAY),
      organization_id: org.id,
      user_id: alex.id,
      role: "viewer" as const,
      status: "active" as const,
      invited_at: ago(15 * DAY),
      invited_by: jordan.id,
      joined_at: ago(15 * DAY),
      last_active_at: ago(1 * DAY),
    },
  ];

  const workspace = (
    name: string,
    slug: string,
    environment: Environment,
    externalRef: string | null,
    createdAgo: number,
  ): WorkspaceRow => ({
    id: id("workspace", nowMs - createdAgo),
    organization_id: org.id,
    name,
    slug,
    environment,
    external_id: externalRef,
    created_at: ago(createdAgo),
  });
  const acme = workspace("Acme", "acme", "production", "tenant_8c1f2a", 34 * DAY);
  const beta = workspace("Beta Labs", "beta-labs", "production", "tenant_41d9e0", 27 * DAY);
  const acmeSandbox = workspace("Acme", "acme-sandbox", "sandbox", "tenant_8c1f2a", 36 * DAY);
  const qa = workspace("QA playground", "qa-playground", "sandbox", null, 12 * DAY);
  const workspaces = [acme, beta, acmeSandbox, qa, ...tenantWorkspaces(rng, org.id, nowMs)];

  // --- Apps -----------------------------------------------------------------

  const apps: AppRow[] = [];
  const configurations: ConfigurationRow[] = [];

  const addApp = (entry: CatalogEntry, owner: WorkspaceRow | null, createdAgo: number) => {
    const app: AppRow = {
      id: id("app", nowMs - createdAgo),
      organization_id: org.id,
      owner_workspace_id: owner?.id ?? null,
      kind: "mcp",
      name: entry.name,
      slug: entry.slug,
      description: entry.description,
      icon: entry.icon,
      auth: entry.auth,
      tools: entry.tools,
      version: 1 + Math.floor(rng() * 3),
      created_at: ago(createdAgo),
      updated_at: ago(createdAgo - DAY),
    };
    apps.push(app);
    return app;
  };

  type ConfigState = "ready" | "incomplete" | "unconfigured";
  const configure = (
    app: AppRow,
    environment: Environment,
    state: ConfigState,
    workspaceId: WorkspaceId | null,
    updatedBy: User,
    updatedAgo: number,
  ) => {
    const entry = Object.values(CATALOG).find((c) => c.slug === app.slug);
    const oauth = app.auth.type === "oauth2";
    const has = state !== "unconfigured";
    const scopes = app.auth.type === "oauth2" ? app.auth.available_scopes.slice(0, 4).map((s) => s.name) : [];
    configurations.push({
      organization_id: org.id,
      workspace_id: workspaceId,
      app_id: app.id,
      environment,
      version: has ? 1 + Math.floor(rng() * 4) : 0,
      mcp_server_url: has ? (entry?.mcp_server_url ?? null) : null,
      oauth_client_id: has && oauth ? `${Math.floor(rng() * 9e11)}-${environment.slice(0, 4)}.apps.example` : null,
      oauth_client_secret_last4: state === "ready" && oauth ? uuid(rng).slice(-4) : null,
      oauth_client_secret_updated_at: state === "ready" && oauth ? ago(updatedAgo) : null,
      oauth_scopes: has ? scopes : [],
      updated_at: has ? ago(updatedAgo) : null,
      updated_by_user_id: has ? updatedBy.id : null,
    });
  };

  const google = addApp(CATALOG.googleWorkspace, null, 40 * DAY);
  configure(google, "sandbox", "ready", null, jordan, 38 * DAY);
  configure(google, "production", "ready", null, jordan, 35 * DAY);

  const taskboard = addApp(CATALOG.taskboard, null, 30 * DAY);
  configure(taskboard, "sandbox", "ready", null, priya, 29 * DAY);
  configure(taskboard, "production", "incomplete", null, priya, 6 * DAY);

  const stripe = addApp(CATALOG.stripe, null, 26 * DAY);
  configure(stripe, "sandbox", "ready", null, priya, 26 * DAY);
  configure(stripe, "production", "ready", null, priya, 25 * DAY);

  const github = addApp(CATALOG.github, null, 18 * DAY);
  configure(github, "sandbox", "ready", null, sam, 18 * DAY);
  configure(github, "production", "ready", null, jordan, 15 * DAY);

  const notion = addApp(CATALOG.notion, null, 3 * DAY);
  configure(notion, "sandbox", "unconfigured", null, jordan, 3 * DAY);
  configure(notion, "production", "unconfigured", null, jordan, 3 * DAY);

  const publishing = addApp(CATALOG.acmePublishing, acme, 22 * DAY);
  configure(publishing, "production", "ready", acme.id, sam, 21 * DAY);

  const echo = addApp(CATALOG.echoServer, qa, 11 * DAY);
  configure(echo, "sandbox", "ready", qa.id, sam, 11 * DAY);

  const fieldnote = addApp(CATALOG.fieldnote, null, 8 * DAY);
  configure(fieldnote, "sandbox", "ready", null, priya, 8 * DAY);
  configure(fieldnote, "production", "unconfigured", null, priya, 8 * DAY);

  // --- Installations ----------------------------------------------------------

  const installations: InstallationRow[] = [];
  const install = (
    ws: WorkspaceRow,
    app: AppRow,
    label: string,
    account: { name: string; email: string | null } | null,
    createdAgo: number,
    status: InstallationRow["status"] = "connected",
  ) => {
    const configuration = configurations.find((c) => c.app_id === app.id && c.environment === ws.environment);
    const connection: AccountConnection | null = account
      ? {
          account: {
            external_id: String(100000000 + Math.floor(rng() * 899999999)),
            display_name: account.name,
            email: account.email,
          },
          granted_scopes:
            app.auth.type === "mcp_oauth"
              ? app.auth.scopes.map((scope) => scope.name)
              : (configuration?.oauth_scopes ?? []),
          credential_version: 1 + Math.floor(rng() * 3),
          connected_at: ago(createdAgo),
          expires_at: app.auth.type === "oauth2" ? ago(-50 * MINUTE) : null,
        }
      : null;
    const row: InstallationRow = {
      id: id("installation", nowMs - createdAgo),
      organization_id: org.id,
      workspace_id: ws.id,
      app_id: app.id,
      label,
      status,
      configuration_version: configuration?.version ?? 1,
      connection,
      created_at: ago(createdAgo),
      updated_at: ago(Math.max(createdAgo - 2 * DAY, HOUR)),
    };
    installations.push(row);
    return row;
  };

  const acmeMarketing = install(
    acme,
    google,
    "Marketing",
    { name: "Acme Marketing", email: "marketing@acme.example" },
    30 * DAY,
  );
  const acmeFinance = install(
    acme,
    google,
    "Finance",
    { name: "Acme Finance", email: "finance@acme.example" },
    29 * DAY,
  );
  const acmeBlog = install(acme, publishing, "Blog", { name: "editor@acme", email: null }, 21 * DAY);
  const acmeBilling = install(acme, stripe, "Billing", { name: "Acme Inc. (acct_1Nq…)", email: null }, 24 * DAY);
  const acmeEng = install(acme, github, "Engineering", { name: "acme-bot", email: "eng@acme.example" }, 14 * DAY);
  const betaGoogle = install(
    beta,
    google,
    "Beta Labs",
    { name: "Beta Labs Ops", email: "ops@betalabs.example" },
    26 * DAY,
  );
  const sandboxMarketing = install(
    acmeSandbox,
    google,
    "Marketing (test)",
    { name: "Acme Test", email: "test-marketing@acme.example" },
    35 * DAY,
  );
  const sandboxTaskboard = install(
    acmeSandbox,
    taskboard,
    "Product",
    { name: "Acme Product (test)", email: "pm-test@acme.example" },
    28 * DAY,
  );
  const qaEcho = install(qa, echo, "Default", { name: "echo", email: null }, 11 * DAY);
  // Nothing names a Fieldnote account: it's anonymous, as sign-in through an MCP server without an identity URL leaves it.
  install(acmeSandbox, fieldnote, "Field team", { name: "Fieldnote account", email: null }, 7 * DAY);
  install(qa, github, "Old bot", null, 9 * DAY, "disconnected");

  // --- Agents and grants ------------------------------------------------------

  const agents: AgentRow[] = [];
  const grants: GrantRow[] = [];
  const agent = (
    ws: WorkspaceRow,
    name: string,
    description: string,
    instructions: string,
    model: string,
    createdAgo: number,
    status: AgentRow["status"] = "active",
  ) => {
    const row: AgentRow = {
      id: id("agent", nowMs - createdAgo),
      organization_id: ws.organization_id,
      workspace_id: ws.id,
      name,
      description,
      model,
      instructions,
      status,
      created_at: ago(createdAgo),
      updated_at: ago(Math.max(createdAgo - 3 * DAY, HOUR)),
    };
    agents.push(row);
    return row;
  };
  const grant = (a: AgentRow, installation: InstallationRow, tools: string[], approval: string[] = []) => {
    grants.push({
      id: id("grant", Date.parse(a.created_at)),
      organization_id: a.organization_id,
      workspace_id: a.workspace_id,
      agent_id: a.id,
      installation_id: installation.id,
      tools,
      approval_required: approval,
      created_at: a.created_at,
      updated_at: a.updated_at,
    });
  };

  const publishingInstructions = `You help Acme's marketing team turn plans into published posts.

- Read source material from the Marketing drive before drafting.
- Write in Acme's brand voice: short sentences, no exclamation marks.
- Never publish without an explicit request from the user.`;

  const acmePublisher = agent(
    acme,
    "Publishing assistant",
    "Drafts and publishes blog posts from Marketing's Drive.",
    publishingInstructions,
    "claude-sonnet-5-5",
    28 * DAY,
  );
  grant(acmePublisher, acmeMarketing, ["search_files", "read_document", "create_document"]);
  grant(acmePublisher, acmeBlog, ["list_drafts", "create_draft", "publish_post"], ["publish_post"]);

  const acmeReporter = agent(
    acme,
    "Finance reporter",
    "Weekly revenue and invoice summaries.",
    "Summarize open invoices and revenue for the finance team. Read-only: never change billing data.",
    "claude-sonnet-5-5",
    23 * DAY,
  );
  grant(acmeReporter, acmeFinance, ["search_files", "read_document", "send_email"], ["send_email"]);
  grant(acmeReporter, acmeBilling, ["list_invoices", "get_customer"]);

  const acmeTriage = agent(
    acme,
    "Engineering triage",
    "Finds related code and pull requests for incoming bug reports.",
    "Given a bug report, search the codebase and open pull requests, then summarize likely causes.",
    "claude-haiku-4-5-20251001",
    13 * DAY,
  );
  grant(acmeTriage, acmeEng, ["search_code", "list_pull_requests", "create_issue"], ["create_issue"]);

  agent(
    acme,
    "Support concierge",
    "Answers customer questions. Not yet granted any installations.",
    "Answer support questions politely and briefly.",
    "claude-haiku-4-5-20251001",
    4 * DAY,
    "paused",
  );

  const betaOps = agent(
    beta,
    "Ops assistant",
    "Keeps Beta Labs' calendar and documents in order.",
    "Help the operations team with scheduling and documents.",
    "claude-sonnet-5-5",
    25 * DAY,
  );
  grant(betaOps, betaGoogle, ["list_events", "search_files", "read_document", "send_email"], ["send_email"]);

  const sandboxPublisher = agent(
    acmeSandbox,
    "Publishing assistant",
    "Sandbox twin of the production publishing agent.",
    publishingInstructions,
    "claude-sonnet-5-5",
    33 * DAY,
  );
  grant(
    sandboxPublisher,
    sandboxMarketing,
    ["search_files", "read_document", "create_document", "delete_file"],
    ["delete_file"],
  );
  const sandboxPm = agent(
    acmeSandbox,
    "Product scribe",
    "Turns meeting notes into Taskboard tasks.",
    "Turn notes into well-scoped Taskboard tasks. Ask before creating more than three.",
    "claude-sonnet-5-5",
    27 * DAY,
  );
  grant(sandboxPm, sandboxTaskboard, ["list_tasks", "create_task"]);

  const echoBot = agent(
    qa,
    "Echo bot",
    "Exercises grants and approvals against the echo server.",
    "Call echo with the user's message, then echo_write.",
    "claude-haiku-4-5-20251001",
    10 * DAY,
  );
  grant(echoBot, qaEcho, ["echo", "echo_write"], ["echo_write"]);

  // --- Runs -------------------------------------------------------------------

  const db: Db = {
    schema: DB_SCHEMA,
    users,
    organizations: [org],
    memberships,
    workspaces,
    apps,
    configurations,
    installations,
    installation_tools: [],
    tool_refreshes: [],
    tool_policies: [],
    app_tool_levels: [],
    connect_sessions: [],
    agents,
    grants,
    runs: [],
    run_events: {},
    connect_settings: [
      {
        organization_id: org.id,
        environment: "sandbox",
        return_urls: [
          "http://localhost:3000/settings/integrations",
          "https://staging.northwind.example/settings/integrations",
        ],
        allowed_origins: ["http://localhost:3000", "https://staging.northwind.example"],
        updated_at: ago(20 * DAY),
      },
      {
        organization_id: org.id,
        environment: "production",
        return_urls: ["https://app.northwind.example/settings/integrations"],
        allowed_origins: ["https://app.northwind.example"],
        updated_at: ago(18 * DAY),
      },
    ],
    api_keys: [
      {
        id: id("api_key", nowMs - 37 * DAY),
        organization_id: org.id,
        environment: "sandbox",
        name: "Local development",
        secret_hash: null,
        prefix: "zf_test_4hTq",
        last4: "w2Ka",
        created_by_user_id: jordan.id,
        created_at: ago(37 * DAY),
        last_used_at: ago(26 * MINUTE),
        revoked_at: null,
      },
      {
        id: id("api_key", nowMs - 15 * DAY),
        organization_id: org.id,
        environment: "sandbox",
        name: "CI",
        secret_hash: null,
        prefix: "zf_test_Pm7c",
        last4: "9QxE",
        created_by_user_id: sam.id,
        created_at: ago(15 * DAY),
        last_used_at: ago(5 * HOUR),
        revoked_at: null,
      },
      {
        id: id("api_key", nowMs - 34 * DAY),
        organization_id: org.id,
        environment: "production",
        name: "Northwind API",
        secret_hash: null,
        prefix: "zf_live_9KcB",
        last4: "h3Ld",
        created_by_user_id: jordan.id,
        created_at: ago(34 * DAY),
        last_used_at: ago(40 * 1000),
        revoked_at: null,
      },
      {
        id: id("api_key", nowMs - 33 * DAY),
        organization_id: org.id,
        environment: "production",
        name: "Old worker key",
        secret_hash: null,
        prefix: "zf_live_2vRs",
        last4: "Ty8n",
        created_by_user_id: priya.id,
        created_at: ago(33 * DAY),
        last_used_at: ago(16 * DAY),
        revoked_at: ago(14 * DAY),
      },
    ],
    webhooks: [] as WebhookEndpoint[],
    webhook_events: [],
    webhook_deliveries: [],
    webhook_delivery_attempts: [],
    audit: [] as Db["audit"],
    idempotency_keys: [],
    // Secure randomness, not the seed's: this key is a secret.
    secret_key: Array.from(crypto.getRandomValues(new Uint8Array(32)), (b) => b.toString(16).padStart(2, "0")).join(""),
  };

  db.webhooks.push(
    {
      id: id("webhook_endpoint", nowMs - 30 * DAY),
      organization_id: org.id,
      environment: "production",
      url: "https://hooks.northwind.example/zupflow",
      description: "Run and installation status for the Northwind API",
      events: [
        "installation.connected",
        "installation.reconnect_required",
        "installation.disconnected",
        "run.completed",
        "run.failed",
        "run.waiting_for_approval",
      ],
      enabled: true,
      signing_secret_last4: "fA2p",
      previous_signing_secret_expires_at: null,
      created_at: ago(30 * DAY),
      last_delivery: { status_code: 200, delivered_at: ago(3 * MINUTE) },
    },
    {
      id: id("webhook_endpoint", nowMs - 29 * DAY),
      organization_id: org.id,
      environment: "sandbox",
      url: "https://staging.northwind.example/hooks/zupflow",
      description: "Staging",
      events: ["connect_session.completed", "connect_session.failed", "run.completed", "run.failed"],
      enabled: true,
      signing_secret_last4: "Lr0d",
      previous_signing_secret_expires_at: null,
      created_at: ago(29 * DAY),
      last_delivery: { status_code: 503, delivered_at: ago(2 * HOUR) },
    },
  );

  seedRuns(db, rng, nowMs, [
    { agent: acmePublisher, count: 46, prompts: PUBLISHING_PROMPTS, waiting: true },
    { agent: acmeReporter, count: 28, prompts: FINANCE_PROMPTS },
    { agent: acmeTriage, count: 22, prompts: TRIAGE_PROMPTS },
    { agent: betaOps, count: 30, prompts: OPS_PROMPTS },
    { agent: sandboxPublisher, count: 24, prompts: PUBLISHING_PROMPTS },
    { agent: sandboxPm, count: 14, prompts: PM_PROMPTS },
    { agent: echoBot, count: 12, prompts: ["ping", "hello from QA", "approval check", "write test"], waiting: true },
  ]);

  // Billing broke a few hours ago; earlier runs used it while it worked.
  acmeBilling.status = "reconnect_required";
  acmeBilling.updated_at = ago(6 * HOUR);

  seedConnectSessions(db, rng, nowMs, [
    { ws: acme, installation: acmeMarketing, app: google },
    { ws: acme, installation: acmeEng, app: github },
    { ws: beta, installation: betaGoogle, app: google },
    { ws: acmeSandbox, installation: sandboxTaskboard, app: taskboard },
  ]);
  db.connect_sessions.push({
    id: id("connect_session", nowMs - 5 * HOUR),
    organization_id: org.id,
    workspace_id: acme.id,
    reference_hash: null,
    environment: "production",
    app_id: stripe.id,
    configuration_version: 1,
    intent: "reconnect",
    installation_id: acmeBilling.id,
    label: acmeBilling.label,
    status: "failed",
    return_url: "https://app.northwind.example/settings/integrations",
    requested_scopes: [],
    created_at: ago(5 * HOUR),
    expires_at: ago(5 * HOUR - 15 * MINUTE),
    completed_at: null,
    failure_code: "credential_rejected",
  });

  seedAudit(
    db,
    rng,
    nowMs,
    { jordan, priya, sam },
    { acme, beta, acmeSandbox, qa },
    {
      google,
      taskboard,
      stripe,
      notion,
      publishing,
      acmeBilling,
      acmePublisher,
    },
  );

  seedLarkspur(db, nowMs);
  seedFernhill(db, nowMs);
  seedTools(db, nowMs, sandboxTaskboard);
  seedLevels(db, nowMs, { google, taskboard, stripe, github }, jordan.id);
  seedWebhookHistory(db, nowMs);
  // Seeded runs record their approvals as they're decided: newest first, like the log.
  db.audit.sort((a, b) => b.created_at.localeCompare(a.created_at));
  return db;
}

/** Fixed scenarios and times relative to `nowMs`, so resetting repeats every delivery outcome. */
function seedWebhookHistory(db: Db, nowMs: number) {
  const rng = mulberry32(20261011);
  for (const endpoint of db.webhooks) {
    const slugs =
      endpoint.environment === "sandbox" ? ["acme-sandbox", "qa-playground"] : ["acme", "beta-labs", "harbor"];
    const workspaces = db.workspaces.filter(
      (workspace) =>
        workspace.organization_id === endpoint.organization_id &&
        workspace.environment === endpoint.environment &&
        slugs.includes(workspace.slug),
    );
    if (workspaces.length === 0) continue;
    const scenarios: { state: WebhookDeliveryState; count: number; test?: true }[] = [
      { state: "pending", count: 2 },
      { state: "succeeded", count: 2 },
      { state: "failed", count: 8 },
      { state: "skipped", count: 0 },
      { state: "succeeded", count: 1, test: true },
    ];
    for (const [index, scenario] of scenarios.entries()) {
      const workspace = workspaces[index % workspaces.length];
      if (!workspace) continue;
      const run = db.runs.find(
        (row) =>
          row.organization_id === endpoint.organization_id &&
          row.workspace_id === workspace.id &&
          row.status === "completed",
      );
      if (!run) continue;
      const at = nowMs - (index + 1) * HOUR;
      const created_at = new Date(at).toISOString();
      const event_id = newId("webhook_event", { rng, at });
      const id = newId("webhook_delivery", { rng, at });
      const common = {
        id: event_id,
        organization_id: endpoint.organization_id,
        environment: endpoint.environment,
        created_at,
      };
      const event: WebhookEvent = scenario.test
        ? { ...common, type: "webhook.test", workspace: null, data: { object: "webhook_endpoint", id: endpoint.id } }
        : {
            ...common,
            type: "run.completed",
            workspace: { id: workspace.id, external_id: workspace.external_id },
            data: { object: "run", id: run.id, agent_id: run.agent_id, status: "completed" },
          };
      db.webhook_events.push({
        id: event_id,
        organization_id: endpoint.organization_id,
        workspace_id: event.workspace?.id ?? null,
        environment: endpoint.environment,
        body: event,
      });
      const last_attempt_at = scenario.count ? new Date(at + scenario.count * 1000).toISOString() : null;
      db.webhook_deliveries.push({
        id,
        organization_id: endpoint.organization_id,
        endpoint_id: endpoint.id,
        event_id,
        state: scenario.state,
        attempts: scenario.count,
        last_status_code: scenario.count ? (scenario.state === "succeeded" ? 200 : 503) : null,
        last_error: scenario.count && scenario.state !== "succeeded" ? "status" : null,
        last_attempt_at,
        next_attempt_at: scenario.state === "pending" ? new Date(nowMs + MINUTE).toISOString() : null,
        created_at,
      });
      for (let number = 1; number <= scenario.count; number++) {
        const succeeded = scenario.state === "succeeded" && number === scenario.count;
        db.webhook_delivery_attempts.push({
          organization_id: endpoint.organization_id,
          endpoint_id: endpoint.id,
          delivery_id: id,
          number,
          status_code: succeeded ? 200 : 503,
          error_class: succeeded ? null : "status",
          attempted_at: new Date(at + number * 1000).toISOString(),
          duration_ms: 12 + number,
        });
      }
    }
  }
}

/**
 * The organizations' levels for their catalog apps' tools: suggested for
 * every tool an installation listed, then set by a person for most of
 * Northwind's, so the grants seeded above hold inside them. `list_events`,
 * Notion's tools and the gone `export_tasks` stay suggested only, and
 * Google Workspace's `delete_file` is denied: the Sandbox grant that still
 * holds it shows that tightening a level doesn't rewrite a grant.
 */
function seedLevels(
  db: Db,
  nowMs: number,
  apps: { google: AppRow; taskboard: AppRow; stripe: AppRow; github: AppRow },
  confirmedBy: UserId,
) {
  for (const tool of db.installation_tools) {
    const app = appIn(db.apps, { ...tool, app_id: installationOf(db, tool).app_id });
    if (app?.owner_workspace_id !== null || !app) continue;
    suggestLevels(db, tool.organization_id, app.id, [tool], tool.first_seen_at);
  }
  const at = new Date(nowMs - 9 * DAY).toISOString();
  const confirm = (app: AppRow, levels: Record<string, ToolLevel>) =>
    setLevels(
      db,
      app.organization_id,
      app.id,
      Object.entries(levels).map(([tool, level]) => ({ tool, level })),
      confirmedBy,
      at,
    );
  confirm(apps.google, {
    search_files: "allow",
    read_document: "allow",
    create_document: "allow",
    send_email: "approve",
    delete_file: "deny",
  });
  // These confirmed levels exercise policy independently of hints; delete_task is never seeded in a grant.
  confirm(apps.taskboard, { list_tasks: "allow", create_task: "allow", delete_task: "allow" });
  confirm(apps.stripe, { list_invoices: "allow", get_customer: "allow", refund_payment: "approve" });
  confirm(apps.github, { search_code: "allow", list_pull_requests: "allow", create_issue: "approve" });

  // Larkspur holds its grant inside its levels too.
  const larkspur = db.organizations.find((o) => o.slug === "larkspur");
  const owner = db.memberships.find((m) => m.organization_id === larkspur?.id && m.role === "owner");
  const larkspurTaskboard = db.apps.find((a) => a.organization_id === larkspur?.id && a.slug === "taskboard");
  if (!owner || !larkspurTaskboard) throw new Error("Larkspur's seed is missing");
  setLevels(
    db,
    larkspurTaskboard.organization_id,
    larkspurTaskboard.id,
    [
      { tool: "list_tasks", level: "allow" },
      { tool: "create_task", level: "allow" },
    ],
    owner.user_id,
    at,
  );
}

function installationOf(db: Db, tool: Db["installation_tools"][number]) {
  const installation = db.installations.find(
    (i) =>
      i.organization_id === tool.organization_id &&
      i.workspace_id === tool.workspace_id &&
      i.id === tool.installation_id,
  );
  if (!installation) throw new Error("A seeded tool's installation is missing");
  return installation;
}

/**
 * Each installation's tools, as discovery recorded them when it connected.
 * `reviewed` shows the registry's other states: listed first with older
 * definitions of two tools (one's text, one's hints only) and a tool its
 * server has since dropped, then again two days ago, so two tools are changed
 * and unreviewed and one is gone.
 */
function seedTools(db: Db, nowMs: number, reviewed: InstallationRow) {
  for (const installation of db.installations) {
    const app = appIn(db.apps, installation);
    if (!installation.connection || !app) continue;
    const listed = app.tools.map(listedFrom);
    if (installation === reviewed) {
      // `create_task` said less then; `delete_task` changed only its hints since.
      const older = listed.map((tool) =>
        tool.name === "create_task"
          ? { ...tool, text: { ...tool.text, description: "Adds a task." } }
          : tool.name === "delete_task"
            ? { ...tool, text: { ...tool.text, annotations: { readOnlyHint: false } } }
            : tool,
      );
      // Synthetic historical tombstone for the gone-tool scenario, not a tool Taskboard serves.
      older.push({
        name: "export_tasks",
        effect: "read",
        text: {
          title: "Export tasks",
          description: "Download the board's tasks as a CSV file.",
          input_schema: { type: "object" },
          output_schema: null,
          annotations: { readOnlyHint: true },
        },
      });
      recordListing(db, installation, older, installation.connection.connected_at);
      recordListing(db, installation, listed, new Date(nowMs - 2 * DAY).toISOString());
    } else {
      recordListing(db, installation, listed, installation.connection.connected_at);
    }
  }
}

/**
 * A second, unrelated organization. Nobody in Northwind belongs to it, and it
 * has its own owner, workspace, app, installation, agent, grant, runs, key and
 * webhook, so the contract tests can use each organization's real IDs against
 * the other. Its own randomness, so adding it changes nothing about Northwind.
 */
/**
 * Fernhill: an organization that invited Jordan yesterday, so the demo has an
 * invitation to accept or decline. Nothing else of it is seeded. Its own
 * randomness, so adding it changes nothing about the others.
 */
function seedFernhill(db: Db, nowMs: number) {
  const rng = mulberry32(20261010);
  const ago = (ms: number) => new Date(nowMs - ms).toISOString();
  const id = <K extends IdKind>(kind: K, agoMs: number) => newId(kind, { rng, at: nowMs - agoMs });
  const casey: User = {
    id: id("user", 30 * DAY),
    name: "Casey Morgan",
    email: "casey@fernhill.example",
    avatar_url: null,
  };
  const org: OrganizationRow = {
    id: id("organization", 30 * DAY),
    name: "Fernhill",
    slug: "fernhill",
    created_at: ago(30 * DAY),
  };
  db.users.push(casey);
  db.organizations.push(org);
  for (const environment of ["sandbox", "production"] as const) {
    db.connect_settings.push({
      organization_id: org.id,
      environment,
      return_urls: [],
      allowed_origins: [],
      updated_at: null,
    });
  }
  db.memberships.push(
    {
      id: id("member", 30 * DAY),
      organization_id: org.id,
      user_id: casey.id,
      role: "owner",
      status: "active",
      invited_at: org.created_at,
      invited_by: null,
      joined_at: org.created_at,
      last_active_at: ago(2 * HOUR),
    },
    {
      id: id("member", DAY),
      organization_id: org.id,
      user_id: DEMO_USER_ID,
      role: "developer",
      status: "invited",
      invited_at: ago(DAY),
      invited_by: casey.id,
      joined_at: null,
      last_active_at: null,
    },
  );
}

function seedLarkspur(db: Db, nowMs: number) {
  const rng = mulberry32(20261004);
  const ago = (ms: number) => new Date(nowMs - ms).toISOString();
  const id = <K extends IdKind>(kind: K, agoMs: number) => newId(kind, { rng, at: nowMs - agoMs });

  const rowan: User = {
    id: id("user", 20 * DAY),
    name: "Rowan Ellis",
    email: "rowan@larkspur.example",
    avatar_url: null,
  };
  const org: OrganizationRow = {
    id: id("organization", 20 * DAY),
    name: "Larkspur Labs",
    slug: "larkspur",
    created_at: ago(20 * DAY),
  };
  db.users.push(rowan);
  db.organizations.push(org);
  db.memberships.push({
    id: id("member", 20 * DAY),
    organization_id: org.id,
    user_id: rowan.id,
    role: "owner",
    status: "active",
    invited_at: org.created_at,
    invited_by: null,
    joined_at: org.created_at,
    last_active_at: ago(HOUR),
  });
  // An invitation for Northwind's viewer that ran out before anyone accepted it:
  // the contract suite's expired invitation.
  const alex = db.users.find((u) => u.email === "alex@northwind.example");
  if (!alex) throw new Error("Larkspur's seed needs Northwind's viewer");
  db.memberships.push({
    id: id("member", 10 * DAY),
    organization_id: org.id,
    user_id: alex.id,
    role: "developer",
    status: "invited",
    invited_at: ago(10 * DAY),
    invited_by: rowan.id,
    joined_at: null,
    last_active_at: null,
  });

  const harbor: WorkspaceRow = {
    id: id("workspace", 19 * DAY),
    organization_id: org.id,
    name: "Harbor",
    slug: "harbor",
    environment: "production",
    external_id: "acct_harbor_01",
    created_at: ago(19 * DAY),
  };
  const harborSandbox: WorkspaceRow = {
    id: id("workspace", 19 * DAY - HOUR),
    organization_id: org.id,
    name: "Harbor",
    slug: "harbor-sandbox",
    environment: "sandbox",
    external_id: "acct_harbor_01",
    created_at: ago(19 * DAY - HOUR),
  };
  db.workspaces.push(harbor, harborSandbox);

  const entry = CATALOG.taskboard;
  const app: AppRow = {
    id: id("app", 18 * DAY),
    organization_id: org.id,
    owner_workspace_id: null,
    kind: "mcp",
    name: entry.name,
    slug: entry.slug,
    description: entry.description,
    icon: entry.icon,
    auth: entry.auth,
    tools: entry.tools,
    version: 1,
    created_at: ago(18 * DAY),
    updated_at: ago(18 * DAY),
  };
  db.apps.push(app);
  const scopes = entry.auth.type === "oauth2" ? entry.auth.available_scopes.slice(0, 2).map((s) => s.name) : [];
  db.configurations.push({
    organization_id: org.id,
    workspace_id: null,
    app_id: app.id,
    environment: "sandbox",
    version: 1,
    mcp_server_url: entry.mcp_server_url ?? null,
    oauth_client_id: entry.auth.type === "oauth2" ? "larkspur-test.apps.example" : null,
    oauth_client_secret_last4: entry.auth.type === "oauth2" ? "p2Wn" : null,
    oauth_client_secret_updated_at: entry.auth.type === "oauth2" ? ago(18 * DAY) : null,
    oauth_scopes: scopes,
    updated_at: ago(18 * DAY),
    updated_by_user_id: rowan.id,
  });
  db.configurations.push({
    organization_id: org.id,
    workspace_id: null,
    app_id: app.id,
    environment: "production",
    version: 1,
    mcp_server_url: entry.mcp_server_url ?? null,
    oauth_client_id: entry.auth.type === "oauth2" ? "larkspur-prod.apps.example" : null,
    oauth_client_secret_last4: entry.auth.type === "oauth2" ? "k9Qz" : null,
    oauth_client_secret_updated_at: entry.auth.type === "oauth2" ? ago(18 * DAY) : null,
    oauth_scopes: scopes,
    updated_at: ago(18 * DAY),
    updated_by_user_id: rowan.id,
  });

  const installation: InstallationRow = {
    id: id("installation", 17 * DAY),
    organization_id: org.id,
    workspace_id: harbor.id,
    app_id: app.id,
    label: "Engineering",
    status: "connected",
    configuration_version: 1,
    connection: {
      account: { external_id: "553201977", display_name: "Harbor Engineering", email: "eng@harbor.example" },
      granted_scopes: scopes,
      credential_version: 1,
      connected_at: ago(17 * DAY),
      expires_at: entry.auth.type === "oauth2" ? ago(-50 * MINUTE) : null,
    },
    created_at: ago(17 * DAY),
    updated_at: ago(17 * DAY),
  };
  db.installations.push(installation);

  const agent: AgentRow = {
    id: id("agent", 16 * DAY),
    organization_id: org.id,
    workspace_id: harbor.id,
    name: "Triage assistant",
    description: "Creates and summarizes board tasks.",
    model: "claude-sonnet-5-5",
    instructions: "Keep task titles short and notes clear.",
    status: "active",
    created_at: ago(16 * DAY),
    updated_at: ago(16 * DAY),
  };
  db.agents.push(agent);
  db.grants.push({
    id: id("grant", 16 * DAY),
    organization_id: org.id,
    workspace_id: harbor.id,
    agent_id: agent.id,
    installation_id: installation.id,
    tools: entry.tools.slice(0, 2).map((t) => t.name),
    approval_required: [],
    created_at: ago(16 * DAY),
    updated_at: ago(16 * DAY),
  });

  db.api_keys.push({
    id: id("api_key", 18 * DAY),
    organization_id: org.id,
    environment: "production",
    name: "Larkspur production",
    secret_hash: null,
    prefix: "zf_live_8kLm",
    last4: "Tq3v",
    created_by_user_id: rowan.id,
    created_at: ago(18 * DAY),
    last_used_at: ago(2 * HOUR),
    revoked_at: null,
  });
  db.webhooks.push({
    id: id("webhook_endpoint", 18 * DAY),
    organization_id: org.id,
    environment: "production",
    url: "https://app.larkspur.example/hooks/zupflow",
    description: "Production",
    events: ["run.completed", "run.failed"],
    enabled: true,
    signing_secret_last4: "Hb7c",
    previous_signing_secret_expires_at: null,
    created_at: ago(18 * DAY),
    last_delivery: { status_code: 200, delivered_at: ago(4 * HOUR) },
  });

  const toolsEntry = CATALOG.echoServer;
  const workspaceApp: AppRow = {
    id: id("app", 15 * DAY),
    organization_id: org.id,
    owner_workspace_id: harbor.id,
    kind: "mcp",
    name: "Harbor tools",
    slug: "harbor-tools",
    description: toolsEntry.description,
    icon: toolsEntry.icon,
    auth: toolsEntry.auth,
    tools: toolsEntry.tools,
    version: 1,
    created_at: ago(15 * DAY),
    updated_at: ago(15 * DAY),
  };
  db.apps.push(workspaceApp);
  db.configurations.push({
    organization_id: org.id,
    workspace_id: harbor.id,
    app_id: workspaceApp.id,
    environment: "production",
    version: 1,
    mcp_server_url: toolsEntry.mcp_server_url ?? null,
    oauth_client_id: null,
    oauth_client_secret_last4: null,
    oauth_client_secret_updated_at: null,
    oauth_scopes: [],
    updated_at: ago(15 * DAY),
    updated_by_user_id: rowan.id,
  });

  db.connect_sessions.push({
    id: id("connect_session", 17 * DAY + 2 * MINUTE),
    organization_id: org.id,
    workspace_id: harbor.id,
    reference_hash: null,
    environment: "production",
    app_id: app.id,
    configuration_version: 1,
    intent: "create",
    installation_id: installation.id,
    label: installation.label,
    status: "completed",
    return_url: "https://app.larkspur.example/integrations",
    requested_scopes: scopes,
    created_at: ago(17 * DAY + 2 * MINUTE),
    expires_at: ago(17 * DAY - 13 * MINUTE),
    completed_at: ago(17 * DAY),
    failure_code: null,
  });

  db.audit.push({
    id: id("audit_event", 19 * DAY),
    organization_id: org.id,
    workspace_id: harbor.id,
    environment: "production",
    actor: { type: "user", id: rowan.id, name: rowan.name },
    action: "workspace.created",
    target: { type: "workspace", id: harbor.id, name: harbor.name },
    created_at: ago(19 * DAY),
  });

  seedRuns(db, rng, nowMs, [
    { agent, count: 3, prompts: ["Create a task from this morning's standup", "List the open tasks on the board"] },
  ]);
}

const NAME_A = [
  "Amber",
  "Arbor",
  "Atlas",
  "Beacon",
  "Birch",
  "Bluefin",
  "Bramble",
  "Canyon",
  "Cedar",
  "Cobalt",
  "Copper",
  "Crescent",
  "Delta",
  "Ember",
  "Fable",
  "Fern",
  "Fjord",
  "Granite",
  "Harbor",
  "Hazel",
  "Indigo",
  "Juniper",
  "Kestrel",
  "Lark",
  "Linden",
  "Lumen",
  "Maple",
  "Meridian",
  "Moss",
  "Nimbus",
  "Northgate",
  "Oak",
  "Onyx",
  "Orchid",
  "Pine",
  "Prairie",
  "Quarry",
  "Raven",
  "Ridge",
  "Saffron",
  "Sable",
  "Sierra",
  "Slate",
  "Sparrow",
  "Summit",
  "Tidal",
  "Topaz",
  "Umber",
  "Vale",
  "Willow",
];
const NAME_B = [
  "Analytics",
  "Bakery",
  "Builders",
  "Clinic",
  "Collective",
  "Consulting",
  "Dental",
  "Design",
  "Foods",
  "Freight",
  "Health",
  "Hotels",
  "Insurance",
  "Labs",
  "Law",
  "Learning",
  "Logistics",
  "Media",
  "Outfitters",
  "Partners",
  "Realty",
  "Robotics",
  "Studio",
  "Supply",
  "Travel",
  "Ventures",
  "Works",
];

/**
 * Many quiet tenant workspaces, the shape of a real customer base: most
 * Production, some with a Sandbox twin. They make search and paging honest.
 */
function tenantWorkspaces(rng: Rng, organizationId: OrganizationId, nowMs: number): WorkspaceRow[] {
  const rows: WorkspaceRow[] = [];
  const used = new Set<string>();
  for (let i = 0; i < 1180; i++) {
    const name = `${NAME_A[Math.floor(rng() * NAME_A.length)]} ${NAME_B[Math.floor(rng() * NAME_B.length)]}`;
    let slug = name.toLowerCase().replace(/\s+/g, "-");
    for (let n = 2; used.has(slug); n++) slug = `${name.toLowerCase().replace(/\s+/g, "-")}-${n}`;
    used.add(slug);
    const ref = `tenant_${uuid(rng).slice(0, 6)}`;
    const createdMs = nowMs - Math.floor(rng() * 300 * DAY) - DAY;
    const production: WorkspaceRow = {
      id: newId("workspace", { rng, at: createdMs }),
      organization_id: organizationId,
      name,
      slug,
      environment: "production",
      external_id: ref,
      created_at: new Date(createdMs).toISOString(),
    };
    rows.push(production);
    if (rng() < 0.18) {
      rows.push({
        ...production,
        id: newId("workspace", { rng, at: createdMs - 2 * DAY }),
        slug: `${slug}-sandbox`,
        environment: "sandbox",
        created_at: new Date(createdMs - 2 * DAY).toISOString(),
      });
      used.add(`${slug}-sandbox`);
    }
  }
  return rows;
}

const PUBLISHING_PROMPTS = [
  "Draft a post announcing the autumn release",
  "Summarize the Q3 launch plan into a blog outline",
  "Create a draft about our new onboarding checklist",
  "Publish the autumn release notes",
  "What's in the brand voice guide about headlines?",
  "Turn the launch review notes into a customer-facing post",
];
const FINANCE_PROMPTS = [
  "Weekly revenue summary for the finance channel",
  "Which invoices are overdue this month?",
  "Email the board pack summary to finance",
  "Compare open invoices with last week",
];
const TRIAGE_PROMPTS = [
  "Invoices page shows the wrong currency for EU customers",
  "Login loop after password reset on Safari",
  "Open an issue for the flaky billing export",
  "Why does the webhook retry twice?",
];
const OPS_PROMPTS = [
  "What's on the calendar next week?",
  "Find the vendor contract renewal doc",
  "Send the offsite agenda to the team",
  "Summarize the hiring plan doc",
];
const PM_PROMPTS = [
  "Turn these meeting notes into tasks",
  "Create a task for the onboarding copy changes",
  "What's open on the board?",
];

interface RunSeed {
  agent: AgentRow;
  count: number;
  prompts: string[];
  waiting?: boolean;
}

/** The organization's active API key for the run's environment: approvals arrive from its backend, never the Console. */
function approvalKey(db: Db, run: RunRow): ApiKeyId {
  const workspace = db.workspaces.find((w) => w.organization_id === run.organization_id && w.id === run.workspace_id);
  const key = db.api_keys.find(
    (k) => k.organization_id === run.organization_id && k.environment === workspace?.environment && !k.revoked_at,
  );
  if (!key) throw new Error("The seed needs an active API key in every environment that has approvals.");
  return key.id;
}

function seedRuns(db: Db, rng: Rng, nowMs: number, specs: RunSeed[]) {
  for (const spec of specs) {
    for (let i = 0; i < spec.count; i++) {
      // Skew toward recent days so the activity chart has a shape.
      const back = Math.floor(rng() ** 1.6 * 13.5 * DAY) + 6 * MINUTE + i * 1000;
      const leaveWaiting = spec.waiting === true && i === 0;
      const createdMs = leaveWaiting ? nowMs - 9 * MINUTE : nowMs - back;
      const run: RunRow = {
        id: newId("run", { rng, at: createdMs }),
        organization_id: spec.agent.organization_id,
        workspace_id: spec.agent.workspace_id,
        agent_id: spec.agent.id,
        session_id: newId("session", { rng, at: createdMs }),
        trigger: rng() < 0.9 ? "api" : "console",
        status: "queued",
        input: spec.prompts[Math.floor(rng() * spec.prompts.length)] ?? "Hello",
        output: null,
        usage: { input_tokens: 0, output_tokens: 0, tool_calls: 0 },
        error: null,
        created_at: new Date(createdMs).toISOString(),
        started_at: null,
        finished_at: null,
        plan: [],
      };
      if (leaveWaiting) run.input = "Publish the autumn release notes";
      // Plan from the seed's randomness, not the run's ID: IDs carry the clock, and the demo must not change with it.
      // Pending examples skip simulated model failure, while still consuming
      // seed randomness so every planned call keeps its own ID.
      run.plan = planRun(db, run, leaveWaiting ? () => 0.05 + rng() * 0.95 : rng);
      db.runs.push(run);
      enqueueRun(db, run);

      let clock = Date.parse(run.created_at) + 400 + Math.floor(rng() * 900);
      for (let guard = 0; guard < 40; guard++) {
        const outcome = advanceRun(db, run, new Date(clock).toISOString(), rng);
        if (outcome === "finished") break;
        if (outcome === "paused") {
          if (leaveWaiting) break;
          clock += 20_000 + Math.floor(rng() * 200_000);
          const approval = [...timelineOf(db, run)].reverse().find((e) => e.type === "approval.requested");
          if (approval?.type !== "approval.requested") break;
          resolveApproval(
            db,
            run,
            approval.data.call_id,
            rng() < 0.85 ? "approved" : "denied",
            approvalKey(db, run),
            new Date(clock).toISOString(),
          );
        }
        clock += 300 + Math.floor(rng() * 2200);
      }
    }
  }
  db.runs.sort((a, b) => b.created_at.localeCompare(a.created_at));
}

function seedConnectSessions(
  db: Db,
  rng: Rng,
  nowMs: number,
  specs: { ws: WorkspaceRow; installation: InstallationRow; app: AppRow }[],
) {
  const returnUrl = (env: Environment) =>
    env === "production"
      ? "https://app.northwind.example/settings/integrations"
      : "https://staging.northwind.example/settings/integrations";
  for (const spec of specs) {
    const created = Date.parse(spec.installation.created_at) - 2 * MINUTE;
    const row: ConnectSessionRow = {
      id: newId("connect_session", { rng, at: created }),
      organization_id: spec.ws.organization_id,
      workspace_id: spec.ws.id,
      reference_hash: null,
      environment: spec.ws.environment,
      app_id: spec.app.id,
      configuration_version: spec.installation.configuration_version,
      intent: "create",
      installation_id: spec.installation.id,
      label: spec.installation.label,
      status: "completed",
      return_url: returnUrl(spec.ws.environment),
      requested_scopes: spec.installation.connection?.granted_scopes ?? [],
      created_at: new Date(created).toISOString(),
      expires_at: new Date(created + 15 * MINUTE).toISOString(),
      completed_at: new Date(created + 70_000).toISOString(),
      failure_code: null,
    };
    db.connect_sessions.push(row);
    if (rng() < 0.7) {
      const abandoned = nowMs - Math.floor(rng() * 9 * DAY) - HOUR;
      db.connect_sessions.push({
        ...row,
        id: newId("connect_session", { rng, at: abandoned }),
        reference_hash: null,
        installation_id: null,
        status: rng() < 0.5 ? "expired" : "cancelled",
        created_at: new Date(abandoned).toISOString(),
        expires_at: new Date(abandoned + 15 * MINUTE).toISOString(),
        completed_at: null,
      });
    }
  }
  db.connect_sessions.sort((a, b) => b.created_at.localeCompare(a.created_at));
}

function seedAudit(
  db: Db,
  rng: Rng,
  nowMs: number,
  people: { jordan: User; priya: User; sam: User },
  ws: { acme: WorkspaceRow; beta: WorkspaceRow; acmeSandbox: WorkspaceRow; qa: WorkspaceRow },
  refs: {
    google: AppRow;
    taskboard: AppRow;
    stripe: AppRow;
    notion: AppRow;
    publishing: AppRow;
    acmeBilling: InstallationRow;
    acmePublisher: AgentRow;
  },
) {
  const org = db.organizations[0];
  if (!org) return;
  const user = (u: User) => ({ type: "user" as const, id: u.id, name: u.name });
  const keyId = (name: string) => db.api_keys.find((k) => k.organization_id === org.id && k.name === name)?.id ?? null;
  const memberId = (email: string) =>
    db.memberships.find(
      (m) => m.organization_id === org.id && db.users.find((u) => u.id === m.user_id)?.email === email,
    )?.id ?? null;
  const add = (
    agoMs: number,
    actor: AuditEvent["actor"],
    action: string,
    target: AuditEvent["target"],
    w: WorkspaceRow | null = null,
    environment: Environment | null = w?.environment ?? null,
  ) =>
    db.audit.push({
      id: newId("audit_event", { rng, at: nowMs - agoMs }),
      organization_id: org.id,
      workspace_id: w?.id ?? null,
      environment,
      actor,
      action,
      target,
      created_at: new Date(nowMs - agoMs).toISOString(),
    });
  const app = (a: AppRow) => ({ type: "app", id: a.id, name: a.name });
  const wsT = (w: WorkspaceRow) => ({ type: "workspace", id: w.id, name: w.name });

  add(41 * DAY, user(people.jordan), "organization.created", { type: "organization", id: org.id, name: org.name });
  add(40 * DAY, user(people.jordan), "app.created", app(refs.google));
  add(38 * DAY, user(people.jordan), "app_configuration.updated", app(refs.google), null, "sandbox");
  add(36 * DAY, user(people.jordan), "workspace.created", wsT(ws.acmeSandbox), ws.acmeSandbox);
  add(35 * DAY, user(people.jordan), "app_configuration.updated", app(refs.google), null, "production");
  add(34 * DAY, user(people.jordan), "workspace.created", wsT(ws.acme), ws.acme);
  add(
    34 * DAY,
    user(people.jordan),
    "api_key.created",
    { type: "api_key", id: keyId("Northwind API"), name: "Northwind API" },
    null,
    "production",
  );
  add(30 * DAY, user(people.priya), "app.created", app(refs.taskboard));
  add(27 * DAY, user(people.priya), "workspace.created", wsT(ws.beta), ws.beta);
  add(26 * DAY, user(people.priya), "app.created", app(refs.stripe));
  add(22 * DAY, user(people.sam), "app.created", app(refs.publishing), ws.acme);
  add(
    14 * DAY,
    user(people.priya),
    "api_key.revoked",
    { type: "api_key", id: keyId("Old worker key"), name: "Old worker key" },
    null,
    "production",
  );
  add(12 * DAY, user(people.sam), "workspace.created", wsT(ws.qa), ws.qa);
  add(6 * DAY, user(people.priya), "app_configuration.updated", app(refs.taskboard), null, "production");
  add(
    5 * DAY,
    user(people.jordan),
    "agent_grant.updated",
    { type: "agent", id: refs.acmePublisher.id, name: refs.acmePublisher.name },
    ws.acme,
  );
  add(3 * DAY, user(people.jordan), "app.created", app(refs.notion));
  add(2 * DAY, user(people.jordan), "member.invited", {
    type: "member",
    id: memberId("mei@northwind.example"),
    name: "mei@northwind.example",
  });
  add(
    6 * HOUR,
    { type: "system", id: null, name: "Credential service" },
    "installation.reconnect_required",
    { type: "installation", id: refs.acmeBilling.id, name: `${refs.stripe.name} · ${refs.acmeBilling.label}` },
    ws.acme,
  );
  add(
    5 * HOUR,
    { type: "api_key", id: keyId("Northwind API"), name: "Northwind API" },
    "connect_session.failed",
    { type: "installation", id: refs.acmeBilling.id, name: `${refs.stripe.name} · ${refs.acmeBilling.label}` },
    ws.acme,
  );
  db.audit.sort((a, b) => b.created_at.localeCompare(a.created_at));
}
