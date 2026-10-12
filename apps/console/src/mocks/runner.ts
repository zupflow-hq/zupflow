/**
 * Simulated agent execution. A run is planned up front from the agent's
 * current grants, then advanced one step at a time: live runs on timers, seeded
 * runs instantly against a fake clock. Every step re-checks the installation,
 * so revoking access or a broken connection shows up the way it will for real.
 */
import type { ApiKeyId, RunError, RunEvent, RunEventType, ToolCallId } from "@zupflow/sdk";
import { sampleToolCall } from "./catalog";
import { newId } from "./ids";
import type { Db, PlannedCall, RunEventRow, RunRow, RunStep } from "./model";
import { type Rng, rngFor } from "./random";
import { appIn, runEventsKey, sameWorkspace, timelineOf } from "./scope";

const SUMMARY: Record<string, (label: string) => string> = {
  search_files: (l) => `found the relevant files in ${l}`,
  read_document: () => "read the launch plan",
  create_document: (l) => `created a draft document in ${l}`,
  list_events: () => "checked next week's calendar",
  send_email: () => "sent the summary email",
  list_invoices: (l) => `pulled open invoices from ${l}`,
  get_customer: () => "looked up the customer's subscriptions",
  list_tasks: () => "listed the tasks on the board",
  create_task: () => "created a task on the board",
  delete_task: () => "deleted a task from the board",
  search_issues: () => "found the matching issues",
  create_issue: () => "opened a new issue",
  list_drafts: () => "listed the current drafts",
  create_draft: () => "created a post draft",
  publish_post: () => "published the post",
  search_code: () => "searched the codebase",
  list_pull_requests: () => "listed the open pull requests",
  echo: () => "echoed the input",
  echo_write: () => "echoed the input as a write",
};

function sentence(parts: string[]): string {
  if (parts.length === 0) return "";
  if (parts.length === 1) return parts[0] ?? "";
  return `${parts.slice(0, -1).join(", ")} and ${parts[parts.length - 1]}`;
}

