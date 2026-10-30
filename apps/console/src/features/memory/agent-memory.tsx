import { Link } from "@tanstack/react-router";
import type { Agent, MemoryType, MemoryTypeId, Organization } from "@zupflow/sdk";
import { ArrowRightIcon, BrainIcon } from "lucide-react";
import { Skeleton } from "@/components/ui/skeleton";
import { canAdminister, canChange } from "@/features/context";
import { useDefaultMemoryType, useMemoryType, useMemoryTypeList } from "@/features/queries";
import { cn } from "@/lib/utils";
import { notServed } from "./labels";

/**
 * The memory type an agent remembers with now: its own, the organization's
 * default for one on the default, or none (memory off, or no default).
 * `type` is undefined while it loads.
 */
export function useAgentMemoryType(
  org: Organization,
  agent: Pick<Agent, "memory" | "memory_type_id">,
): { type: MemoryType | null | undefined; error: Error | null; retry: () => void } {
  const own = agent.memory === "type" ? agent.memory_type_id : null;
  const chosen = useMemoryType(org.id, own);
  const defaults = useDefaultMemoryType(org.id);
  if (agent.memory === "off") return { type: null, error: null, retry: () => {} };
  if (agent.memory === "type") {
    return { type: own ? chosen.data : null, error: chosen.error, retry: () => void chosen.refetch() };
  }
  return {
    type: defaults.data ? defaults.data.memory_type : undefined,
    error: defaults.error,
    retry: () => void defaults.refetch(),
  };
}

/**
 * An agent's memory as a short line: "Notes (organization default)", a type
 * of its own, off, or that the default it uses is missing. A failed lookup
 * says so instead of loading forever.
 */
export function AgentMemoryName({
  org,
  agent,
}: {
  org: Organization;
  agent: Pick<Agent, "memory" | "memory_type_id">;
}) {
  const { type, error } = useAgentMemoryType(org, agent);
  if (agent.memory === "off") return <span className="text-muted-foreground">Off</span>;
  if (error) return <span className="text-muted-foreground">Memory type not found</span>;
  if (type === undefined) return <Skeleton className="inline-block h-3.5 w-28 align-middle" />;
  if (type === null) {
    return <span className="text-muted-foreground">None: the organization has no default memory type</span>;
  }
  return (
    <span>
      {type.name}
      {agent.memory === "default" ? <span className="text-muted-foreground"> (organization default)</span> : null}
      {type.served ? null : <span className="text-muted-foreground"> · not served, so no memory</span>}
    </span>
  );
}

/**
 * Explains why an agent with this memory choice would run without memory, and
 * links to where it's fixed: no default to use, no memory type at all, or a
 * type of a kind this server doesn't serve. Nothing when it remembers, or
 * when memory is deliberately off.
 */
export function AgentMemoryNotice({
  org,
  choice,
  className,
}: {
  org: Organization;
  /** `default`, `off`, or a memory type's ID. */
  choice: string;
  className?: string;
}) {
  const onDefault = choice === "default";
  const own = !onDefault && choice !== "off" ? (choice as MemoryTypeId) : null;
  const defaults = useDefaultMemoryType(org.id);
  const chosen = useMemoryType(org.id, own);
  // One row is enough to know whether there's any type to choose.
  const anyType = useMemoryTypeList(org.id, 1, onDefault);
  const admin = canAdminister(org);
  if (choice === "off") return null;

  let message: string | null = null;
  if (onDefault && defaults.data && !defaults.data.memory_type && anyType.data) {
    const none = (anyType.data.pages[0]?.items.length ?? 0) === 0;
    const what = `${org.name} has no default memory type, so this agent runs without memory.`;
    const fix = none
      ? admin
        ? "Create a memory type on the Memory page and make it the default."
        : "Ask an owner or admin to create a memory type and make it the default."
      : admin
        ? "Choose a default on the Memory page, or a memory type for this agent."
        : canChange(org)
          ? "Ask an owner or admin to choose a default, or choose a memory type for this agent."
          : "Ask an owner or admin to choose a default, or a developer to choose a memory type for this agent.";
    message = `${what} ${fix}`;
  } else if (onDefault && defaults.data?.memory_type && !defaults.data.memory_type.served) {
    message = notServed(defaults.data.memory_type);
  } else if (own && chosen.data && !chosen.data.served) {
    message = notServed(chosen.data);
  }
  if (!message) return null;
  return (
    <div className={cn("flex gap-3 rounded-lg border bg-muted/40 p-3 text-sm", className)}>
      <BrainIcon className="mt-0.5 size-4 shrink-0 text-muted-foreground" />
      <div className="min-w-0 space-y-1.5">
        <p>{message}</p>
        <Link
          to="/$org/memory"
          params={{ org: org.slug }}
          className="inline-flex items-center gap-1 font-medium underline-offset-4 hover:underline"
        >
          Open memory
          <ArrowRightIcon className="size-3.5" />
        </Link>
      </div>
    </div>
  );
}
