import { createFileRoute } from "@tanstack/react-router";
import { MEMORY_KIND_NOTES, type MemoryType, type Organization } from "@zupflow/sdk";
import { BrainIcon, EllipsisIcon, PencilIcon, PlusIcon, StarIcon, Trash2Icon } from "lucide-react";
import { useState } from "react";
import { toast } from "sonner";
import { QueryError, RowsSkeleton } from "@/components/app/misc";
import { PageContainer, PageHeader, Panel, SectionHeader } from "@/components/app/page";
import { StatusPill } from "@/components/app/status";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty";
import { Spinner } from "@/components/ui/spinner";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { canAdminister, useDocumentTitle, useOrg } from "@/features/context";
import { DefaultMemoryTypePanel } from "@/features/memory/default-memory-type";
import { DeleteMemoryTypeDialog } from "@/features/memory/delete-memory-type";
import { kindName, listed, memoryTypeRefusal, notServed, WRITES } from "@/features/memory/labels";
import { Mark } from "@/features/memory/mark";
import { MemoryTypeDialog } from "@/features/memory/memory-type-dialog";
import { useMemoryKinds, useMemoryTypeList, useSetDefaultMemoryType } from "@/features/queries";
import { cn } from "@/lib/utils";

export const Route = createFileRoute("/_authed/$org/memory")({
  staticData: { crumb: "Memory" },
  component: MemoryPage,
});

function MemoryPage() {
  const org = useOrg();
  useDocumentTitle("Memory", org.name);
  const admin = canAdminister(org);
  const kinds = useMemoryKinds(org.id);
  const types = useMemoryTypeList(org.id);
  const items = types.data?.pages.flatMap((page) => page.items) ?? [];
  const total = types.data?.pages[0]?.pagination.total ?? null;
  const [creating, setCreating] = useState(false);
  // The type being edited as it was when its dialog opened, so a refresh behind the form doesn't reset it.
  const [editing, setEditing] = useState<MemoryType | null>(null);
  const [editOpen, setEditOpen] = useState(false);
  // The type being deleted as listed now, so a refresh after a refusal shows what holds it; as it was when its
  // dialog opened once it's gone from the list, so the dialog doesn't empty as it closes.
  const [toDelete, setToDelete] = useState<MemoryType | null>(null);
  const deleting = (toDelete && items.find((t) => t.id === toDelete.id)) ?? toDelete;
  const [deleteOpen, setDeleteOpen] = useState(false);

  return (
    <PageContainer>
      <PageHeader
        title="Memory"
        description="What agents keep from one conversation for the next, in each workspace. Memory types are the organization's, the same in Sandbox and Production: each agent uses the default, a type of its own, or none."
        actions={
          admin ? (
            <Button onClick={() => setCreating(true)}>
              <PlusIcon />
              Create memory type
            </Button>
          ) : null
        }
      />
      <div className="space-y-8">
        <DefaultMemoryTypePanel org={org} />

        <section>
          <SectionHeader
            title="Memory types"
            description={admin ? undefined : "Owners and admins create memory types and choose the default."}
          />
          {types.isError ? (
            <QueryError error={types.error} onRetry={() => types.refetch()} what="memory types" />
          ) : items.length === 0 && types.data ? (
            <Panel>
              <Empty>
                <EmptyHeader>
                  <EmptyMedia variant="icon">
                    <BrainIcon />
                  </EmptyMedia>
                  <EmptyTitle>No memory types yet</EmptyTitle>
                  <EmptyDescription>
                    A memory type sets up a kind this server serves, such as notes, for the organization's agents.
                    Create one, then make it the default so agents remember.
                  </EmptyDescription>
                </EmptyHeader>
                {admin ? (
                  <EmptyContent>
                    <Button onClick={() => setCreating(true)}>
                      <PlusIcon />
                      Create memory type
                    </Button>
                  </EmptyContent>
                ) : null}
              </Empty>
            </Panel>
          ) : (
            <Panel className="overflow-hidden">
              {!types.data ? (
                <RowsSkeleton rows={2} />
              ) : (
                <Table>
                  <TableHeader>
                    <TableRow className="hover:bg-transparent">
                      <TableHead className="pl-4">Memory type</TableHead>
                      <TableHead className="hidden md:table-cell">Agent writes</TableHead>
                      <TableHead className={cn("hidden text-right sm:table-cell", !admin && "pr-4")}>
                        Chosen by
                      </TableHead>
                      {admin ? (
                        <TableHead className="w-12 pr-4">
                          <span className="sr-only">Actions</span>
                        </TableHead>
                      ) : null}
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {items.map((t) => (
                      <TypeRow
                        key={t.id}
                        org={org}
                        type={t}
                        kind={kindName(kinds.data, t.kind)}
                        admin={admin}
                        onEdit={() => {
                          setEditing(t);
                          setEditOpen(true);
                        }}
                        onDelete={() => {
                          setToDelete(t);
                          setDeleteOpen(true);
                        }}
                      />
                    ))}
                  </TableBody>
                </Table>
              )}
              {types.hasNextPage ? (
                <div className="border-t p-3 text-center">
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={() => types.fetchNextPage()}
                    disabled={types.isFetchingNextPage}
                  >
                    {types.isFetchingNextPage ? <Spinner /> : null}
                    Show more
                    {total !== null ? (
                      <span className="tabular text-muted-foreground">
                        {items.length.toLocaleString("en")} of {total.toLocaleString("en")} shown
                      </span>
                    ) : null}
                  </Button>
                </div>
              ) : null}
            </Panel>
          )}
          {items.length > 0 ? (
            <p className="mt-3 text-xs text-muted-foreground">
              Chosen by counts the agents that choose a type of their own; agents on the default use the default type
              and aren't counted.{" "}
              {kinds.data
                ? kinds.data.length > 0
                  ? `This server serves ${listed(kinds.data.map((k) => k.name))}.`
                  : "This server serves no memory kinds."
                : null}
            </p>
          ) : null}
        </section>
      </div>

      {admin ? (
        <>
          <MemoryTypeDialog org={org} type={null} open={creating} onOpenChange={setCreating} />
          <MemoryTypeDialog org={org} type={editing} open={editOpen} onOpenChange={setEditOpen} />
          {deleting ? (
            // One dialog per type: nothing typed or refused for one carries over to another, even of the same name.
            <DeleteMemoryTypeDialog
              key={deleting.id}
              org={org}
              type={deleting}
              open={deleteOpen}
              onOpenChange={setDeleteOpen}
            />
          ) : null}
        </>
      ) : null}
    </PageContainer>
  );
}

