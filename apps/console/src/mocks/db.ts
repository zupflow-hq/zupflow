/**
 * The mock API's storage: one in-memory database, persisted to localStorage so
 * the demo survives a reload, plus the timers that advance live runs.
 */
import type { Db, RunRow } from "./model";
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

const timers = new Map<string, ReturnType<typeof setTimeout>>();

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

/** Advance a live run until it pauses or finishes. */
export function scheduleRun(runId: string) {
  const existing = timers.get(runId);
  if (existing) clearTimeout(existing);
  const run = db().runs.find((r) => r.id === runId);
  if (!run || run.plan.length === 0) return;

  const rng = rngFor(`${runId}:${run.plan.length}`);
  timers.set(
    runId,
    setTimeout(() => {
      timers.delete(runId);
      const live = db().runs.find((r) => r.id === runId);
      if (!live || live.status === "cancelled" || live.status === "waiting_for_approval") return;
      const outcome = advanceRun(db(), live, new Date().toISOString(), rng);
      persist();
      if (outcome === "continue") scheduleRun(runId);
    }, delayFor(run)),
  );
}

export function cancelScheduledRun(runId: string) {
  const timer = timers.get(runId);
  if (timer) clearTimeout(timer);
  timers.delete(runId);
}

/** Pick up runs that were mid-flight when the page was last closed. */
export function resumeLiveRuns() {
  for (const run of db().runs) {
    if ((run.status === "queued" || run.status === "running") && run.plan.length > 0) {
      scheduleRun(run.id);
    }
  }
}
