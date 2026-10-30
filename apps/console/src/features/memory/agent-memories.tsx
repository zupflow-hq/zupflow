import { Link } from "@tanstack/react-router";
import {
  type Agent,
  type ApiKey,
  MEMORY_KIND_NOTES,
  type Member,
  type Memory,
  type MemoryOrigin,
  type MemoryType,
  type MemoryTypeId,
  type Organization,
  type Workspace,
} from "@zupflow/sdk";
import {
  BrainIcon,
  EllipsisIcon,
  MessageSquareIcon,
  PencilIcon,
  PlusIcon,
  ShieldAlertIcon,
  Trash2Icon,
} from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { ConfirmDialog } from "@/components/app/confirm-dialog";
import { QueryError, RelativeTime } from "@/components/app/misc";
import { Panel } from "@/components/app/page";
import { SearchInput, Toolbar } from "@/components/app/toolbar";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
import { canChange } from "@/features/context";
import {
  useApiKeys,
  useCreateMemory,
  useDeleteMemory,
  useForgetAllMemories,
  useMembers,
  useMemories,
  useMemoryKinds,
  useMemoryType,
  useMemoryTypeList,
  useUpdateMemory,
} from "@/features/queries";
import { isApiError, isStatus } from "@/lib/errors";
import { shortId } from "@/lib/format";
import { useDebounced } from "@/lib/use-debounced";
import { useAgentMemoryType } from "./agent-memory";
import { type MemoryAction, memoryRefusal, NOTE_CHARACTERS, noMemoryFix, noteLimit, notServed } from "./labels";
import { Mark } from "./mark";
import { MemoryTextDialog } from "./memory-text-dialog";
import { MemoryTypePicker } from "./memory-type-picker";

/** Why the Console shows memories in Production without changing them. */
export const PRODUCTION_MEMORIES =
  "In Production, memories are saved, changed and forgotten through the API. The Console shows them.";

/** The picker's value while no memory type is shown. */
const NO_TYPE = "none";

/**
 * What an agent remembers in its workspace, one memory type at a time: the
 * type it uses unless another is chosen. Newest first, searched on the
 * server, a page at a time. Developers and up add, change and forget them in
 * Sandbox; everyone else, and everyone in Production, reads them.
 */
