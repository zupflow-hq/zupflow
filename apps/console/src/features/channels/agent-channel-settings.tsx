import {
  type AgentChannel,
  type AgentMessages,
  CHANNEL_PROVIDER_NAMES,
  CHANNEL_TOOL_NAMES,
  CHANNEL_TOOLS,
  type ChannelAudience,
  type ChannelConnection,
  type ChannelConversationId,
  type ChannelSenders,
  type MidRun,
  type Organization,
  type PutAgentChannelInput,
  type ThreadFollowUp,
  type Workspace,
  ZupflowApiError,
} from "@zupflow/sdk";
import { AlertTriangleIcon, CheckIcon, PlusIcon, XIcon } from "lucide-react";
import { useEffect, useId, useMemo, useState } from "react";
import { toast } from "sonner";
import { type Choice, ChoiceCards } from "@/components/app/choice-cards";
import { QueryError, RowsSkeleton } from "@/components/app/misc";
import { StatusPill, ToolEffectTag } from "@/components/app/status";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Command, CommandEmpty, CommandGroup, CommandInput, CommandItem, CommandList } from "@/components/ui/command";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "@/components/ui/select";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import { useChannelConversations, useChannelToolLevels, usePutAgentChannel } from "@/features/queries";
import { errorMessage } from "@/lib/errors";
import { shortId } from "@/lib/format";
import { useDebounced } from "@/lib/use-debounced";
import { cn } from "@/lib/utils";
import { CHANNEL_TOOL_COPY, conversationName, SHARED_CONVERSATION } from "./labels";

/** The settings as edited: the sender list as typed, one address per line. */
type Draft = Omit<PutAgentChannelInput, "sender_list" | "persona"> & {
  senderText: string;
  persona: boolean;
  personaName: string;
  personaAvatar: string;
};

function draftOf(channel: AgentChannel): Draft {
  return {
    tools: channel.tools,
    approval_required: channel.approval_required,
    audience: channel.audience,
    audience_conversations: channel.audience_conversations,
    answers_dms: channel.answers_dms,
    answers_mentions: channel.answers_mentions,
    thread_follow_up: channel.thread_follow_up,
    agent_messages: channel.agent_messages,
    max_agent_hops: channel.max_agent_hops,
    mid_run: channel.mid_run,
    senders: channel.senders,
    senderText: channel.sender_list.join("\n"),
    allow_automated: channel.allow_automated,
    allow_external: channel.allow_external,
    reply_with_output: channel.reply_with_output,
    persona: channel.persona !== null,
    personaName: channel.persona?.name ?? "",
    personaAvatar: channel.persona?.avatar_url ?? "",
  };
}

const senderList = (text: string) =>
  text
    .split(/[\n,;]+/)
    .map((s) => s.trim())
    .filter(Boolean);

/** What the API takes: every setting, replacing what was there. */
export function inputOf(draft: Draft): PutAgentChannelInput {
  const { senderText, persona, personaName, personaAvatar, ...rest } = draft;
  return {
    ...rest,
    approval_required: rest.approval_required.filter((t) => rest.tools.includes(t)),
    audience_conversations: rest.audience === "listed" ? rest.audience_conversations : [],
    sender_list: rest.senders === "listed" ? senderList(senderText) : [],
    persona: persona ? { name: personaName.trim(), avatar_url: personaAvatar.trim() || null } : null,
  };
}

const EMAIL = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

/** What's wrong before saving, by the API's field names, in the API's words where it has them. */
function problems(draft: Draft): Record<string, string> {
  const found: Record<string, string> = {};
  if (draft.senders === "listed") {
    const list = senderList(draft.senderText);
    const bad = list.find((s) => !EMAIL.test(s));
    if (list.length === 0) found.sender_list = "Add the email addresses of the people who may talk to the agent.";
    else if (bad) found.sender_list = `${bad} isn't an email address.`;
    else if (list.length > 500) found.sender_list = "List 500 addresses or fewer. To let more people in, use anyone.";
  }
  if (draft.persona && !draft.personaName.trim()) found["persona.name"] = "Name the persona, or turn it off.";
  if (draft.persona && draft.personaAvatar.trim() && !/^https:\/\/\S+$/.test(draft.personaAvatar.trim())) {
    found["persona.avatar_url"] = "Use an https:// URL.";
  }
  return found;
}