/** Decide what the run will do, from the agent's grants at creation time. */
export function planRun(
  db: Db,
  run: Pick<RunRow, "id" | "organization_id" | "workspace_id" | "agent_id" | "input" | "created_at">,
  rng: Rng = rngFor(run.id),
) {
  const at = Date.parse(run.created_at);
  const grants = db.grants.filter((g) => sameWorkspace(g, run) && g.agent_id === run.agent_id);

  type Candidate = { call: PlannedCall; effect: string; approval: boolean };
  const candidates: Candidate[] = [];
  for (const grant of grants) {
    const installation = db.installations.find((i) => sameWorkspace(i, run) && i.id === grant.installation_id);
    if (!installation) continue;
    // The app is looked up within the run's scope, never the installation row's own label.
    const app = appIn(db.apps, {
      organization_id: run.organization_id,
      workspace_id: run.workspace_id,
      app_id: installation.app_id,
    });
    if (!app) continue;
    for (const toolName of grant.tools) {
      const tool = app.tools.find((t) => t.name === toolName);
      if (!tool) continue;
      const sample = sampleToolCall(tool.name, run.input);
      candidates.push({
        effect: tool.effect,
        approval: grant.approval_required.includes(tool.name),
        call: {
          call_id: newId("tool_call", { rng, at }),
          installation_id: installation.id,
          installation_label: installation.label,
          app_name: app.name,
          tool: tool.name,
          arguments: sample.arguments,
          result: sample.result,
          error: null,
        },
      });
    }
  }

  const steps: RunStep[] = [{ kind: "start" }];

  if (rng() < 0.05) {
    steps.push({
      kind: "fail",
      error: { code: "model_unavailable", message: "The model provider did not respond in time." },
    });
    return steps;
  }

  if (candidates.length === 0) {
    steps.push({
      kind: "complete",
      parts: [],
      fallback:
        "I can't reach any connected accounts yet, so I answered from the conversation alone. Grant me an installation to work with real data.",
      closing: "",
    });
    return steps;
  }

  const reads = candidates.filter((c) => c.effect === "read");
  const writes = candidates.filter((c) => c.effect !== "read");
  const chosen: Candidate[] = [];
  const pick = (from: Candidate[]) => from[Math.floor(rng() * from.length)];
  const firstRead = pick(reads);
  if (firstRead) chosen.push(firstRead);
  const secondRead = pick(reads.filter((c) => c !== firstRead));
  if (secondRead && rng() < 0.5) chosen.push(secondRead);
  const wantsWrite = /\b(create|draft|publish|send|email|open|write|post)\b/i.test(run.input) || rng() < 0.3;
  // Prefer the write tool the request names, e.g. "publish" → publish_post.
  const verbs: [RegExp, RegExp][] = [
    [/\bpublish/i, /publish/],
    [/\b(send|email)/i, /send/],
    [/\b(issue|bug)/i, /issue/],
    [/\bdraft/i, /draft|document/],
  ];
  const named = verbs.flatMap(([inInput, inTool]) =>
    inInput.test(run.input) ? writes.filter((c) => inTool.test(c.call.tool)) : [],
  );
  const write = named[0] ?? pick(writes);
  if (write && wantsWrite) chosen.push(write);
  if (chosen.length === 0) {
    const any = pick(candidates);
    if (any) chosen.push(any);
  }

  steps.push({
    kind: "message",
    text: `I'll start with ${chosen[0]?.call.app_name ?? "the connected apps"} and work from there.`,
  });

  const parts: { call_id: ToolCallId; text: string }[] = [];
  for (const { call, approval } of chosen) {
    if (approval) steps.push({ kind: "approval", call });
    steps.push({ kind: "tool_call", call });
    steps.push({ kind: "tool_result", call, duration_ms: 300 + Math.floor(rng() * 1400) });
    parts.push({ call_id: call.call_id, text: SUMMARY[call.tool]?.(call.installation_label) ?? `ran ${call.tool}` });
  }
  steps.push({
    kind: "complete",
    parts,
    fallback: "I stopped there: the step that needed approval was denied, so nothing was changed.",
    closing: rng() < 0.5 ? "Everything I changed is linked above." : "Let me know if you want a different angle.",
  });
  return steps;
}

function pushEvent<T extends RunEventType>(
  db: Db,
  run: RunRow,
  at: string,
  type: T,
  data: Extract<RunEvent, { type: T }>["data"],
) {
  const key = runEventsKey(run);
  const list = db.run_events[key] ?? [];
  db.run_events[key] = list;
  list.push({
    id: newId("run_event", { at: Date.parse(at) }),
    organization_id: run.organization_id,
    workspace_id: run.workspace_id,
    run_id: run.id,
    // Counted within the run's own scope only.
    sequence: timelineOf(db, run).length + 1,
    created_at: at,
    type,
    data,
  } as RunEventRow);
}

function finish(db: Db, run: RunRow, at: string, error: RunError | null) {
  run.finished_at = at;
  run.plan = [];
  if (error) {
    run.status = "failed";
    run.error = error;
    pushEvent(db, run, at, "run.failed", { error });
  }
}

/** Record the queued event for a new run. */
export function enqueueRun(db: Db, run: RunRow) {
  pushEvent(db, run, run.created_at, "run.queued", {});
}

export type StepOutcome = "continue" | "paused" | "finished";

/**
 * Execute the next step. Returns `paused` when the run waits for a human and
 * `finished` when it reached a terminal state.
 */