export function AgentMemories({ org, workspace, agent }: { org: Organization; workspace: Workspace; agent: Agent }) {
  const current = useAgentMemoryType(org, agent);
  const kinds = useMemoryKinds(org.id);
  // One row is enough to know whether there's another type to look in; the picker pages the rest.
  const types = useMemoryTypeList(org.id, 2);
  const [viewing, setViewing] = useState<MemoryTypeId | null>(null);
  const viewed = useMemoryType(org.id, viewing);
  // A type chosen to look in that was deleted since: the list goes back to the agent's own.
  const looking = viewing !== null && isStatus(viewed.error, 404) ? null : viewing;
  const [query, setQuery] = useState("");
  const q = useDebounced(query.trim());
  const production = workspace.environment === "production";
  const mayWrite = canChange(org) && !production;

  // The type the list shows: one chosen to look in, or the one the agent uses.
  const shown: MemoryType | null | undefined = looking ? viewed.data : current.type;
  const kind = shown ? kinds.data?.find((k) => k.kind === shown.kind) : undefined;
  const reachable = Boolean(shown?.served);
  // Nothing to list without a type, and a type of a kind this server doesn't serve is refused.
  const listing = Boolean(shown && reachable);
  // Listed by the type's ID, never as "the agent's own": that can change while the page is open.
  const memories = useMemories(workspace.id, agent.id, { memory_type: shown?.id, q: q || undefined }, listing);
  const items = memories.data?.pages.flatMap((page) => page.items) ?? [];
  // Counted only from this type's list as the server answered it, never from results standing in while it loads.
  const answered = listing && memories.isSuccess && !memories.isPlaceholderData;
  const total = answered ? (memories.data?.pages[0]?.pagination.total ?? null) : null;
  const limit = noteLimit(shown ?? undefined);
  // The Console knows how much text one kind keeps: the one this repository ships.
  const characters = shown?.kind === MEMORY_KIND_NOTES ? NOTE_CHARACTERS : null;
  const full = !q && limit !== null && total !== null && total >= limit;
  const inUse = !looking || looking === current.type?.id;
  const manage = kind?.manage;
  // Any of the organization's types takes memories, the one the agent uses or another it may use later.
  const mayAdd = mayWrite && Boolean(shown && reachable && manage?.add);
  const anotherType = (types.data?.pages[0]?.pagination.total ?? 0) > (current.type ? 1 : 0);
  // Search only where there's a list: not while no type is shown, nor in a type this server can't read.
  const searchable = shown === undefined || reachable;

  // The type a new memory goes to, as it was when the dialog opened: a refresh behind it doesn't move it.
  const [addingTo, setAddingTo] = useState<MemoryType | null>(null);
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState<Memory | null>(null);
  const [forgetting, setForgetting] = useState<Memory | null>(null);
  const [forgettingAll, setForgettingAll] = useState(false);
  const create = useCreateMemory(workspace.id, agent.id);
  const update = useUpdateMemory(workspace.id, agent.id);
  const remove = useDeleteMemory(workspace.id, agent.id);
  const forgetAll = useForgetAllMemories(workspace.id, agent.id);
  const refusal = (error: unknown, action: MemoryAction, type = shown) =>
    memoryRefusal(error, { org, agent, type: type ?? undefined, action });
  const textError = (error: unknown) =>
    isApiError(error, "invalid_memory_text") ? (error.fields?.text ?? error.message) : null;
  const startAdding = () => {
    if (!shown) return;
    create.reset();
    setAddingTo(shown);
    setAdding(true);
  };

  const lookupError = current.error ?? (looking ? viewed.error : null);
  if (lookupError) {
    return (
      <QueryError
        error={lookupError}
        onRetry={() => {
          current.retry();
          void viewed.refetch();
        }}
        what="this agent's memory"
      />
    );
  }

  return (
    <div className="space-y-4">
      <MemorySummary org={org} agent={agent} type={current.type} />
      {production ? (
        <p className="text-sm text-muted-foreground">{PRODUCTION_MEMORIES}</p>
      ) : !mayWrite ? (
        <p className="text-sm text-muted-foreground">
          You can view memories. Owners, admins and developers can add, change and forget them.
        </p>
      ) : null}

      {searchable || anotherType || total !== null || mayAdd || mayWrite ? (
        <Toolbar className="sm:flex-wrap">
          <div className="flex min-w-0 flex-col gap-2 sm:flex-row sm:items-center">
            {searchable ? <SearchInput value={query} onChange={setQuery} placeholder="Search memories" /> : null}
            {anotherType ? (
              <MemoryTypePicker
                orgId={org.id}
                label="Memory type shown"
                className="sm:w-56"
                value={looking ?? current.type?.id ?? NO_TYPE}
                selected={shown ?? null}
                before={
                  current.type
                    ? []
                    : [
                        {
                          value: NO_TYPE,
                          label: <span className="text-muted-foreground">Choose a type</span>,
                          search: "",
                        },
                      ]
                }
                onChange={(next) => {
                  setViewing(next === NO_TYPE || next === current.type?.id ? null : (next as MemoryTypeId));
                }}
              />
            ) : null}
          </div>
          <div className="flex items-center gap-2">
            {total !== null ? (
              <span className="tabular text-sm whitespace-nowrap text-muted-foreground">
                {q
                  ? `${total.toLocaleString("en")} found`
                  : limit !== null
                    ? `${total.toLocaleString("en")} of ${limit.toLocaleString("en")}`
                    : `${total.toLocaleString("en")} ${total === 1 ? "memory" : "memories"}`}
              </span>
            ) : null}
            {mayAdd ? (
              <Button disabled={full} onClick={startAdding}>
                <PlusIcon />
                Add memory
              </Button>
            ) : null}
            {mayWrite ? (
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button variant="outline" size="icon" aria-label="More memory actions">
                    {forgetAll.isPending ? <Spinner /> : <EllipsisIcon />}
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end">
                  <DropdownMenuItem
                    variant="destructive"
                    onSelect={() => {
                      forgetAll.reset();
                      setForgettingAll(true);
                    }}
                  >
                    <Trash2Icon />
                    Forget all memories
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
            ) : null}
          </div>
        </Toolbar>
      ) : null}

      {full && shown && (inUse || mayAdd) ? (
        <p className="flex items-start gap-2 rounded-xl border border-warning/40 bg-warning-soft/40 p-3 text-sm">
          <ShieldAlertIcon className="mt-0.5 size-4 shrink-0 text-warning" />
          <span>
            {agent.name} keeps {limit?.toLocaleString("en")} memories at most in {shown.name}, and it's full
            {inUse
              ? `: its runs can't save more until ${mayWrite ? "you forget one, or change one instead" : "one is forgotten"}.`
              : ". Forget one to add another."}
          </span>
        </p>
      ) : null}

      <MemoryList
        org={org}
        workspace={workspace}
        agent={agent}
        shown={shown}
        canChooseType={anotherType}
        viewingOther={!inUse}
        query={q}
        memories={memories}
        items={items}
        mayChange={mayWrite && Boolean(manage?.change)}
        mayForget={mayWrite && Boolean(manage?.forget)}
        mayAdd={mayAdd && !full}
        onAdd={startAdding}
        onEdit={(memory) => {
          update.reset();
          setEditing(memory);
        }}
        onForget={(memory) => {
          remove.reset();
          setForgetting(memory);
        }}
      />

      {mayWrite ? (
        <>
          {addingTo ? (
            <MemoryTextDialog
              title="Add memory"
              description={
                addingTo.id === current.type?.id
                  ? `For ${agent.name} in ${workspace.name}, in ${addingTo.name}. Its runs here can recall it.`
                  : `For ${agent.name} in ${workspace.name}, in ${addingTo.name}, which it doesn't use now: its runs recall this only once it does.`
              }
              open={adding}
              onOpenChange={setAdding}
              pending={create.isPending}
              limit={addingTo.kind === MEMORY_KIND_NOTES ? NOTE_CHARACTERS : null}
              textError={textError(create.error)}
              refusal={create.isError && !textError(create.error) ? refusal(create.error, "add", addingTo) : null}
              actionLabel="Add memory"
              onSave={(text) =>
                create.mutate(
                  // Named, so it lands in the type the dialog says even if the agent's own changed meanwhile.
                  { text, memory_type_id: addingTo.id },
                  {
                    onSuccess: (memory) => {
                      setAdding(false);
                      toast.success(`Saved memory ${memory.short_id}`);
                    },
                  },
                )
              }
            />
          ) : null}
          <MemoryTextDialog
            title="Change memory"
            description={
              <>
                <code className="text-xs text-foreground">{editing?.short_id}</code>. Who changes it becomes who wrote
                it.
              </>
            }
            initial={editing?.text ?? ""}
            open={editing !== null}
            onOpenChange={(open) => {
              if (!open) setEditing(null);
            }}
            pending={update.isPending}
            limit={characters}
            textError={textError(update.error)}
            refusal={update.isError && !textError(update.error) ? refusal(update.error, "change") : null}
            actionLabel="Save memory"
            onSave={(text) => {
              if (!editing) return;
              const memory = editing;
              update.mutate(
                { id: memory.id, text },
                {
                  onSuccess: () => {
                    setEditing(null);
                    toast.success(`Saved memory ${memory.short_id}`);
                  },
                },
              );
            }}
          />
          <ConfirmDialog
            open={forgetting !== null}
            onOpenChange={(open) => {
              if (!open) setForgetting(null);
            }}
            title="Forget this memory?"
            description={
              <>
                <blockquote className="line-clamp-4 border-l-2 pl-3 text-foreground [overflow-wrap:anywhere]">
                  {forgetting?.text}
                </blockquote>
                <p>{agent.name}'s runs won't recall it again. This can't be undone.</p>
                {remove.isError ? (
                  <p
                    role="alert"
                    className="rounded-lg border border-destructive/30 bg-danger-soft/50 p-3 text-left text-foreground"
                  >
                    {refusal(remove.error, "forget")}
                  </p>
                ) : null}
              </>
            }
            actionLabel="Forget memory"
            pending={remove.isPending}
            onConfirm={() => {
              if (!forgetting) return;
              const memory = forgetting;
              remove.mutate(memory.id, {
                onSuccess: () => {
                  setForgetting(null);
                  toast.success(`Forgot memory ${memory.short_id}`);
                },
                onError: (error) => {
                  // Already gone: the list is read again, and there's nothing left to confirm.
                  if (isStatus(error, 404)) {
                    setForgetting(null);
                    toast.success(`Memory ${memory.short_id} was already forgotten`);
                  }
                },
              });
            }}
          />
          <ConfirmDialog
            open={forgettingAll}
            onOpenChange={setForgettingAll}
            title={`Forget everything ${agent.name} remembers here?`}
            description={
              <>
                <p>
                  Every memory {agent.name} keeps in {workspace.name} is forgotten, in each memory type of a kind this
                  server serves, and webhooks get <code className="text-xs">memory.forgotten</code> for each.{" "}
                  {current.type?.served ? "Its memory stays on, so its runs start remembering again. " : ""}
                  This can't be undone.
                </p>
                {forgetAll.isError ? (
                  <p
                    role="alert"
                    className="rounded-lg border border-destructive/30 bg-danger-soft/50 p-3 text-left text-foreground"
                  >
                    {refusal(forgetAll.error, "forget_all")}
                  </p>
                ) : null}
              </>
            }
            confirmText={agent.name}
            actionLabel="Forget all memories"
            pending={forgetAll.isPending}
            onConfirm={() =>
              forgetAll.mutate(undefined, {
                onSuccess: () => {
                  setForgettingAll(false);
                  toast.success(`Forgot everything ${agent.name} kept in ${workspace.name}`);
                },
              })
            }
          />
        </>
      ) : null}
    </div>
  );
}

