import type { AppDefinition } from "@zupflow/sdk";
import { CheckIcon, ExternalLinkIcon, LockIcon } from "lucide-react";
import { MonoValue } from "@/components/app/misc";
import { Panel, Properties, Property, SectionHeader } from "@/components/app/page";
import { ToolEffectTag } from "@/components/app/status";

export function AuthTypeLabel({ app }: { app: Pick<AppDefinition, "auth"> }) {
  return <>{app.auth.type === "oauth2" ? "OAuth" : "API token"}</>;
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
export function AuthTemplatePanel({ app }: { app: AppDefinition }) {
  const auth = app.auth;
  return (
    <section>
      <SectionHeader
        title="Authentication"
        description={
          auth.type === "oauth2"
            ? "Defined once. Each environment brings its own OAuth client."
            : "What Connect asks for, and where the credential is sent."
        }
      />
      <Panel>
        {auth.type === "oauth2" ? (
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