/**
 * What an agent does on one of its own connections: the channel tools it may
 * call within the organization's levels, what wakes it, who may talk to it,
 * where it may send and how it answers. Saving replaces every setting.
 */
export function AgentChannelSettings({
  org,
  workspace,
  connection,
  channel,
  readOnly,
}: {
  org: Organization;
  workspace: Workspace;
  connection: ChannelConnection;
  channel: AgentChannel;
  readOnly: boolean;
}) {
  // An agent can have a card per team on one page: every control's ID is this card's own.
  const uid = useId();
  const fid = (name: string) => `${uid}-${name}`;
  const initial = useMemo(() => draftOf(channel), [channel]);
  const [draft, setDraft] = useState<Draft>(initial);
  const [checked, setChecked] = useState(false);
  useEffect(() => setDraft(initial), [initial]);
  const put = usePutAgentChannel(workspace.id, channel.agent_id);
  const levels = useChannelToolLevels(org.id);
  const levelOf = (tool: string) =>
    levels.data?.find((l) => l.provider === connection.provider && l.tool === tool)?.level ?? null;
  const set = <K extends keyof Draft>(key: K, value: Draft[K]) => setDraft((d) => ({ ...d, [key]: value }));
  const dirty =
    JSON.stringify(inputOf(draft)) !== JSON.stringify(inputOf(initial)) || draft.senderText !== initial.senderText;
  const local = checked ? problems(draft) : {};
  const server = put.error instanceof ZupflowApiError ? (put.error.fields ?? {}) : {};
  const errors = { ...server, ...local };
  const blocked = draft.tools.filter((t) => levelOf(t) === "deny");
  const handle = `@${connection.identity.handle}`;

  const toggleTool = (tool: string, on: boolean) => {
    setDraft((d) => ({
      ...d,
      tools: on ? [...d.tools, tool] : d.tools.filter((t) => t !== tool),
      approval_required:
        on && levelOf(tool) === "approve"
          ? [...new Set([...d.approval_required, tool])]
          : on
            ? d.approval_required
            : d.approval_required.filter((t) => t !== tool),
    }));
  };

  const save = () => {
    setChecked(true);
    if (Object.keys(problems(draft)).length > 0) return;
    const input = inputOf(draft);
    // Tools the organization requires approval for always ask first.
    const required = input.tools.filter((t) => levelOf(t) === "approve");
    put.mutate(
      {
        connectionId: connection.id,
        ...input,
        approval_required: [...new Set([...input.approval_required, ...required])],
      },
      {
        onSuccess: () => {
          setChecked(false);
          toast.success("Saved channel settings");
        },
      },
    );
  };

  return (
    <form
      className="divide-y"
      onSubmit={(e) => {
        e.preventDefault();
        save();
      }}
    >
      <fieldset disabled={readOnly} className="contents">
        <Group
          title="Tools"
          description={`What ${handle} may do in ${CHANNEL_PROVIDER_NAMES[connection.provider]}, within your organization's levels.`}
        >
          {levels.isError ? (
            <QueryError
              error={levels.error}
              onRetry={() => levels.refetch()}
              what="your organization's channel tool levels"
            />
          ) : levels.isPending ? (
            <RowsSkeleton rows={3} columns={[45, 15]} />
          ) : (
            <ul className="-mx-4 divide-y border-y sm:mx-0 sm:rounded-lg sm:border">
              {CHANNEL_TOOL_NAMES.map((tool) => {
                const on = draft.tools.includes(tool);
                const level = levelOf(tool);
                const isBlocked = level === "deny";
                const required = level === "approve";
                const id = fid(tool);
                return (
                  <li key={tool} className="flex items-center gap-3 px-4 py-2.5">
                    <Checkbox
                      id={id}
                      checked={on}
                      disabled={readOnly || (isBlocked && !on)}
                      onCheckedChange={(v) => toggleTool(tool, v === true)}
                    />
                    <Label
                      htmlFor={id}
                      className={cn(
                        "min-w-0 flex-1 flex-col items-start gap-0.5 font-normal",
                        !readOnly && "cursor-pointer",
                      )}
                    >
                      <span className="flex min-w-0 flex-wrap items-center gap-2">
                        <span className="font-medium">{CHANNEL_TOOL_COPY[tool].title}</span>
                        <ToolEffectTag effect={CHANNEL_TOOLS[tool].effect} />
                        {isBlocked ? <StatusPill tone="danger">Blocked</StatusPill> : null}
                      </span>
                      <span className="text-xs text-muted-foreground">
                        {CHANNEL_TOOL_COPY[tool].description}
                        {isBlocked
                          ? " Your organization blocks it."
                          : required
                            ? " Your organization requires approval for it."
                            : ""}
                      </span>
                    </Label>
                    <div className={cn("flex shrink-0 items-center gap-2", !on && "opacity-40")}>
                      <Label
                        htmlFor={`${id}-approval`}
                        className="hidden text-xs font-normal text-muted-foreground sm:block"
                      >
                        Ask first
                      </Label>
                      <Switch
                        id={`${id}-approval`}
                        aria-label={`Require approval for ${CHANNEL_TOOL_COPY[tool].title}`}
                        disabled={readOnly || !on || required}
                        checked={draft.approval_required.includes(tool) || (on && required)}
                        onCheckedChange={(next) =>
                          set(
                            "approval_required",
                            next
                              ? [...draft.approval_required, tool]
                              : draft.approval_required.filter((t) => t !== tool),
                          )
                        }
                      />
                    </div>
                  </li>
                );
              })}
            </ul>
          )}
          {blocked.length > 0 ? (
            <p className="mt-3 flex items-start gap-2 text-sm">
              <AlertTriangleIcon className="mt-0.5 size-4 shrink-0 text-destructive" />
              Your organization blocks{" "}
              {blocked.map((t) => CHANNEL_TOOL_COPY[t as keyof typeof CHANNEL_TOOL_COPY]?.title ?? t).join(", ")}. Calls
              are refused; turn {blocked.length === 1 ? "it" : "them"} off to save.
            </p>
          ) : null}
          <FieldError message={errors.tools ?? errors.approval_required} />
          <p className="mt-3 text-xs text-muted-foreground">
            Owners and admins set each tool's level for every agent in organization settings.
          </p>
        </Group>

        <Group title="What wakes it" description="Messages that start a run. Everything else in a channel is ignored.">
          <SwitchRow
            id={fid("answers-dms")}
            label="Answer direct messages"
            description={`People can message ${handle} directly.`}
            checked={draft.answers_dms}
            onChange={(v) => set("answers_dms", v)}
          />
          <SwitchRow
            id={fid("answers-mentions")}
            label="Answer mentions"
            description={`When someone mentions ${handle} in a conversation it's in.`}
            checked={draft.answers_mentions}
            onChange={(v) => set("answers_mentions", v)}
          />
          <SelectRow<ThreadFollowUp>
            id={fid("thread-follow-up")}
            label="Replies in its threads"
            description={
              draft.thread_follow_up === "off"
                ? "Only a new mention wakes it again in a thread."
                : draft.thread_follow_up === "always"
                  ? "Every reply in a thread it answered wakes it, even ones meant for someone else."
                  : "A reply wakes it while it's the agent people last spoke to in that thread."
            }
            value={draft.thread_follow_up}
            onChange={(v) => set("thread_follow_up", v)}
            options={[
              { value: "off", label: "Only when mentioned" },
              { value: "when_last_addressed", label: "When last addressed" },
              { value: "always", label: "Always" },
            ]}
          />
          <SelectRow<AgentMessages>
            id={fid("agent-messages")}
            label="Other agents' messages"
            description={
              draft.agent_messages === "ignore"
                ? "Messages from other agents never wake it."
                : `Another agent mentioning ${handle} wakes it, until a chain of agents answering agents reaches the limit.`
            }
            value={draft.agent_messages}
            onChange={(v) => set("agent_messages", v)}
            options={[
              { value: "ignore", label: "Ignore" },
              { value: "mentions", label: "Answer mentions" },
            ]}
          />
          {draft.agent_messages === "mentions" ? (
            <SelectRow<string>
              id={fid("max-hops")}
              label="Longest chain of agents"
              description="How many agents in a row may answer one another before it stops."
              value={String(draft.max_agent_hops)}
              onChange={(v) => set("max_agent_hops", Number(v))}
              options={[1, 2, 3, 4, 5].map((n) => ({
                value: String(n),
                label: `${n} ${n === 1 ? "agent" : "agents"}`,
              }))}
            />
          ) : null}
          <div className="space-y-2 py-3">
            <p className="text-sm font-medium">Messages that arrive while it works</p>
            <ChoiceCards<MidRun>
              label="Messages that arrive while it works"
              value={draft.mid_run}
              onValueChange={(v) => set("mid_run", v)}
              className="sm:grid-cols-2"
              options={[
                choice(
                  "steer",
                  "Add them to the run",
                  "It reads them before its next step, so its answer accounts for them.",
                  readOnly,
                ),
                choice(
                  "collect",
                  "Answer them afterwards",
                  "They wait until the run finishes, then one follow-up run answers them all.",
                  readOnly,
                ),
              ]}
            />
          </div>
        </Group>

        <Group title="Who may talk to it" description="Checked before anything wakes it.">
          <div className="space-y-2 py-3">
            <ChoiceCards<ChannelSenders>
              label="Who may talk to it"
              value={draft.senders}
              onValueChange={(v) => set("senders", v)}
              className="sm:grid-cols-2"
              options={[
                choice(
                  "anyone",
                  "Anyone in the team",
                  `Every person in the ${CHANNEL_PROVIDER_NAMES[connection.provider]} team who can reach it.`,
                  readOnly,
                ),
                choice(
                  "listed",
                  "Only people you list",
                  `By the email address on their ${CHANNEL_PROVIDER_NAMES[connection.provider]} profile.`,
                  readOnly,
                ),
              ]}
            />
            {draft.senders === "listed" ? (
              <div className="space-y-1.5 pt-1">
                <Label htmlFor={fid("sender-list")}>Email addresses</Label>
                <Textarea
                  id={fid("sender-list")}
                  rows={4}
                  className="font-mono text-sm"
                  placeholder={"ana@example.com\nbruno@example.com"}
                  aria-invalid={Boolean(errors.sender_list)}
                  value={draft.senderText}
                  onChange={(e) => set("senderText", e.target.value)}
                />
                {errors.sender_list ? (
                  <FieldError message={errors.sender_list} />
                ) : (
                  <p className="text-xs text-muted-foreground">One per line, up to 500.</p>
                )}
              </div>
            ) : null}
          </div>
          <SwitchRow
            id={fid("allow-automated")}
            label="Automated accounts"
            description="Let apps and integrations that post on their own wake it. Its own messages never do."
            checked={draft.allow_automated}
            onChange={(v) => set("allow_automated", v)}
          />
          <SwitchRow
            id={fid("allow-external")}
            label={`${SHARED_CONVERSATION[connection.provider]} channels`}
            description="Let people from other organizations in shared channels wake it."
            checked={draft.allow_external}
            onChange={(v) => set("allow_external", v)}
          />
        </Group>

        <Group title="Where it may send" description="Checked before every message it sends, ahead of approvals.">
          <div className="space-y-2 py-3">
            <ChoiceCards<ChannelAudience>
              label="Where it may send"
              value={draft.audience}
              onValueChange={(v) => set("audience", v)}
              className="sm:grid-cols-3"
              options={[
                choice("origin", "Where it was asked", "Only the conversation that woke it.", readOnly),
                choice("listed", "And listed conversations", "That one, and the conversations you pick.", readOnly),
                choice("anywhere", "Anywhere it's a member", "Any conversation its app was added to.", readOnly),
              ]}
            />
            {draft.audience === "listed" ? (
              <ListedConversations
                workspace={workspace}
                connection={connection}
                value={draft.audience_conversations}
                onChange={(v) => set("audience_conversations", v)}
                readOnly={readOnly}
              />
            ) : null}
            <FieldError message={errors.audience_conversations} />
          </div>
        </Group>

        <Group title="How it answers">
          <SwitchRow
            id={fid("reply-with-output")}
            label="Reply with its answer"
            description="Post the run's answer in the thread that woke it. Turn off when it replies with its tools instead. Runs started through the API never post on their own."
            checked={draft.reply_with_output}
            onChange={(v) => set("reply_with_output", v)}
          />
          <SwitchRow
            id={fid("persona")}
            label="Sign as a persona"
            description={`Post under another name and picture instead of ${handle}'s app.`}
            checked={draft.persona}
            onChange={(v) => set("persona", v)}
          />
          {draft.persona ? (
            <div className="grid gap-4 pb-3 sm:grid-cols-2">
              <div className="space-y-1.5">
                <Label htmlFor={fid("persona-name")}>Name</Label>
                <Input
                  id={fid("persona-name")}
                  maxLength={80}
                  value={draft.personaName}
                  aria-invalid={Boolean(errors["persona.name"])}
                  onChange={(e) => set("personaName", e.target.value)}
                />
                <FieldError message={errors["persona.name"]} />
              </div>
              <div className="space-y-1.5">
                <Label htmlFor={fid("persona-avatar")}>Picture URL</Label>
                <Input
                  id={fid("persona-avatar")}
                  type="url"
                  placeholder="https://"
                  value={draft.personaAvatar}
                  aria-invalid={Boolean(errors["persona.avatar_url"])}
                  onChange={(e) => set("personaAvatar", e.target.value)}
                />
                <FieldError message={errors["persona.avatar_url"]} />
              </div>
            </div>
          ) : null}
        </Group>
      </fieldset>

      {put.isError && Object.keys(server).length === 0 ? (
        <p role="alert" className="mx-4 my-3 rounded-lg border border-destructive/30 bg-danger-soft/50 p-3 text-sm">
          {errorMessage(put.error)}
        </p>
      ) : null}
      {readOnly ? (
        <p className="px-4 py-3 text-xs text-muted-foreground">
          You can view these settings. Owners, admins and developers can change them.
        </p>
      ) : (
        <div className="flex flex-wrap items-center gap-2 px-4 py-3">
          <Button type="submit" disabled={!dirty || put.isPending || blocked.length > 0 || !levels.data}>
            {put.isPending ? <Spinner /> : null}
            Save channel settings
          </Button>
          {dirty ? (
            <Button
              type="button"
              variant="ghost"
              onClick={() => {
                setDraft(initial);
                setChecked(false);
                put.reset();
              }}
            >
              Discard
            </Button>
          ) : null}
          <span className="text-xs text-muted-foreground sm:ml-auto">
            Applies to messages that arrive after you save.
          </span>
        </div>
      )}
    </form>
  );
}