/** What the agent remembers with, or why it remembers nothing. */
function MemorySummary({
  org,
  agent,
  type,
}: {
  org: Organization;
  agent: Agent;
  /** The type it uses; null for none, undefined while loading. */
  type: MemoryType | null | undefined;
}) {
  if (type === undefined) return <Skeleton className="h-5 w-80 max-w-full" />;
  const turnOn = canChange(org)
    ? "Turn it on in its Configuration tab."
    : "An owner, an admin or a developer can turn it on in its Configuration tab.";
  const fix =
    type === null
      ? agent.memory === "off"
        ? `${agent.name}'s memory is off: its runs neither recall nor save memories. ${turnOn} What it kept before stays until it's forgotten.`
        : noMemoryFix(org, agent)
      : !type.served
        ? notServed(type)
        : null;
  if (fix) {
    return (
      <p className="flex items-start gap-2 rounded-xl border border-warning/40 bg-warning-soft/40 p-3 text-sm">
        <BrainIcon className="mt-0.5 size-4 shrink-0 text-warning" />
        <span>{fix}</span>
      </p>
    );
  }
  return (
    <p className="flex items-start gap-2 text-sm text-muted-foreground">
      <BrainIcon className="mt-0.5 size-4 shrink-0" />
      <span>
        {agent.name} remembers with <span className="font-medium text-foreground">{type?.name}</span>
        {agent.memory === "default" ? ", the organization's default memory type" : ""}. What it keeps here is its own,
        in this workspace only.
      </span>
    </p>
  );
}

