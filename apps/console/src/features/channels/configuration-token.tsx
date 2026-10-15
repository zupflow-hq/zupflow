import { ZupflowApiError } from "@zupflow/sdk";
import { ExternalLinkIcon } from "lucide-react";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { cn } from "@/lib/utils";

export interface TokenValues {
  configuration_token: string;
  refresh_token: string;
}

export const EMPTY_TOKENS: TokenValues = { configuration_token: "", refresh_token: "" };

/** What's missing before the tokens can be sent, by field: both are required. */
export function missingTokens(values: TokenValues): Partial<Record<keyof TokenValues, string>> {
  const missing: Partial<Record<keyof TokenValues, string>> = {};
  if (!values.configuration_token.trim()) missing.configuration_token = "Paste the configuration token.";
  if (!values.refresh_token.trim()) missing.refresh_token = "Paste the refresh token that came with it.";
  return missing;
}

/**
 * The API's complaints about the tokens, by field. Hosted Connect names them
 * under `values.`; the Console's own call names them directly.
 */
export function tokenFieldErrors(error: unknown): Partial<Record<keyof TokenValues, string>> {
  if (!(error instanceof ZupflowApiError) || !error.fields) return {};
  const errors: Partial<Record<keyof TokenValues, string>> = {};
  for (const [key, message] of Object.entries(error.fields)) {
    const field = key.replace(/^values\./, "");
    if (field === "configuration_token" || field === "refresh_token") errors[field] = message;
  }
  return errors;
}

/**
 * How to make a Slack app configuration token: once per Slack team, by
 * someone allowed to install apps there. The apps Zupflow makes belong to
 * whoever made the token.
 */
export function SlackTokenSteps({ className }: { className?: string }) {
  return (
    <ol className={cn("space-y-2.5 text-sm", className)}>
      <Step n={1}>
        Open{" "}
        <a
          href="https://api.slack.com/apps"
          target="_blank"
          rel="noreferrer"
          className="inline-flex items-center gap-0.5 font-medium text-brand-ink underline-offset-4 hover:underline"
        >
          api.slack.com/apps
          <ExternalLinkIcon className="size-3" />
        </a>{" "}
        signed in to the Slack team, as someone who may install apps there.
      </Step>
      <Step n={2}>
        Under <span className="font-medium">Your App Configuration Tokens</span>, choose{" "}
        <span className="font-medium">Generate Token</span> and pick the team's workspace.
      </Step>
      <Step n={3}>
        Copy the <span className="font-medium">access token</span> and the{" "}
        <span className="font-medium">refresh token</span>, and paste both here.
      </Step>
      <li className="pl-8 text-xs text-muted-foreground">
        Zupflow renews the token before it expires every 12 hours. Each agent's app belongs to whoever made the token,
        so a shared admin account keeps them working when people leave.
      </li>
    </ol>
  );
}

function Step({ n, children }: { n: number; children: React.ReactNode }) {
  return (
    <li className="grid grid-cols-[1.5rem_minmax(0,1fr)] gap-2">
      <span className="grid size-6 place-items-center rounded-full border font-mono text-[11px] text-muted-foreground">
        {n}
      </span>
      <span className="pt-0.5 text-pretty">{children}</span>
    </li>
  );
}

/** The two pasted tokens. Never echoed back: the fields are cleared once sent. */
export function TokenFields({
  values,
  onChange,
  errors,
  idPrefix,
  disabled,
}: {
  values: TokenValues;
  onChange: (values: TokenValues) => void;
  errors: Partial<Record<keyof TokenValues, string>>;
  idPrefix: string;
  disabled?: boolean;
}) {
  const fields: { key: keyof TokenValues; label: string; placeholder: string; help: string }[] = [
    {
      key: "configuration_token",
      label: "Configuration token",
      placeholder: "xoxe.xoxp-…",
      help: "The access token, starting xoxe.xoxp-.",
    },
    {
      key: "refresh_token",
      label: "Refresh token",
      placeholder: "xoxe-…",
      help: "Shown with it, starting xoxe-. It works once, so paste the newest one.",
    },
  ];
  return (
    <div className="space-y-4">
      {fields.map((field) => (
        <div key={field.key} className="space-y-1.5">
          <Label htmlFor={`${idPrefix}-${field.key}`}>{field.label}</Label>
          <Input
            id={`${idPrefix}-${field.key}`}
            type="password"
            className="font-mono text-sm"
            placeholder={field.placeholder}
            autoComplete="off"
            spellCheck={false}
            disabled={disabled}
            aria-invalid={Boolean(errors[field.key])}
            value={values[field.key]}
            onChange={(e) => onChange({ ...values, [field.key]: e.target.value })}
          />
          {errors[field.key] ? (
            <p className="text-sm text-destructive">{errors[field.key]}</p>
          ) : (
            <p className="text-xs text-muted-foreground">{field.help}</p>
          )}
        </div>
      ))}
    </div>
  );
}