export function advanceRun(db: Db, run: RunRow, at: string, rng: Rng): StepOutcome {
  const step = run.plan.shift();
  if (!step) return "finished";
  const agent = db.agents.find((a) => sameWorkspace(a, run) && a.id === run.agent_id);

  switch (step.kind) {
    case "start":
      run.status = "running";
      run.started_at = at;
      pushEvent(db, run, at, "run.started", { model: agent?.model ?? "unknown" });
      run.usage.input_tokens += 900 + Math.floor(rng() * 2600);
      return "continue";

    case "message":
      pushEvent(db, run, at, "message.assistant", { text: step.text });
      run.usage.output_tokens += 20 + Math.floor(rng() * 60);
      return "continue";

    case "approval": {
      // Nobody is asked about a call whose installation went: it's refused.
      const gone = goneFor(db, run, step.call);
      if (gone) {
        refuseCall(db, run, step.call, gone, at);
        return "continue";
      }
      run.status = "waiting_for_approval";
      pushEvent(db, run, at, "approval.requested", {
        call_id: step.call.call_id,
        installation_id: step.call.installation_id,
        installation_label: step.call.installation_label,
        app_name: step.call.app_name,
        tool: step.call.tool,
        arguments: step.call.arguments,
      });
      return "paused";
    }

    case "tool_call": {
      const { result: _result, error: _error, ...ref } = step.call;
      pushEvent(db, run, at, "tool_call.started", ref);
      run.usage.tool_calls += 1;
      return "continue";
    }

    case "tool_result": {
      const { arguments: _args, result, error: _error, ...ref } = step.call;
      // Re-check the grant's installation now, not at planning time.
      const installation = db.installations.find((i) => sameWorkspace(i, run) && i.id === step.call.installation_id);
      if (installation?.status !== "connected") {
        const error: RunError = {
          code: "installation_reconnect_required",
          message: `${step.call.installation_label} needs to be reconnected before its tools can run.`,
        };
        pushEvent(db, run, at, "tool_call.failed", { ...ref, error, duration_ms: step.duration_ms });
        finish(db, run, at, error);
        return "finished";
      }
      pushEvent(db, run, at, "tool_call.completed", {
        ...ref,
        result,
        duration_ms: step.duration_ms,
      });
      run.usage.input_tokens += 200 + Math.floor(rng() * 1800);
      return "continue";
    }

    case "complete": {
      const text =
        step.parts.length > 0
          ? `Done — I ${sentence(step.parts.map((p) => p.text))}.${step.closing ? ` ${step.closing}` : ""}`
          : step.fallback;
      pushEvent(db, run, at, "message.assistant", { text });
      run.usage.output_tokens += 120 + Math.floor(rng() * 700);
      run.output = text;
      run.status = "completed";
      run.finished_at = at;
      run.plan = [];
      pushEvent(db, run, at, "run.completed", { usage: { ...run.usage } });
      return "finished";
    }

    case "fail":
      finish(db, run, at, step.error);
      return "finished";
  }
}

/** How an installation went away (decision 0047): deleted, or disconnected. */
export type InstallationGone = "removed" | "disconnected";

/** Whether the call's installation went, read within the run's own workspace. */
function goneFor(db: Db, run: RunRow, call: PlannedCall): InstallationGone | null {
  const installation = db.installations.find((i) => sameWorkspace(i, run) && i.id === call.installation_id);
  if (!installation) return "removed";
  return installation.status === "disconnected" ? "disconnected" : null;
}

/**
 * Refuses a call whose installation went, never sent: its timeline shows why,
 * its steps leave the plan, and the run says it skipped it, as a denial does.
 */
function refuseCall(db: Db, run: RunRow, call: PlannedCall, gone: InstallationGone, at: string) {
  const error: RunError =
    gone === "removed"
      ? { code: "installation_removed", message: `${call.installation_label} was removed, so this call wasn't made.` }
      : {
          code: "installation_disconnected",
          message: `${call.installation_label} was disconnected, so this call wasn't made.`,
        };
  pushEvent(db, run, at, "tool_call.failed", {
    call_id: call.call_id,
    installation_id: call.installation_id,
    installation_label: call.installation_label,
    app_name: call.app_name,
    tool: call.tool,
    error,
    duration_ms: 0,
  });
  run.plan = run.plan.filter(
    (s) => !((s.kind === "tool_call" || s.kind === "tool_result") && s.call.call_id === call.call_id),
  );
  for (const s of run.plan) {
    if (s.kind === "complete") s.parts = s.parts.filter((p) => p.call_id !== call.call_id);
  }
  run.plan.unshift({ kind: "message", text: `Skipping ${call.tool} on ${call.installation_label} — ${error.message}` });
}