function MemoryList({
  org,
  workspace,
  agent,
  shown,
  canChooseType,
  viewingOther,
  query,
  memories,
  items,
  mayChange,
  mayForget,
  mayAdd,
  onAdd,
  onEdit,
  onForget,
}: {
  org: Organization;
  workspace: Workspace;
  agent: Agent;
  shown: MemoryType | null | undefined;
  /** Whether another of the organization's types can be chosen to look in. */
  canChooseType: boolean;
  viewingOther: boolean;
  query: string;
  memories: ReturnType<typeof useMemories>;
  items: Memory[];
  mayChange: boolean;
  mayForget: boolean;
  mayAdd: boolean;
  onAdd: () => void;
  onEdit: (memory: Memory) => void;
  onForget: (memory: Memory) => void;
}) {
  const members = useMembers(org.id);
  const keys = useApiKeys(org.id);
  // Who wrote each memory: null when the names couldn't be read, so rows say "a member" instead of waiting.
  const memberList = members.isError ? null : members.data;
  const keyList = keys.isError ? null : keys.data;
  if (shown === null) {
    // What it kept before its memory went off, or before the default went away, stays in its type.
    return canChooseType ? (
      <Panel>
        <p className="p-6 text-center text-sm text-muted-foreground">
          Choose a memory type above to see what {agent.name} kept in it.
        </p>
      </Panel>
    ) : null;
  }
  if (shown && !shown.served) {
    return (
      <Panel>
        <p className="p-6 text-center text-sm text-muted-foreground">
          This server doesn't serve {shown.kind}, so {shown.name}'s memories can't be read here.
        </p>
      </Panel>
    );
  }
  if (memories.isError) {
    if (isApiError(memories.error, "memory_kind_unsupported")) {
      return (
        <Panel>
          <p className="p-6 text-center text-sm text-muted-foreground">
            The kind of {shown?.name ?? "this memory type"} doesn't let people list its memories from the Console or the
            API.
          </p>
        </Panel>
      );
    }
    if (isApiError(memories.error, "memory_kind_not_served") && shown) {
      return (
        <Panel>
          <p className="p-6 text-center text-sm text-muted-foreground">{notServed(shown)}</p>
        </Panel>
      );
    }
    if (isApiError(memories.error, "unknown_memory_type")) {
      return (
        <Panel>
          <p className="p-6 text-center text-sm text-muted-foreground">
            {shown?.name ?? "This memory type"} was deleted, with the memories it kept.
          </p>
        </Panel>
      );
    }
    return <QueryError error={memories.error} onRetry={() => memories.refetch()} what="memories" />;
  }
  if (!shown || memories.isPending) return <MemorySkeleton />;

  if (items.length === 0) {
    if (query) {
      return (
        <Panel>
          <p className="p-10 text-center text-sm text-muted-foreground">No memory holds every word of “{query}”.</p>
        </Panel>
      );
    }
    return (
      <Panel>
        <Empty>
          <EmptyHeader>
            <EmptyMedia variant="icon">
              <BrainIcon />
            </EmptyMedia>
            <EmptyTitle>
              {viewingOther ? `Nothing kept in ${shown.name}` : `${agent.name} remembers nothing here yet`}
            </EmptyTitle>
            <EmptyDescription>
              {viewingOther
                ? `${agent.name} keeps no memories in ${shown.name} in this workspace.`
                : shown.writes === "deny"
                  ? searchOnly(org, workspace)
                  : shown.writes === "approve"
                    ? "Its runs save what's worth keeping once a person approves each save."
                    : "Its runs save what's worth keeping as they go."}
            </EmptyDescription>
          </EmptyHeader>
          {mayAdd ? (
            <EmptyContent>
              <Button onClick={onAdd}>
                <PlusIcon />
                Add memory
              </Button>
            </EmptyContent>
          ) : null}
        </Empty>
      </Panel>
    );
  }

  // The last search's results stand in, dimmed, while the next loads: nothing to page or count in them.
  const standIn = memories.isPlaceholderData;
  const total = standIn ? null : (memories.data?.pages[0]?.pagination.total ?? null);
  return (
    <>
      <Panel className={standIn ? "opacity-60 transition-opacity" : undefined}>
        <ul className="divide-y">
          {items.map((memory) => (
            <MemoryRow
              key={memory.id}
              org={org}
              workspace={workspace}
              memory={memory}
              members={memberList}
              keys={keyList}
              mayChange={mayChange}
              mayForget={mayForget}
              onEdit={() => onEdit(memory)}
              onForget={() => onForget(memory)}
            />
          ))}
        </ul>
      </Panel>
      {memories.hasNextPage && !standIn ? (
        <div className="flex flex-col items-center gap-2">
          <Button variant="outline" onClick={() => memories.fetchNextPage()} disabled={memories.isFetchingNextPage}>
            {memories.isFetchingNextPage ? <Spinner /> : null}
            Load older memories
          </Button>
          {total !== null ? (
            <p className="tabular text-xs text-muted-foreground">
              {items.length.toLocaleString("en")} of {total.toLocaleString("en")} shown
            </p>
          ) : null}
        </div>
      ) : null}
    </>
  );
}

