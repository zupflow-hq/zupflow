/**
 * Simulated agent execution. A run is planned up front from the agent's
 * current grants, then advanced one step at a time: live runs on timers, seeded
 * runs instantly against a fake clock. Every step re-checks the installation,
 * so revoking access or a broken connection shows up the way it will for real.
 *
 * A run asked to remember something saves it with the agent's memory tool,
 * in the memory type it resolved when it was planned. That call is decided
 * when it's made, on the type as it is then, as the backend decides it: a
 * run started with memory off, a type deleted or made read-only since, are
 * refused with the backend's fixed messages; a type that asks for approval
 * pauses the run. The mock sends no prompt, so it has no recall to show.
 */
import type { ApiKeyId, RunError, RunEvent, RunEventType, ToolCallId } from "@zupflow/sdk";
import { sampleToolCall } from "./catalog";
import { newId } from "./ids";
import { memoryTypeNow, runMemoryOf, saveFromRun } from "./memory";
import type { Db, PlannedCall, PlannedMemoryCall, RunEventRow, RunRow, RunStep } from "./model";
import { runRoute } from "./model-scope";
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

/** What a refused memory call reads back, as the backend refuses it. */
const MEMORY_OFF: RunError = { code: "memory_off", message: "Memory is off for this run." };
const MEMORY_TYPE_REMOVED: RunError = {
  code: "memory_type_removed",
  message: "This run's memory was deleted, so this call wasn't made.",
};
const MEMORY_NOT_SERVED: RunError = {
  code: "tool_refused",
  message: "This run's memory can't be reached on this server, so this call wasn't made.",
};
const MEMORY_READ_ONLY: RunError = {
  code: "tool_refused",
  message: "This agent's memory is read-only now, so it can't be changed.",
};
/** A kind's own refusal is for the model; the timeline says only that it refused. */
const TOOL_ERROR: RunError = { code: "tool_error", message: "The tool reported an error." };

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

  const remember = rememberCall(db, run, rng, at);

  if (candidates.length === 0 && remember) {
    steps.push(
      { kind: "message", text: "I'll keep that in mind." },
      { kind: "memory_call", call: remember, duration_ms: 40 + Math.floor(rng() * 160), approved: false },
      {
        kind: "complete",
        parts: [{ call_id: remember.call_id, text: "saved that to memory" }],
        fallback: "I couldn't save that to memory, so nothing was saved.",
        closing: "",
      },
    );
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
  if (remember) {
    steps.push({ kind: "memory_call", call: remember, duration_ms: 40 + Math.floor(rng() * 160), approved: false });
    parts.push({ call_id: remember.call_id, text: "saved that to memory" });
  }
  steps.push({
    kind: "complete",
    parts,
    fallback: "I stopped there: the step that needed approval was denied, so nothing was changed.",
    closing: rng() < 0.5 ? "Everything I changed is linked above." : "Let me know if you want a different angle.",
  });
  return steps;
}

/**
 * The save a request to remember something asks for, in the memory type the
 * run's agent has now: none without one this server serves, or when the
 * request doesn't ask. Its text is what follows "remember".
 */
function rememberCall(
  db: Db,
  run: Pick<RunRow, "organization_id" | "workspace_id" | "agent_id" | "input">,
  rng: Rng,
  at: number,
): PlannedMemoryCall | null {
  const asked = /\bremember\b(?:\s+that\b)?[\s:,]*(.*)$/is.exec(run.input);
  if (!asked) return null;
  const agent = db.agents.find((a) => sameWorkspace(a, run) && a.id === run.agent_id);
  const memory = agent && runMemoryOf(db, agent);
  if (!memory) return null;
  const said = (asked[1] ?? "").replace(/\s+/g, " ").trim() || run.input.replace(/\s+/g, " ").trim();
  const text = `${said.charAt(0).toUpperCase()}${said.slice(1)}`.replace(/(?<![.!?])$/, ".").slice(0, 500);
  return {
    call_id: newId("tool_call", { rng, at }),
    memory_type_id: memory.type.id,
    memory_type_name: memory.type.name,
    kind: memory.type.kind,
    tool: "save_memory",
    arguments: { text },
  };
}

/** A memory call as the run's events name it. */
function memoryRef(call: PlannedMemoryCall) {
  return {
    call_id: call.call_id,
    memory_type_id: call.memory_type_id,
    memory_type_name: call.memory_type_name,
    kind: call.kind,
    tool: call.tool,
  };
}

/** Whether the run had read an app's result before now: its origin says so of what it saves. */
function readAppContent(db: Db, run: RunRow): boolean {
  return timelineOf(db, run).some(
    (e) =>
      (e.type === "tool_call.completed" || (e.type === "tool_call.failed" && e.data.error.code === TOOL_ERROR.code)) &&
      "installation_id" in e.data,
  );
}

/** The call a step makes, when it makes one. */
function callOf(step: RunStep): PlannedCall | PlannedMemoryCall | null {
  return step.kind === "tool_call" || step.kind === "tool_result" || step.kind === "memory_call" ? step.call : null;
}

