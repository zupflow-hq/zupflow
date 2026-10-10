/**
 * App definitions used by the seed, plus deterministic sample arguments and
 * results for their tools so simulated runs read like real ones.
 */
import type { AuthTemplate, ToolDefinition } from "@zupflow/sdk";

export interface CatalogEntry {
  slug: string;
  name: string;
  description: string;
  icon: { glyph: string; color: string };
  auth: AuthTemplate;
  tools: ToolDefinition[];
  mcp_server_url: string;
}

const googleWorkspace: CatalogEntry = {
  slug: "google-workspace",
  name: "Google Workspace",
  description: "Drive, Docs, Gmail and Calendar for the connected Google account.",
  icon: { glyph: "G", color: "#3B6FD8" },
  mcp_server_url: "https://mcp.northwind.example/google-workspace",
  auth: {
    type: "oauth2",
    authorize_url: "https://accounts.google.com/o/oauth2/v2/auth",
    token_url: "https://oauth2.googleapis.com/token",
    pkce: true,
    identity_url: "https://openidconnect.googleapis.com/v1/userinfo",
    available_scopes: [
      { name: "openid", description: "Identify the connected account" },
      { name: "email", description: "Read the account's email address" },
      {
        name: "https://www.googleapis.com/auth/drive.file",
        description: "Files the app creates or the user opens with it",
      },
      {
        name: "https://www.googleapis.com/auth/drive.readonly",
        description: "Read all Drive files",
      },
      { name: "https://www.googleapis.com/auth/gmail.send", description: "Send email" },
      {
        name: "https://www.googleapis.com/auth/calendar.events",
        description: "Read and edit calendar events",
      },
    ],
  },
  tools: [
    {
      name: "search_files",
      title: "Search files",
      description: "Find Drive files by name or content.",
      effect: "read",
    },
    {
      name: "read_document",
      title: "Read document",
      description: "Return the text of a Google Doc.",
      effect: "read",
    },
    {
      name: "create_document",
      title: "Create document",
      description: "Create a Google Doc in a chosen folder.",
      effect: "write",
    },
    {
      name: "list_events",
      title: "List calendar events",
      description: "List events in a time range.",
      effect: "read",
    },
    {
      name: "send_email",
      title: "Send email",
      description: "Send an email from the connected account.",
      effect: "write",
    },
    {
      name: "delete_file",
      title: "Delete file",
      description: "Move a Drive file to the trash.",
      effect: "destructive",
    },
  ],
};

const linear: CatalogEntry = {
  slug: "linear",
  name: "Linear",
  description: "Issues, projects and cycles in the connected Linear workspace.",
  icon: { glyph: "L", color: "#5E6AD2" },
  mcp_server_url: "https://mcp.linear.app/mcp",
  auth: {
    type: "oauth2",
    authorize_url: "https://linear.app/oauth/authorize",
    token_url: "https://api.linear.app/oauth/token",
    pkce: true,
    identity_url: "https://api.linear.app/graphql",
    available_scopes: [
      { name: "read", description: "Read issues, projects and teams" },
      { name: "issues:create", description: "Create issues" },
      { name: "comments:create", description: "Comment on issues" },
    ],
  },
  tools: [
    {
      name: "search_issues",
      title: "Search issues",
      description: "Find issues by text, team or state.",
      effect: "read",
    },
    {
      name: "create_issue",
      title: "Create issue",
      description: "Open an issue in a team.",
      effect: "write",
    },
    {
      name: "add_comment",
      title: "Comment on issue",
      description: "Add a comment to an issue.",
      effect: "write",
    },
  ],
};

const notion: CatalogEntry = {
  slug: "notion",
  name: "Notion",
  description: "Pages and databases shared with the integration.",
  icon: { glyph: "N", color: "#2F3437" },
  mcp_server_url: "https://mcp.notion.com/mcp",
  auth: {
    type: "oauth2",
    authorize_url: "https://api.notion.com/v1/oauth/authorize",
    token_url: "https://api.notion.com/v1/oauth/token",
    pkce: false,
    identity_url: "https://api.notion.com/v1/users/me",
    available_scopes: [],
  },
  tools: [
    {
      name: "search_pages",
      title: "Search pages",
      description: "Find pages shared with the integration.",
      effect: "read",
    },
    {
      name: "append_blocks",
      title: "Append to page",
      description: "Add content to the end of a page.",
      effect: "write",
    },
  ],
};