/** Where a search-only type's memories come from, for this person in this workspace. */
function searchOnly(org: Organization, workspace: Workspace): string {
  if (workspace.environment === "production") {
    return "Its runs only search their memories, which are added through the API.";
  }
  return canChange(org)
    ? "Its runs only search their memories. Add them here, or through the API."
    : "Its runs only search their memories, which owners, admins and developers add here or through the API.";
}

function MemorySkeleton() {
  return (
    <Panel>
      <div className="divide-y" role="status" aria-busy aria-label="Loading">
        {[72, 54, 64].map((width) => (
          <div key={width} className="space-y-2.5 px-4 py-3.5">
            <Skeleton className="h-4" style={{ width: `${width}%` }} />
            <Skeleton className="h-3 w-48 max-w-full" />
          </div>
        ))}
      </div>
    </Panel>
  );
}

function MemoryRow({
  org,
  workspace,
  memory,
  members,
  keys,
  mayChange,
  mayForget,
  onEdit,
  onForget,
}: {
  org: Organization;
  workspace: Workspace;
  memory: Memory;
  members: Member[] | null | undefined;
  keys: ApiKey[] | null | undefined;
  mayChange: boolean;
  mayForget: boolean;
  onEdit: () => void;
  onForget: () => void;
}) {
  const changed = memory.updated_at !== memory.created_at;
  const origin = memory.origin;
  return (
    <li className="flex items-start gap-3 px-4 py-3">
      <div className="min-w-0 flex-1 space-y-1.5">
        <p className="text-sm break-words">{memory.text}</p>
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground">
          <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-[11px] text-foreground/90">
            {memory.short_id}
          </code>
          <span>
            {changed ? "Changed" : "Saved"} <RelativeTime iso={memory.updated_at} /> by{" "}
            <OriginName org={org} workspace={workspace} origin={origin} members={members} keys={keys} />
          </span>
          {origin.type === "run" && origin.after_app_content ? (
            <Mark
              tone="warning"
              label="After app results"
              explanation="The run had read an app's results before it wrote this, so the text may come from that app's content. Check it before relying on it."
            />
          ) : null}
          {memory.channel_conversation_id ? (
            <Mark
              tone="neutral"
              icon={<MessageSquareIcon className="size-3" />}
              label="Direct conversation"
              explanation="Saved in a direct conversation: recalled and found only there."
            />
          ) : null}
        </div>
      </div>
      {mayChange || mayForget ? (
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button
              variant="ghost"
              size="icon"
              className="-my-1.5"
              aria-label={`Actions for memory ${memory.short_id}`}
            >
              <EllipsisIcon />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            {mayChange ? (
              <DropdownMenuItem onSelect={onEdit}>
                <PencilIcon />
                Change
              </DropdownMenuItem>
            ) : null}
            {mayChange && mayForget ? <DropdownMenuSeparator /> : null}
            {mayForget ? (
              <DropdownMenuItem variant="destructive" onSelect={onForget}>
                <Trash2Icon />
                Forget
              </DropdownMenuItem>
            ) : null}
          </DropdownMenuContent>
        </DropdownMenu>
      ) : null}
    </li>
  );
}