/** Leaves a call that wasn't made out of the run's result. */
function dropPart(run: RunRow, callId: ToolCallId) {
  for (const s of run.plan) {
    if (s.kind === "complete") s.parts = s.parts.filter((p) => p.call_id !== callId);
  }
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
 * Why the run's model can't be called when it starts, if it can't: the model
 * or its connection was removed, or the provider refused the connection's key.
 */
function modelRefusal(db: Db, run: RunRow): RunError | null {
  const route = runRoute(db, run);
  if (!route) {
    return { code: "model_unavailable", message: "The run's model was removed before it started." };
  }
  if (route.connection.status === "invalid_key") {
    return {
      code: "model_key_invalid",
      message: `The model provider refused ${route.connection.label}'s key. Replace it under Model providers.`,
    };
  }
  return null;
}

/**
 * One model turn that reads `fresh` tokens after everything the run's previous
 * turn read. The provider serves that prefix from its prompt cache; Claude
 * (through Anthropic, or OpenRouter) also reports what it writes to the cache,
 * where OpenAI and OpenRouter's other models cache on their own and report no
 * writes. `input_tokens` counts all of it.
 */
function modelTurn(db: Db, run: RunRow, fresh: number): void {
  const cached = run.prompt_tokens;
  run.usage.input_tokens += cached + fresh;
  run.usage.cache_read_input_tokens += cached;
  if (writesCache(db, run)) run.usage.cache_write_input_tokens += fresh;
  run.prompt_tokens = cached + fresh;
}

/** Whether the run's provider reports the tokens it writes to its prompt cache. */
function writesCache(db: Db, run: RunRow): boolean {
  const kind = runRoute(db, run)?.connection.kind;
  return kind === "anthropic" || (kind === "openrouter" && run.model.provider_model_id.startsWith("anthropic/"));
}

/**
 * Execute the next step. Returns `paused` when the run waits for a human and
 * `finished` when it reached a terminal state.
 */
export function advanceRun(db: Db, run: RunRow, at: string, rng: Rng): StepOutcome {
  const step = run.plan.shift();
  if (!step) return "finished";

  switch (step.kind) {
    case "start": {
      run.status = "running";
      run.started_at = at;
      // The model recorded when the run was created, never the default as it is now.
      pushEvent(db, run, at, "run.started", {
        model: run.model.provider_model_id,
        model_id: run.model.model_id,
        connection_id: run.model.connection_id,
      });
      const refusal = modelRefusal(db, run);
      if (refusal) {
        run.plan = [];
        finish(db, run, at, refusal);
        return "finished";
      }
      run.model.key_version = runRoute(db, run)?.connection.key_version ?? null;
      modelTurn(db, run, 900 + Math.floor(rng() * 2600));
      return "continue";
    }

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
      modelTurn(db, run, 200 + Math.floor(rng() * 1800));
      return "continue";
    }

    case "memory_call": {
      const ref = memoryRef(step.call);
      const refuse = (error: RunError): StepOutcome => {
        pushEvent(db, run, at, "tool_call.failed", { ...ref, error, duration_ms: 0 });
        dropPart(run, step.call.call_id);
        return "continue";
      };
      // Decided now, on the type as it is: the tools the model read stay as they were.
      if (run.memory === "off") return refuse(MEMORY_OFF);
      const now = memoryTypeNow(db, run.organization_id, step.call.memory_type_id);
      if (now === "gone") return refuse(MEMORY_TYPE_REMOVED);
      if (now === "not_served") return refuse(MEMORY_NOT_SERVED);
      if (now.type.writes === "deny") return refuse(MEMORY_READ_ONLY);
      if (now.type.writes === "approve" && !step.approved) {
        run.status = "waiting_for_approval";
        pushEvent(db, run, at, "approval.requested", { ...ref, arguments: step.call.arguments });
        run.plan.unshift({ ...step, approved: true });
        return "paused";
      }
      pushEvent(db, run, at, "tool_call.started", { ...ref, arguments: step.call.arguments });
      run.usage.tool_calls += 1;
      const answer = saveFromRun(db, run, now, step.call, readAppContent(db, run), at);
      if (answer === null) {
        pushEvent(db, run, at, "tool_call.failed", { ...ref, error: TOOL_ERROR, duration_ms: step.duration_ms });
        dropPart(run, step.call.call_id);
      } else {
        pushEvent(db, run, at, "tool_call.completed", {
          ...ref,
          result: { result: answer },
          duration_ms: step.duration_ms,
        });
      }
      modelTurn(db, run, 100 + Math.floor(rng() * 400));
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
  run.plan = run.plan.filter((s) => callOf(s)?.call_id !== call.call_id);
  dropPart(run, call.call_id);
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
    if (
      requested?.type !== "approval.requested" ||
      !("installation_id" in requested.data) ||
      requested.data.installation_id !== installationId
    )
      continue;
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
    const skipped = run.plan.find(
      (s) => (s.kind === "tool_call" || s.kind === "memory_call") && s.call.call_id === callId,
    );
    run.plan = run.plan.filter((s) => callOf(s)?.call_id !== callId);
    dropPart(run, callId);
    if (skipped && (skipped.kind === "tool_call" || skipped.kind === "memory_call")) {
      const where =
        skipped.kind === "tool_call" ? `on ${skipped.call.installation_label}` : `in ${skipped.call.memory_type_name}`;
      run.plan.unshift({
        kind: "message",
        // A denial's reason goes back to the model, which works with it.
        text: reason?.trim()
          ? `Skipping ${skipped.call.tool} ${where} — the request was denied: ${reason.trim()}`
          : `Skipping ${skipped.call.tool} ${where} — the request was denied.`,
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
