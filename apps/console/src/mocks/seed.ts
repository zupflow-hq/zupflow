/**
 * Demo data: Northwind, a fictional product that embeds Zupflow. Acme installs
 * Google Workspace twice ("Marketing" and "Finance") and owns a private
 * publishing app; Beta Labs connects its own Google account; each has Sandbox
 * counterparts. Everything is relative to the moment the seed runs.
 */
import type {
  AccountConnection,
  AgentGrant,
  ApiKeyId,
  AuditEvent,
  Environment,
  IdKind,
  OrganizationId,
  User,
  UserId,
  WebhookEndpoint,
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
  InstallationRow,
  RunRow,
  WorkspaceRow,
} from "./model";
import { mulberry32, type Rng, uuid } from "./random";
import { advanceRun, enqueueRun, planRun, resolveApproval } from "./runner";

export const DB_SCHEMA = 10;
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
  ];
  const [jordan, priya, sam, mei] = users as [User, User, User, User];

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
      joined_at: null,
      last_active_at: null,
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
      app_id: app.id,
      organization_id: org.id,
      workspace_id: workspaceId,
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

  const linear = addApp(CATALOG.linear, null, 30 * DAY);
  configure(linear, "sandbox", "ready", null, priya, 29 * DAY);
  configure(linear, "production", "incomplete", null, priya, 6 * DAY);

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
          granted_scopes: configuration?.oauth_scopes ?? [],
          credential_version: 1 + Math.floor(rng() * 3),
          connected_at: ago(createdAgo),
          expires_at: app.auth.type === "oauth2" ? ago(-50 * MINUTE) : null,
        }
      : null;
    const row: InstallationRow = {
      id: id("installation", nowMs - createdAgo),
      workspace_id: ws.id,
      organization_id: org.id,
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
  const sandboxLinear = install(
    acmeSandbox,
    linear,
    "Product",
    { name: "Acme Product (test)", email: "pm-test@acme.example" },
    28 * DAY,
  );
  const qaEcho = install(qa, echo, "Default", { name: "echo", email: null }, 11 * DAY);
  install(qa, github, "Old bot", null, 9 * DAY, "disconnected");

  // --- Agents and grants ------------------------------------------------------

  const agents: AgentRow[] = [];
  const grants: AgentGrant[] = [];
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
    "Turns meeting notes into Linear issues.",
    "Turn notes into well-scoped Linear issues. Ask before creating more than three.",
    "claude-sonnet-5-5",
    27 * DAY,
  );
  grant(sandboxPm, sandboxLinear, ["search_issues", "create_issue"]);

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
        prefix: "zf_live_2vRs",
        last4: "Ty8n",
        created_by_user_id: priya.id,
        created_at: ago(33 * DAY),
        last_used_at: ago(16 * DAY),
        revoked_at: ago(14 * DAY),
      },
    ],
    webhooks: [] as WebhookEndpoint[],
    audit: [] as Db["audit"],
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
    { agent: echoBot, count: 12, prompts: ["ping", "hello from QA", "approval check", "write test"] },
  ]);

  // Billing broke a few hours ago; earlier runs used it while it worked.
  acmeBilling.status = "reconnect_required";
  acmeBilling.updated_at = ago(6 * HOUR);

  seedConnectSessions(db, rng, nowMs, [
    { ws: acme, installation: acmeMarketing, app: google },
    { ws: acme, installation: acmeEng, app: github },
    { ws: beta, installation: betaGoogle, app: google },
    { ws: acmeSandbox, installation: sandboxLinear, app: linear },
  ]);
  db.connect_sessions.push({
    id: id("connect_session", nowMs - 5 * HOUR),
    reference_hash: null,
    workspace_id: acme.id,
    organization_id: org.id,
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
      linear,
      stripe,
      notion,
      publishing,
      acmeBilling,
      acmePublisher,
    },
  );

  return db;
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
  "Turn these meeting notes into issues",
  "Create an issue for the onboarding copy changes",
  "What's in progress for the PRD team?",
];

interface RunSeed {
  agent: AgentRow;
  count: number;
  prompts: string[];
  waiting?: boolean;
}

/** The organization's active API key for the run's environment: approvals arrive from its backend, never the Console. */
function approvalKey(db: Db, run: RunRow): ApiKeyId {
  const workspace = db.workspaces.find((w) => w.id === run.workspace_id);
  const key = db.api_keys.find(
    (k) =>
      k.organization_id === workspace?.organization_id && k.environment === workspace?.environment && !k.revoked_at,
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
      run.plan = planRun(db, run, rng);
      db.runs.push(run);
      enqueueRun(db, run);

      let clock = Date.parse(run.created_at) + 400 + Math.floor(rng() * 900);
      for (let guard = 0; guard < 40; guard++) {
        const outcome = advanceRun(db, run, new Date(clock).toISOString(), rng);
        if (outcome === "finished") break;
        if (outcome === "paused") {
          if (leaveWaiting) break;
          clock += 20_000 + Math.floor(rng() * 200_000);
          const approval = [...(db.run_events[run.id] ?? [])].reverse().find((e) => e.type === "approval.requested");
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
      reference_hash: null,
      workspace_id: spec.ws.id,
      organization_id: spec.ws.organization_id,
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
    linear: AppRow;
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
  add(30 * DAY, user(people.priya), "app.created", app(refs.linear));
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
  add(6 * DAY, user(people.priya), "app_configuration.updated", app(refs.linear), null, "production");
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