/**
 * Who wrote a memory's current text: a member by name, an API key by its
 * name, or the run. While names load, a placeholder; when they couldn't be
 * read (null), "a member" or "an API key".
 */
function OriginName({
  org,
  workspace,
  origin,
  members,
  keys,
}: {
  org: Organization;
  workspace: Workspace;
  origin: MemoryOrigin;
  members: Member[] | null | undefined;
  keys: ApiKey[] | null | undefined;
}) {
  switch (origin.type) {
    case "member": {
      if (members === undefined) return <NamePlaceholder />;
      if (members === null) return <span>a member</span>;
      const member = members.find((m) => m.user.id === origin.user_id);
      return <span className="text-foreground">{member?.user.name ?? "a former member"}</span>;
    }
    case "api_key": {
      if (keys === undefined) return <NamePlaceholder />;
      const key = keys?.find((k) => k.id === origin.api_key_id);
      return key ? (
        <span>
          API key <span className="text-foreground">{key.name}</span>
        </span>
      ) : (
        <span>an API key</span>
      );
    }
    case "run":
      return (
        <Link
          to="/$org/w/$ws/runs/$runId"
          params={{ org: org.slug, ws: workspace.id, runId: origin.run_id }}
          className="font-mono text-foreground underline-offset-4 hover:underline"
        >
          {shortId(origin.run_id)}
        </Link>
      );
  }
}

/** A name still loading, inline in a line of text. */
function NamePlaceholder() {
  return <span aria-hidden className="inline-block h-3 w-20 animate-pulse rounded-md bg-accent align-middle" />;
}
