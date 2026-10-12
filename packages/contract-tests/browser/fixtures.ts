import { type ChildProcess, spawn } from "node:child_process";
import { createHash, X509Certificate } from "node:crypto";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { type BrowserContext, test as base, chromium, type Page } from "@playwright/test";
import {
  type AppId,
  type ConnectSessionId,
  type CreateConnectSessionInput,
  createClient,
  createHttpTransport,
  type Environment,
  type OrganizationId,
  type WorkspaceId,
  type ZupflowClient,
} from "@zupflow/sdk";
// The package barrel also exports the Vitest suite. Load only the shared
// slug-based reference fixture here, without importing Vitest into Playwright.
import { type Fixture, loadFixture, must } from "../src/fixture";

type Mode = "token" | "oauth";
type Persona = "owner" | "outsider";
const root = fileURLToPath(new URL("../../../", import.meta.url));
const binary = join(root, "server/target/debug/zupflow-contract-server");
const taskboardBinary = join(root, "server/target/debug/taskboard");
const closedChildren = new WeakSet<ChildProcess>();

async function command(args: string[]): Promise<number> {
  return new Promise((resolve) => {
    const child = spawn(binary, args, { stdio: "ignore" });
    child.once("error", () => resolve(-1));
    child.once("close", (code) => resolve(code ?? -1));
  });
}

async function stop(child: ChildProcess) {
  if (!child.pid || closedChildren.has(child) || child.exitCode !== null || child.signalCode !== null) return;
  await new Promise<void>((resolve) => {
    const timer = setTimeout(() => child.kill("SIGKILL"), 2_000);
    child.once("close", () => {
      clearTimeout(timer);
      resolve();
    });
    child.kill("SIGTERM");
  });
}

async function serve(executable: string, args: string[], children: ChildProcess[]): Promise<number> {
  const child = spawn(executable, args, { stdio: ["ignore", "pipe", "pipe"] });
  child.once("close", () => {
    closedChildren.add(child);
  });
  children.push(child);
  child.stderr.resume();
  return new Promise((resolve, reject) => {
    let pending = "";
    let done = false;
    const timer = setTimeout(() => fail(), 15_000);
    function fail() {
      if (done) return;
      done = true;
      clearTimeout(timer);
      reject(new Error("A disposable browser fixture could not start."));
    }
    child.once("error", fail);
    child.once("exit", fail);
    child.stdout.on("data", (chunk: Buffer) => {
      if (done) return;
      // Do not retain or print provider output, even when startup fails.
      pending = (pending + chunk.toString("utf8")).slice(-4096);
      const match = /(?:^|\n)listening ([0-9]{1,5})(?:\r?\n)/.exec(pending);
      if (!match?.[1]) return;
      const port = Number(match[1]);
      if (port < 1 || port > 65535) {
        fail();
        return;
      }
      done = true;
      pending = "";
      clearTimeout(timer);
      resolve(port);
    });
  });
}

export interface Stack {
  baseURL: string;
  fixture: Fixture;
  owner: ZupflowClient;
  outsider: ZupflowClient;
  anonymous: ZupflowClient;
  taskboard: { origin: string; app: AppId; spki: string } | null;
  member(organization: OrganizationId | null, persona?: Persona): ZupflowClient;
  key(organization: OrganizationId, environment: Environment): Promise<ZupflowClient>;
  session(
    organization: OrganizationId,
    workspace: WorkspaceId,
    input: CreateConnectSessionInput,
  ): ReturnType<ZupflowClient["connectSessions"]["create"]>;
  expire(organization: OrganizationId, workspace: WorkspaceId, session: ConnectSessionId): Promise<number>;
  malformedExpiry(): Promise<number>;
}