function choice<T extends string>(value: T, title: string, body: string, disabled: boolean): Choice<T> {
  return {
    value,
    disabled,
    content: (
      <>
        <span className="text-sm font-medium">{title}</span>
        <span className="text-xs text-muted-foreground">{body}</span>
      </>
    ),
  };
}

function Group({ title, description, children }: { title: string; description?: string; children: React.ReactNode }) {
  return (
    <section className="px-4 py-4">
      <h4 className="text-sm font-semibold">{title}</h4>
      {description ? <p className="mt-0.5 mb-2 text-sm text-muted-foreground">{description}</p> : null}
      <div className="divide-y">{children}</div>
    </section>
  );
}

function FieldError({ message }: { message: string | undefined }) {
  return message ? <p className="mt-1.5 text-sm text-destructive">{message}</p> : null;
}

function SwitchRow({
  id,
  label,
  description,
  checked,
  onChange,
}: {
  id: string;
  label: string;
  description: string;
  checked: boolean;
  onChange: (checked: boolean) => void;
}) {
  return (
    <div className="flex items-start gap-4 py-3">
      <Label htmlFor={id} className="min-w-0 flex-1 flex-col items-start gap-0.5">
        <span className="text-sm font-medium">{label}</span>
        <span className="text-xs font-normal text-muted-foreground">{description}</span>
      </Label>
      <Switch id={id} checked={checked} onCheckedChange={onChange} className="mt-0.5" />
    </div>
  );
}

