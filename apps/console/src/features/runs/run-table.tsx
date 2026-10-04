import { useNavigate } from "@tanstack/react-router";
import type { Run, WorkspaceRef } from "@zupflow/sdk";
import { RelativeTime, rowLinkProps } from "@/components/app/misc";
import { RunStatusPill } from "@/components/app/status";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { duration, shortId } from "@/lib/format";

/** Runs, newest first. Rows from several workspaces carry `workspace` and show it. */
export function RunTable({
  runs,
  orgSlug,
  showAgent = true,
  empty = "No runs yet.",
}: {
  runs: (Run & { workspace?: WorkspaceRef })[];
  orgSlug: string;
  showAgent?: boolean;
  empty?: React.ReactNode;
}) {
  const showWorkspace = runs.some((r) => r.workspace);
  const navigate = useNavigate();
  return (
    <div className="@container">
      <Table className="table-fixed">
        <TableHeader>
          <TableRow className="hover:bg-transparent">
            <TableHead className="w-auto pl-4">Input</TableHead>
            {showWorkspace ? <TableHead className="hidden w-44 @2xl:table-cell">Workspace</TableHead> : null}
            {showAgent ? <TableHead className="hidden w-44 @2xl:table-cell">Agent</TableHead> : null}
            <TableHead className="w-36">Status</TableHead>
            <TableHead className="hidden w-16 text-right @3xl:table-cell">Tools</TableHead>
            <TableHead className="hidden w-24 text-right @lg:table-cell">Duration</TableHead>
            <TableHead className="hidden w-32 pr-4 @xl:table-cell">Started</TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {runs.length === 0 ? (
            <TableRow className="hover:bg-transparent">
              <TableCell colSpan={7} className="py-10 text-center text-muted-foreground">
                {empty}
              </TableCell>
            </TableRow>
          ) : (
            runs.map((run) => (
              <TableRow
                key={run.id}
                {...rowLinkProps(() =>
                  navigate({
                    to: "/$org/w/$ws/runs/$runId",
                    params: { org: orgSlug, ws: run.workspace_id, runId: run.id },
                  }),
                )}
              >
                <TableCell className="py-3 pl-4">
                  <p className="truncate font-medium">{run.input}</p>
                  <p className="truncate font-mono text-xs text-muted-foreground">
                    {shortId(run.id)}
                    {run.workspace ? <span className="@2xl:hidden"> · {run.workspace.name}</span> : null}
                    {showAgent ? <span className="@2xl:hidden"> · {run.agent_name}</span> : null}
                    {run.trigger === "console" ? " · console" : ""}
                  </p>
                </TableCell>
                {showWorkspace ? (
                  <TableCell className="hidden truncate @2xl:table-cell">{run.workspace?.name}</TableCell>
                ) : null}
                {showAgent ? <TableCell className="hidden truncate @2xl:table-cell">{run.agent_name}</TableCell> : null}
                <TableCell>
                  <RunStatusPill status={run.status} />
                </TableCell>
                <TableCell className="tabular hidden text-right @3xl:table-cell">{run.usage.tool_calls}</TableCell>
                <TableCell className="tabular hidden text-right text-muted-foreground @lg:table-cell">
                  {duration(run.started_at, run.finished_at)}
                </TableCell>
                <TableCell className="hidden pr-4 text-muted-foreground @xl:table-cell">
                  <RelativeTime iso={run.created_at} />
                </TableCell>
              </TableRow>
            ))
          )}
        </TableBody>
      </Table>
    </div>
  );
}