async function start(mode: Mode): Promise<{ stack: Stack; dispose(): Promise<void> }> {
  const runDirectory = process.env.ZUPFLOW_BROWSER_RUN_DIR;
  if (!runDirectory) throw new Error("Use the hosted Connect browser runner.");
  const directory = await mkdtemp(join(runDirectory, "fixture-"));
  const database = join(directory, "contract.sqlite");
  const children: ChildProcess[] = [];
  const dispose = async () => {
    for (const child of children.reverse()) await stop(child);
    await rm(directory, { recursive: true, force: true });
  };
  try {
    if ((await command(["reset", database])) !== 0) throw new Error("Reference fixture reset failed.");
    const args = ["serve", database];
    let board: { origin: string; spki: string } | null = null;
    if (mode === "oauth") {
      const certificate = join(directory, "taskboard.pem");
      const port = await serve(taskboardBinary, ["--port", "0", "--certificate", certificate], children);
      const publicKey = new X509Certificate(await readFile(certificate)).publicKey;
      const spki = createHash("sha256")
        .update(publicKey.export({ type: "spki", format: "der" }))
        .digest("base64");
      board = { origin: `https://taskboard.example:${port}`, spki };
      args.push("--local-app", `taskboard.example:${port}=${certificate}`);
    }
    const port = await serve(binary, args, children);
    const baseURL = `http://localhost:${port}`;
    const client = (organization: OrganizationId | null, token: string | null) =>
      createClient(
        createHttpTransport({
          baseUrl: baseURL,
          getToken: async () => token,
          getOrganization: () => organization,
          retries: 0,
        }),
      );
    const member = (organization: OrganizationId | null, persona: Persona = "owner") =>
      client(organization, `contract:${persona}`);
    const fixture = await loadFixture(member, member(null, "outsider"));
    const owner = member(fixture.organization);
    await owner.connectSettings.update(fixture.organization, "sandbox", { return_urls: [], allowed_origins: [] });
    let taskboard: Stack["taskboard"] = null;
    if (board) {
      const app = await owner.organizationApps.create(fixture.organization, {
        kind: "mcp",
        name: "Taskboard",
        key: "taskboard_live",
        description: "Tasks for local browser tests.",
        auth_type: "mcp_oauth",
      });
      await owner.organizationApps.update(fixture.organization, app.id, {
        auth: {
          type: "mcp_oauth",
          scopes: [
            { name: "tasks:read", description: "See tasks" },
            { name: "tasks:write", description: "Change tasks" },
          ],
          identity_url: `${board.origin}/me`,
        },
      });
      await owner.organizationApps.updateConfiguration(fixture.organization, app.id, "sandbox", {
        mcp_server_url: `${board.origin}/mcp`,
      });
      taskboard = { ...board, app: app.id };
    }
    const stack: Stack = {
      baseURL,
      fixture,
      owner,
      member,
      taskboard,
      outsider: member(fixture.otherOrganization, "outsider"),
      anonymous: client(null, null),
      async key(organization, environment) {
        const manager = member(organization, organization === fixture.organization ? "owner" : "outsider");
        const created = await manager.apiKeys.create(organization, { name: "Browser journey", environment });
        // Held only in this server-side fixture's closure, never in browser headers.
        return client(null, created.secret);
      },
      session: (organization, workspace, input) => member(organization).connectSessions.create(workspace, input),
      expire: (organization, workspace, session) =>
        command(["expire-connect", database, organization, workspace, session]),
      malformedExpiry: () => command(["expire-connect", database, "invalid", "invalid", "invalid"]),
    };
    return { stack, dispose };
  } catch {
    await dispose();
    throw new Error("Hosted Connect fixture setup failed.");
  }
}

export interface Flow {
  page: Page;
  context: BrowserContext;
  /** Permit subsequent frame-ancestors violations for an intentional framing refusal. */
  expectFrameRefusal(): void;
}

export const test = base.extend<{ mode: Mode; stack: Stack; flow: Flow }>({
  mode: ["token", { option: true }],
  stack: async ({ mode }, use) => {
    const instance = await start(mode);
    try {
      await use(instance.stack);
    } finally {
      await instance.dispose();
    }
  },
  flow: async ({ stack }, use, testInfo) => {
    const args = ["--no-proxy-server"];
    if (stack.taskboard) {
      args.push(
        "--host-resolver-rules=MAP taskboard.example 127.0.0.1",
        `--ignore-certificate-errors-spki-list=${stack.taskboard.spki}`,
      );
    }
    const browser = await chromium.launch({ args });
    let scriptErrors = 0;
    let unexpectedCspErrors = 0;
    let leakedBrowserCredentials = false;
    let expectedFrameRefusal = false;
    try {
      const context = await browser.newContext({
        viewport: testInfo.project.use.viewport,
        colorScheme: testInfo.project.use.colorScheme,
        reducedMotion: "reduce",
        serviceWorkers: "block",
      });
      context.setDefaultTimeout(10_000);
      context.setDefaultNavigationTimeout(15_000);
      context.on("request", (request) => {
        const headers = request.headers();
        if (headers.authorization || headers.referer?.startsWith(`${stack.baseURL}/connect/`)) {
          leakedBrowserCredentials = true;
        }
      });
      await context.exposeBinding("recordConnectPolicyViolation", (_source, effectiveDirective: unknown) => {
        if (expectedFrameRefusal && effectiveDirective === "frame-ancestors") return;
        unexpectedCspErrors++;
      });
      await context.addInitScript(() => {
        document.addEventListener("securitypolicyviolation", (event) => {
          const reporter = window as unknown as {
            recordConnectPolicyViolation(effectiveDirective: string): Promise<void>;
          };
          // Never send the event's URL, blocked resource or source sample to the runner.
          void reporter.recordConnectPolicyViolation(event.effectiveDirective);
        });
      });
      context.on("page", (page) =>
        page.on("pageerror", () => {
          scriptErrors++;
        }),
      );
      const page = await context.newPage();
      await use({
        page,
        context,
        expectFrameRefusal: () => {
          expectedFrameRefusal = true;
        },
      });
      await base.step("browser-script-errors", () => {
        if (scriptErrors) throw new Error("Hosted Connect raised a script error.");
      });
      await base.step("browser-csp-errors", () => {
        if (unexpectedCspErrors) throw new Error("Hosted Connect raised a security-policy error.");
      });
      await base.step("browser-credential-leak", () => {
        if (leakedBrowserCredentials) throw new Error("Hosted Connect exposed a browser credential.");
      });
    } finally {
      await browser.close();
    }
  },
});

export { expect } from "@playwright/test";
export { must };

export function createInput(app: AppId, label = "Browser connection"): CreateConnectSessionInput {
  return { app_id: app, intent: "create", installation_id: null, return_url: null, label };
}

export function reference(url: string): string {
  return must(new URL(url).pathname.split("/").at(-1), "Connect reference");
}
