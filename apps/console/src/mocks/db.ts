/**
 * The mock API's storage: one in-memory database, persisted to localStorage so
 * the demo survives a reload, plus the timers that advance live runs.
 */
import type { Db, RunRow, RunScope } from "./model";
import { rngFor } from "./random";
import { advanceRun } from "./runner";
import { DB_SCHEMA, seed } from "./seed";

const STORAGE_KEY = "zupflow.mock.db";

let current: Db | null = null;

// Connect runs in its own window with its own copy of this store. When another
// tab writes, drop ours so the next request reads the shared state.
if (typeof window !== "undefined") {
  window.addEventListener("storage", (event) => {
    if (event.key === STORAGE_KEY) current = null;
  });
}

function load(): Db | null {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Db;
    return parsed.schema === DB_SCHEMA ? parsed : null;
  } catch {
    return null;
  }
}

export function db(): Db {
  if (!current) {
    current = load() ?? seed();
    persist();
  }
  return current;
}

export function persist() {
  if (!current) return;
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(current));
  } catch {
    // Storage full or blocked: the demo keeps working in memory.
  }
}

export function resetDb() {
  for (const timer of timers.values()) clearTimeout(timer);
  timers.clear();
  current = seed();
  persist();
}

// ---------------------------------------------------------------------------
// Live run scheduling

/** Timers keyed by the run's full scope, so cancelling needs the run's organization and workspace too. */
const timers = new Map<string, ReturnType<typeof setTimeout>>();
const timerKey = (scope: RunScope) => `${scope.organization_id}/${scope.workspace_id}/${scope.id}`;

function delayFor(run: RunRow): number {
  const next = run.plan[0];
  switch (next?.kind) {
    case "start":
      return 700;
    case "message":
      return 1100;
    case "tool_call":
      return 900;
    case "tool_result":
      return Math.min(next.duration_ms, 1600);
    case "complete":
      return 1400;
    default:
      return 800;
  }
}

const findRun = (scope: RunScope) =>
  db().runs.find(
    (r) => r.organization_id === scope.organization_id && r.workspace_id === scope.workspace_id && r.id === scope.id,
  );

/** Advance a live run until it pauses or finishes. */
export function scheduleRun(scope: RunScope): boolean {
  const key = timerKey(scope);
  const existing = timers.get(key);
  if (existing) clearTimeout(existing);
  timers.delete(key);
  const run = findRun(scope);
  if (!run || run.plan.length === 0) return false;

  const rng = rngFor(`${scope.id}:${run.plan.length}`);
  timers.set(
    key,
    setTimeout(() => {
      timers.delete(key);
      const live = findRun(scope);
      if (!live || live.status === "cancelled" || live.status === "waiting_for_approval") return;
      const outcome = advanceRun(db(), live, new Date().toISOString(), rng);
      persist();
      if (outcome === "continue") scheduleRun(scope);
    }, delayFor(run)),
  );
  return true;
}

/** Whether this exact run, in this scope, has a timer: for tests and diagnostics. */
export function isRunScheduled(scope: RunScope): boolean {
  return timers.has(timerKey(scope));
}

export function cancelScheduledRun(scope: RunScope) {
  const key = timerKey(scope);
  const timer = timers.get(key);
  if (timer) clearTimeout(timer);
  timers.delete(key);
}

/**
 * Cross-tenant by design, like a backend worker claiming work: picks up runs
 * of every organization that were mid-flight when the page was last closed.
 * It reads only each run's scope; everything after is scoped to that run's
 * organization and workspace.
 */
export function resumeLiveRunsCrossTenant() {
  for (const run of db().runs) {
    if ((run.status === "queued" || run.status === "running") && run.plan.length > 0) {
      scheduleRun({ id: run.id, organization_id: run.organization_id, workspace_id: run.workspace_id });
    }
  }
}