/**
 * Refuses the call each run of the workspace waits on for approval through
 * `installationId`, once it was deleted or disconnected (decision 0047): the
 * run moves on without anyone deciding, and a decision sent later answers
 * `409 not_waiting`. The backend does this in a job the installation's change
 * queues; the mock at once. Only runs of exactly this organization and
 * workspace are read. The runs that moved on, to schedule.
 */
export function refuseWaitingOn(
  db: Db,
  scope: { organization_id: RunRow["organization_id"]; workspace_id: RunRow["workspace_id"] },
  installationId: PlannedCall["installation_id"],
  gone: InstallationGone,
  at: string,
): RunRow[] {
  const moved: RunRow[] = [];
  for (const run of db.runs) {
    if (!sameWorkspace(run, scope) || run.status !== "waiting_for_approval") continue;
    const events = timelineOf(db, run);
    const requested = [...events].reverse().find((event) => event.type === "approval.requested");
    if (requested?.type !== "approval.requested" || requested.data.installation_id !== installationId) continue;
    const callId = requested.data.call_id;
    if (events.some((event) => event.type === "approval.resolved" && event.data.call_id === callId)) continue;
    const step = run.plan.find((s) => s.kind === "tool_call" && s.call.call_id === callId);
    if (step?.kind !== "tool_call") continue;
    refuseCall(db, run, step.call, gone, at);
    run.status = "running";
    moved.push(run);
  }
  return moved;
}

/** A run as the audit log names it: by its agent, within the run's own workspace. */
export function runTarget(db: Db, run: RunRow) {
  const agent = db.agents.find((a) => sameWorkspace(a, run) && a.id === run.agent_id);
  return { type: "run", id: run.id, name: agent?.name ?? "Agent" };
}

/**
 * Apply a human decision to a run waiting for approval. The customer's
 * backend records it with an API key, so the key is the audit log's actor.
 */
export function resolveApproval(
  db: Db,
  run: RunRow,
  callId: ToolCallId,
  decision: "approved" | "denied",
  apiKeyId: ApiKeyId,
  at: string,
  reason?: string,
) {
  pushEvent(db, run, at, "approval.resolved", {
    call_id: callId,
    decision,
    api_key_id: apiKeyId,
  });
  const key = db.api_keys.find((k) => k.organization_id === run.organization_id && k.id === apiKeyId);
  const workspace = db.workspaces.find((w) => w.organization_id === run.organization_id && w.id === run.workspace_id);
  db.audit.unshift({
    id: newId("audit_event", { at: Date.parse(at) }),
    organization_id: run.organization_id,
    workspace_id: run.workspace_id,
    environment: workspace?.environment ?? null,
    actor: { type: "api_key", id: apiKeyId, name: key?.name ?? "API key" },
    action: "approval.resolved",
    target: runTarget(db, run),
    created_at: at,
  });
  run.status = "running";
  if (decision === "denied") {
    const skipped = run.plan.find((s) => s.kind === "tool_call" && s.call.call_id === callId);
    run.plan = run.plan.filter(
      (s) => !((s.kind === "tool_call" || s.kind === "tool_result") && s.call.call_id === callId),
    );
    for (const s of run.plan) {
      if (s.kind === "complete") s.parts = s.parts.filter((p) => p.call_id !== callId);
    }
    if (skipped && skipped.kind === "tool_call") {
      run.plan.unshift({
        kind: "message",
        // A denial's reason goes back to the model, which works with it.
        text: reason?.trim()
          ? `Skipping ${skipped.call.tool} on ${skipped.call.installation_label} — the request was denied: ${reason.trim()}`
          : `Skipping ${skipped.call.tool} on ${skipped.call.installation_label} — the request was denied.`,
      });
    }
  }
}

/** Cancel a non-terminal run. */
export function cancelRun(db: Db, run: RunRow, cancelledBy: string, at: string) {
  run.status = "cancelled";
  run.finished_at = at;
  run.plan = [];
  pushEvent(db, run, at, "run.cancelled", { cancelled_by: cancelledBy });
}
