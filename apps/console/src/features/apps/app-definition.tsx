import type { AppDefinition, AuthType } from "@zupflow/sdk";
import { CheckIcon, ExternalLinkIcon, LockIcon, PencilIcon } from "lucide-react";
import { MonoValue } from "@/components/app/misc";
import { Panel, Properties, Property, SectionHeader } from "@/components/app/page";
import { ToolEffectTag } from "@/components/app/status";
import { Button } from "@/components/ui/button";

const AUTH_TYPE_LABELS: Record<AuthType, string> = {
  oauth2: "OAuth",
  mcp_oauth: "MCP sign-in",
  token: "API token",
};

/** A short name for how an app's accounts authenticate. */
export function authTypeLabel(type: AuthType): string {
  return AUTH_TYPE_LABELS[type];
}

export function AuthTypeLabel({ app }: { app: Pick<AppDefinition, "auth"> }) {
  return <>{authTypeLabel(app.auth.type)}</>;
}

/** Tools the app's MCP server exposes. Agents get them only through grants. */
export function ToolsPanel({ app }: { app: AppDefinition }) {
  return (
    <section>
      <SectionHeader
        title="Tools"
        description="Agents can call a tool only when a grant on a specific installation allows it."
      />
      <Panel>
        <ul className="divide-y">
          {app.tools.map((tool) => (
            <li key={tool.name} className="flex flex-col gap-1 px-4 py-3 sm:flex-row sm:items-center sm:gap-4">
              <div className="min-w-0 flex-1">
                <p className="flex flex-wrap items-center gap-2 text-sm font-medium">
                  {tool.title}
                  <code className="text-xs font-normal text-muted-foreground">{tool.name}</code>
                </p>
                <p className="text-sm text-muted-foreground">{tool.description}</p>
              </div>
              <ToolEffectTag effect={tool.effect} />
            </li>
          ))}
        </ul>
      </Panel>
    </section>
  );
}

/** The declarative authentication template. Never contains secrets. */
export function AuthTemplatePanel({ app, onEdit }: { app: AppDefinition; onEdit?: () => void }) {
  const auth = app.auth;
  return (
    <section>
      <SectionHeader
        title="Authentication"
        actions={
          // Sign-in through the MCP server isn't edited in the Console yet; the API takes it.
          onEdit && auth.type !== "mcp_oauth" ? (
            <Button variant="outline" size="sm" onClick={onEdit}>
              <PencilIcon /> Edit
            </Button>
          ) : null
        }
        description={
          auth.type === "oauth2"
            ? "Defined once. Each environment brings its own OAuth client."
            : auth.type === "mcp_oauth"
              ? "Users sign in through the MCP server. Zupflow finds its endpoints and registers its own client."
              : "What Connect asks for, and where the credential is sent."
        }
      />
      <Panel>
        {auth.type === "mcp_oauth" ? (
          <Properties>
            <Property label="Type">Sign-in through the MCP server</Property>
            <Property label="Endpoints">
              <span className="text-muted-foreground">Found from the server when a user signs in</span>
            </Property>
            <Property label="Client">
              <span className="text-muted-foreground">
                {app.owner_workspace_id
                  ? "Registered by Zupflow for this workspace"
                  : "Registered by Zupflow, once per environment"}
              </span>
            </Property>
            <Property label="Account identity">
              {auth.identity_url ? (
                <MonoValue value={auth.identity_url} />
              ) : (
                <span className="text-muted-foreground">Not verified</span>
              )}
            </Property>
            <Property label="Scopes asked for">
              {auth.scopes.length > 0 ? (
                <span className="tabular">{auth.scopes.length}</span>
              ) : (
                <span className="text-muted-foreground">The server's default</span>
              )}
            </Property>
          </Properties>
        ) : auth.type === "oauth2" ? (
          <Properties>
            <Property label="Type">OAuth 2.0 authorization code</Property>
            <Property label="PKCE">
              {auth.pkce ? (
                <span className="inline-flex items-center gap-1.5">
                  <CheckIcon className="size-4 text-success" /> S256
                </span>
              ) : (
                <span className="text-muted-foreground">Not supported by the provider</span>
              )}
            </Property>
            <Property label="Authorize URL">
              {auth.authorize_url ? (
                <MonoValue value={auth.authorize_url} />
              ) : (
                <span className="text-muted-foreground">Not set</span>
              )}
            </Property>
            <Property label="Token URL">
              {auth.token_url ? (
                <MonoValue value={auth.token_url} />
              ) : (
                <span className="text-muted-foreground">Not set</span>
              )}
            </Property>
            <Property label="Account identity">
              {auth.identity_url ? (
                <MonoValue value={auth.identity_url} />
              ) : (
                <span className="text-muted-foreground">Not verified</span>
              )}
            </Property>
            <Property label="Scopes offered">
              {auth.available_scopes.length > 0 ? (
                <span className="tabular">{auth.available_scopes.length}</span>
              ) : (
                <span className="text-muted-foreground">Provider-defined</span>
              )}
            </Property>
          </Properties>
        ) : (
          <Properties>
            <Property label="Type">API token, entered in Connect</Property>
            <Property label="Inputs">
              <ul className="space-y-1.5">
                {auth.inputs.map((input) => (
                  <li key={input.key} className="flex flex-wrap items-center gap-2">
                    <span>{input.label}</span>
                    <code className="text-xs text-muted-foreground">{input.key}</code>
                    {input.secret ? (
                      <span className="inline-flex items-center gap-1 text-xs text-muted-foreground">
                        <LockIcon className="size-3" /> secret
                      </span>
                    ) : null}
                  </li>
                ))}
              </ul>
            </Property>
            <Property label="Sent as">
              <code className="rounded bg-muted px-1.5 py-0.5 text-[12px]">
                {auth.placement.name}: {auth.placement.prefix}
                {"<token>"}
              </code>
            </Property>
            <Property label="Verified with">
              {auth.verify_url ? (
                <span className="inline-flex items-center gap-2">
                  <span className="font-mono text-xs text-muted-foreground">GET</span>
                  <MonoValue value={auth.verify_url} />
                </span>
              ) : (
                <span className="text-muted-foreground">Not set</span>
              )}
            </Property>
            <Property label="Instructions">
              <p>{auth.instructions}</p>
              {auth.docs_url ? (
                <a
                  href={auth.docs_url}
                  target="_blank"
                  rel="noreferrer"
                  className="mt-1 inline-flex items-center gap-1 text-brand-ink underline-offset-4 hover:underline"
                >
                  Provider docs <ExternalLinkIcon className="size-3" />
                </a>
              ) : null}
            </Property>
          </Properties>
        )}
      </Panel>
    </section>
  );
}