const stripe: CatalogEntry = {
  slug: "stripe",
  name: "Stripe",
  description: "Read customers, invoices and subscriptions with a restricted key.",
  icon: { glyph: "S", color: "#635BFF" },
  mcp_server_url: "https://mcp.stripe.com",
  auth: {
    type: "token",
    inputs: [
      {
        key: "api_key",
        label: "Restricted API key",
        secret: true,
        help: "Create a restricted key with read access to Customers, Invoices and Subscriptions.",
        placeholder: "rk_live_…",
        pattern: "^rk_(test|live)_[A-Za-z0-9]{8,}$",
      },
    ],
    placement: { location: "header", name: "Authorization", prefix: "Bearer " },
    verify_url: "https://api.stripe.com/v1/account",
    instructions:
      "In the Stripe Dashboard, open Developers → API keys and create a restricted key. Zupflow only uses it for the tools you grant.",
    docs_url: "https://docs.stripe.com/keys#limit-access",
  },
  tools: [
    {
      name: "list_invoices",
      title: "List invoices",
      description: "List invoices for a customer or period.",
      effect: "read",
    },
    {
      name: "get_customer",
      title: "Get customer",
      description: "Return a customer and their subscriptions.",
      effect: "read",
    },
    {
      name: "refund_payment",
      title: "Refund payment",
      description: "Refund a payment in full or in part.",
      effect: "destructive",
    },
  ],
};

const github: CatalogEntry = {
  slug: "github",
  name: "GitHub",
  description: "Repositories, issues and pull requests the account can reach.",
  icon: { glyph: "GH", color: "#24292F" },
  mcp_server_url: "https://api.githubcopilot.com/mcp/",
  auth: {
    type: "oauth2",
    authorize_url: "https://github.com/login/oauth/authorize",
    token_url: "https://github.com/login/oauth/access_token",
    pkce: true,
    identity_url: "https://api.github.com/user",
    available_scopes: [
      { name: "repo", description: "Full access to private repositories" },
      { name: "read:org", description: "Read organization membership" },
    ],
  },
  tools: [
    {
      name: "search_code",
      title: "Search code",
      description: "Search code across reachable repositories.",
      effect: "read",
    },
    {
      name: "list_pull_requests",
      title: "List pull requests",
      description: "List open pull requests in a repository.",
      effect: "read",
    },
    {
      name: "create_issue",
      title: "Create issue",
      description: "Open an issue in a repository.",
      effect: "write",
    },
  ],
};

const acmePublishing: CatalogEntry = {
  slug: "acme-publishing",
  name: "Acme Publishing",
  description: "Acme's own CMS. Drafts, schedules and publishes blog posts.",
  icon: { glyph: "AP", color: "#C2410C" },
  mcp_server_url: "https://cms.acme.example/mcp",
  auth: {
    type: "token",
    inputs: [
      {
        key: "token",
        label: "CMS token",
        secret: true,
        help: "Settings → Integrations → Personal tokens in the Acme CMS.",
        placeholder: "acms_…",
        pattern: null,
      },
    ],
    placement: { location: "header", name: "Authorization", prefix: "Bearer " },
    verify_url: "https://cms.acme.example/api/me",
    instructions: "Paste a token from the CMS account the agent should publish as.",
    docs_url: null,
  },
  tools: [
    {
      name: "list_drafts",
      title: "List drafts",
      description: "List unpublished posts.",
      effect: "read",
    },
    {
      name: "create_draft",
      title: "Create draft",
      description: "Create a post draft from Markdown.",
      effect: "write",
    },
    {
      name: "publish_post",
      title: "Publish post",
      description: "Publish a draft to the public blog.",
      effect: "destructive",
    },
  ],
};

/** Signs in through its MCP server, with no identity URL: its accounts can't be checked on reconnect. */
const fieldnote: CatalogEntry = {
  slug: "fieldnote",
  name: "Fieldnote",
  description: "Shared notes and field reports. People sign in through its MCP server.",
  icon: { glyph: "Fn", color: "#4D7C0F" },
  mcp_server_url: "https://mcp.fieldnote.example/mcp",
  auth: {
    type: "mcp_oauth",
    scopes: [
      { name: "notes:read", description: "Read notes and reports" },
      { name: "notes:write", description: "Create and edit notes" },
    ],
    identity_url: null,
  },
  tools: [
    { name: "search_notes", title: "Search notes", description: "Find notes by text or tag.", effect: "read" },
    { name: "create_note", title: "Create note", description: "Add a note to a notebook.", effect: "write" },
  ],
};