function TypeRow({
  org,
  type: t,
  kind,
  admin,
  onEdit,
  onDelete,
}: {
  org: Organization;
  type: MemoryType;
  kind: string;
  admin: boolean;
  onEdit: () => void;
  onDelete: () => void;
}) {
  const setDefault = useSetDefaultMemoryType(org.id);
  return (
    <TableRow>
      <TableCell className={cn("w-full max-w-0 py-3 pl-4 whitespace-normal", !admin && "pr-4 sm:pr-2")}>
        <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
          <span className="min-w-0 font-medium break-words">{t.name}</span>
          {t.is_default ? <StatusPill tone="info">Default</StatusPill> : null}
          {t.served ? null : <Mark tone="warning" label="Not served" explanation={notServed(t)} />}
        </div>
        <p className="text-sm break-words text-muted-foreground">
          {summary(t, kind)}
          <span className="md:hidden"> · Agent writes: {WRITES[t.writes].label}</span>
          <span className="sm:hidden">
            {" "}
            ·{" "}
            {t.agent_count === 0
              ? "chosen by no agent"
              : `chosen by ${t.agent_count} ${t.agent_count === 1 ? "agent" : "agents"}`}
          </span>
        </p>
      </TableCell>
      <TableCell className="hidden whitespace-nowrap md:table-cell">{WRITES[t.writes].label}</TableCell>
      <TableCell className={cn("tabular hidden text-right whitespace-nowrap sm:table-cell", !admin && "pr-4")}>
        {t.agent_count === 0 ? (
          <span className="text-muted-foreground">None</span>
        ) : (
          `${t.agent_count} ${t.agent_count === 1 ? "agent" : "agents"}`
        )}
      </TableCell>
      {admin ? (
        <TableCell className="pr-4 text-right">
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button variant="ghost" size="icon" aria-label={`Actions for ${t.name}`}>
                {setDefault.isPending ? <Spinner /> : <EllipsisIcon />}
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              <DropdownMenuItem onSelect={onEdit}>
                <PencilIcon />
                Edit
              </DropdownMenuItem>
              {t.is_default ? null : (
                <DropdownMenuItem
                  onSelect={() =>
                    setDefault.mutate(t.id, {
                      onSuccess: () => toast.success(`${t.name} is the default memory type`),
                      onError: (e) =>
                        toast.error("Couldn't change the default memory type", { description: memoryTypeRefusal(e) }),
                    })
                  }
                >
                  <StarIcon />
                  Make default
                </DropdownMenuItem>
              )}
              <DropdownMenuSeparator />
              <DropdownMenuItem variant="destructive" onSelect={onDelete}>
                <Trash2Icon />
                Delete memory type
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </TableCell>
      ) : null}
    </TableRow>
  );
}

/** A type's kind, and for notes how much each agent keeps and recalls. */
function summary(type: MemoryType, kind: string): React.ReactNode {
  // A kind this server doesn't serve has no name here, only its ID.
  if (!type.served) {
    return (
      <>
        <code className="text-xs">{type.kind}</code> kind
      </>
    );
  }
  const { max_notes: kept, recall_characters: recalled } = type.settings;
  if (type.kind !== MEMORY_KIND_NOTES || typeof kept !== "number" || typeof recalled !== "number") {
    return `${kind} kind`;
  }
  return `${kind} kind · up to ${kept.toLocaleString("en")} per agent, ${recalled.toLocaleString("en")} characters recalled`;
}
