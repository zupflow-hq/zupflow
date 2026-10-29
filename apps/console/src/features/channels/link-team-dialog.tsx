import { CHANNEL_PROVIDER_NAMES, type ChannelAppManager, type ChannelProvider, type WorkspaceId } from "@zupflow/sdk";
import { useEffect, useState } from "react";
import { toast } from "sonner";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Spinner } from "@/components/ui/spinner";
import { useCreateChannelAppManager, useReplaceChannelAppManagerTokens } from "@/features/queries";
import { errorMessage } from "@/lib/errors";
import {
  EMPTY_TOKENS,
  missingTokens,
  SlackTokenSteps,
  TokenFields,
  type TokenValues,
  tokenFieldErrors,
} from "./configuration-token";
import { CHANNEL_TEAM_TERM } from "./labels";

/**
 * Links a team of `provider` (in Slack, a workspace) to this Zupflow
 * workspace by pasting a configuration token, or, given `manager`, replaces
 * a linked one's tokens. Sandbox only: in Production the customer's product
 * links teams. Offered only for a provider the server reaches.
 */
export function LinkTeamDialog({
  workspaceId,
  provider,
  manager,
  open,
  onOpenChange,
}: {
  workspaceId: WorkspaceId;
  /** The provider of the team to link, or of `manager`. */
  provider: ChannelProvider;
  /** The linked team whose tokens are replaced; none to link a new one. */
  manager?: ChannelAppManager;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const create = useCreateChannelAppManager(workspaceId);
  const replace = useReplaceChannelAppManagerTokens(workspaceId);
  const mutation = manager ? replace : create;
  const [values, setValues] = useState<TokenValues>(EMPTY_TOKENS);
  const [missing, setMissing] = useState<Partial<Record<keyof TokenValues, string>>>({});
  const { reset: resetCreate } = create;
  const { reset: resetReplace } = replace;

  // Closing forgets what was pasted, and the mutation with it.
  useEffect(() => {
    if (!open) {
      setValues(EMPTY_TOKENS);
      setMissing({});
      resetCreate();
      resetReplace();
    }
  }, [open, resetCreate, resetReplace]);

  const serverErrors = tokenFieldErrors(mutation.error);
  const errors = { ...serverErrors, ...missing };
  const name = CHANNEL_PROVIDER_NAMES[provider];
  const teamName = manager?.team.name ?? `this ${name} team`;

  const submit = () => {
    const absent = missingTokens(values);
    setMissing(absent);
    if (Object.keys(absent).length > 0) return;
    const tokens = { configuration_token: values.configuration_token, refresh_token: values.refresh_token };
    const done = {
      onSuccess: (linked: ChannelAppManager) => {
        setValues(EMPTY_TOKENS);
        toast.success(manager ? `Replaced the tokens for ${teamName}` : `Linked ${linked.team.name ?? name}`);
        onOpenChange(false);
      },
    };
    if (manager) replace.mutate({ managerId: manager.id, ...tokens }, done);
    else create.mutate({ provider, method: "configuration_token", ...tokens }, done);
  };

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>{manager ? `Replace the tokens for ${teamName}` : `Link a ${name} team`}</DialogTitle>
          <DialogDescription>
            {manager
              ? "Paste a new configuration token and its refresh token. Installed apps keep answering meanwhile."
              : `Zupflow uses a configuration token to create each agent's own ${name} app in the team${CHANNEL_TEAM_TERM[provider] ? ` (${CHANNEL_TEAM_TERM[provider]})` : ""}. You link a team once; adding an agent then takes one Allow in ${name}.`}
          </DialogDescription>
        </DialogHeader>
        <form
          id="link-team"
          autoComplete="off"
          className="space-y-5"
          onSubmit={(e) => {
            e.preventDefault();
            submit();
          }}
        >
          {provider === "slack" ? <SlackTokenSteps className="rounded-lg border bg-muted/30 p-3" /> : null}
          <TokenFields
            idPrefix="link-team"
            values={values}
            onChange={(next) => {
              setValues(next);
              setMissing({});
            }}
            errors={errors}
            disabled={mutation.isPending}
          />
          {mutation.isError && Object.keys(serverErrors).length === 0 ? (
            <p role="alert" className="rounded-lg border border-destructive/30 bg-danger-soft/50 p-3 text-sm">
              {errorMessage(mutation.error)}
            </p>
          ) : null}
        </form>
        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            Cancel
          </Button>
          <Button type="submit" form="link-team" disabled={mutation.isPending}>
            {mutation.isPending ? <Spinner /> : null}
            {manager ? "Replace tokens" : "Link team"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