const echoServer: CatalogEntry = {
  slug: "echo-server",
  name: "Echo test server",
  description: "Returns its input. For exercising grants and approvals in Sandbox.",
  icon: { glyph: "E", color: "#0E7490" },
  mcp_server_url: "https://echo.zupflow.dev/mcp",
  auth: {
    type: "token",
    inputs: [
      {
        key: "token",
        label: "Token",
        secret: true,
        help: "Any value. The echo server accepts every token except `invalid`.",
        placeholder: "test-token",
        pattern: null,
      },
    ],
    placement: { location: "header", name: "Authorization", prefix: "Bearer " },
    verify_url: "https://echo.zupflow.dev/me",
    instructions: "Use any token to connect.",
    docs_url: null,
  },
  tools: [
    { name: "echo", title: "Echo", description: "Return the input unchanged.", effect: "read" },
    {
      name: "echo_write",
      title: "Echo as write",
      description: "Return the input, marked as a write.",
      effect: "write",
    },
  ],
};

export const CATALOG = {
  googleWorkspace,
  linear,
  notion,
  stripe,
  github,
  acmePublishing,
  fieldnote,
  echoServer,
} as const;

/** Blank tools for a newly created app; the definition is edited later. */
export function defaultToolsFor(name: string): ToolDefinition[] {
  return [
    {
      name: "ping",
      title: "Ping",
      description: `Check that ${name} is reachable.`,
      effect: "read",
    },
  ];
}

interface Sample {
  arguments: unknown;
  result: unknown;
}

/** Plausible tool traffic. Deterministic for a given input. */
export function sampleToolCall(tool: string, input: string): Sample {
  const topic = input.replace(/[.?!]+$/, "").slice(0, 60);
  switch (tool) {
    case "search_files":
      return {
        arguments: { query: topic, limit: 5 },
        result: {
          files: [
            { id: "1Fz…aQ", name: "Q3 launch plan", mime_type: "application/vnd.google-apps.document" },
            { id: "1Kd…xR", name: "Brand voice guide", mime_type: "application/vnd.google-apps.document" },
          ],
        },
      };
    case "read_document":
      return {
        arguments: { file_id: "1Fz…aQ" },
        result: { title: "Q3 launch plan", characters: 8412, truncated: false },
      };
    case "create_document":
      return {
        arguments: { title: `Draft: ${topic}`, folder_id: "0AB…root" },
        result: { file_id: "1Np…tW", url: "https://docs.google.com/document/d/1Np…tW" },
      };
    case "list_events":
      return {
        arguments: { time_min: "2026-10-05T00:00:00Z", time_max: "2026-10-12T00:00:00Z" },
        result: { events: [{ summary: "Launch review", start: "2026-10-07T15:00:00Z" }] },
      };
    case "send_email":
      return {
        arguments: { to: ["team@acme.example"], subject: topic },
        result: { message_id: "18c2…f1" },
      };
    case "list_invoices":
      return {
        arguments: { status: "open", limit: 10 },
        result: { invoices: [{ id: "in_1Q…", amount_due: 420000, currency: "usd" }], has_more: false },
      };
    case "get_customer":
      return {
        arguments: { customer: "cus_Q…" },
        result: { id: "cus_Q…", subscriptions: 2, delinquent: false },
      };
    case "search_issues":
      return {
        arguments: { query: topic, state: "started" },
        result: { issues: [{ identifier: "PRD-412", title: "Onboarding checklist polish" }] },
      };
    case "create_issue":
      return {
        arguments: { title: topic, team: "PRD" },
        result: { identifier: "PRD-431", url: "https://linear.app/acme/issue/PRD-431" },
      };
    case "list_drafts":
      return { arguments: {}, result: { drafts: [{ id: "post_88", title: "Autumn release notes" }] } };
    case "create_draft":
      return {
        arguments: { title: topic, body_markdown: "# …" },
        result: { id: "post_91", status: "draft" },
      };
    case "publish_post":
      return {
        arguments: { id: "post_91" },
        result: { id: "post_91", status: "published", url: "https://acme.example/blog/autumn" },
      };
    case "search_code":
      return {
        arguments: { query: topic },
        result: { total_count: 3, items: [{ path: "src/billing/invoice.ts" }] },
      };
    case "list_pull_requests":
      return { arguments: { repo: "acme/web", state: "open" }, result: { pull_requests: 4 } };
    case "echo":
    case "echo_write":
      return { arguments: { text: input }, result: { text: input } };
    default:
      return { arguments: { input: topic }, result: { ok: true } };
  }
}