function SelectRow<T extends string>({
  id,
  label,
  description,
  value,
  onChange,
  options,
}: {
  id: string;
  label: string;
  description: string;
  value: T;
  onChange: (value: T) => void;
  options: { value: T; label: string }[];
}) {
  return (
    <div className="flex flex-col gap-2 py-3 sm:flex-row sm:items-start sm:gap-4">
      <div className="min-w-0 flex-1">
        <Label htmlFor={id} className="text-sm font-medium">
          {label}
        </Label>
        <p className="mt-0.5 text-xs text-muted-foreground">{description}</p>
      </div>
      <Select value={value} onValueChange={(v) => onChange(v as T)}>
        <SelectTrigger id={id} className="w-full bg-card sm:w-52">
          <SelectValue />
        </SelectTrigger>
        <SelectContent>
          {options.map((o) => (
            <SelectItem key={o.value} value={o.value}>
              {o.label}
            </SelectItem>
          ))}
        </SelectContent>
      </Select>
    </div>
  );
}

/**
 * The conversations it may send to besides the one that woke it, picked
 * from this connection's, searched on the server as people type.
 */
function ListedConversations({
  workspace,
  connection,
  value,
  onChange,
  readOnly,
}: {
  workspace: Workspace;
  connection: ChannelConnection;
  value: ChannelConversationId[];
  onChange: (value: ChannelConversationId[]) => void;
  readOnly: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const q = useDebounced(query.trim());
  const results = useChannelConversations(workspace.id, connection.id, { q: q || undefined, limit: 20 }, open);
  // The most recent ones name what's already listed without loading every conversation.
  const recent = useChannelConversations(workspace.id, connection.id, { limit: 50 }, value.length > 0);
  const [names, setNames] = useState<Record<string, string>>({});
  const known = useMemo(() => {
    const map: Record<string, string> = { ...names };
    for (const page of [...(recent.data?.pages ?? []), ...(results.data?.pages ?? [])]) {
      for (const c of page.items) map[c.id] = conversationName(c);
    }
    return map;
  }, [names, recent.data, results.data]);
  const items = results.data?.pages[0]?.items ?? [];
  const total = results.data?.pages[0]?.pagination.total ?? null;

  return (
    <div className="space-y-2 pt-1">
      {value.length > 0 ? (
        <ul className="flex flex-wrap gap-1.5" aria-label="Listed conversations">
          {value.map((id) => (
            <li key={id} className="inline-flex h-7 items-center gap-1 rounded-md border bg-muted/40 pr-1 pl-2 text-sm">
              {known[id] ?? <span className="font-mono text-xs">{shortId(id)}</span>}
              {readOnly ? null : (
                <Button
                  type="button"
                  variant="ghost"
                  size="icon-xs"
                  aria-label={`Remove ${known[id] ?? id}`}
                  onClick={() => onChange(value.filter((v) => v !== id))}
                >
                  <XIcon />
                </Button>
              )}
            </li>
          ))}
        </ul>
      ) : (
        <p className="text-sm text-muted-foreground">No conversations listed yet.</p>
      )}
      {readOnly ? null : (
        <Popover
          open={open}
          onOpenChange={(next) => {
            setOpen(next);
            if (!next) setQuery("");
          }}
        >
          <PopoverTrigger asChild>
            <Button type="button" variant="outline" size="sm">
              <PlusIcon />
              Add conversation
            </Button>
          </PopoverTrigger>
          <PopoverContent className="w-72 p-0" align="start">
            <Command shouldFilter={false}>
              <CommandInput placeholder="Search conversations" value={query} onValueChange={setQuery} />
              <CommandList>
                {results.isPending ? (
                  <div className="space-y-2 p-2">
                    {[0, 1, 2].map((i) => (
                      <Skeleton key={i} className="h-7 w-full" />
                    ))}
                  </div>
                ) : results.isError ? (
                  <p className="px-3 py-6 text-center text-sm text-muted-foreground">
                    Couldn't load conversations. Close this and try again.
                  </p>
                ) : (
                  <>
                    <CommandEmpty>No conversation matches.</CommandEmpty>
                    <CommandGroup>
                      {items.map((c) => {
                        const on = value.includes(c.id);
                        return (
                          <CommandItem
                            key={c.id}
                            value={c.id}
                            onSelect={() => {
                              setNames((n) => ({ ...n, [c.id]: conversationName(c) }));
                              onChange(on ? value.filter((v) => v !== c.id) : [...value, c.id]);
                            }}
                          >
                            <span className="min-w-0 flex-1 truncate">{conversationName(c)}</span>
                            {on ? <CheckIcon className="size-4" /> : null}
                          </CommandItem>
                        );
                      })}
                    </CommandGroup>
                    {total !== null && total > items.length ? (
                      <p className="border-t px-3 py-2 text-xs text-muted-foreground">
                        Showing {items.length} of {total.toLocaleString("en")}. Type to narrow it down.
                      </p>
                    ) : null}
                  </>
                )}
              </CommandList>
            </Command>
          </PopoverContent>
        </Popover>
      )}
    </div>
  );
}
