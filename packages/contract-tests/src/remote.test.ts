/**
 * Runs the contract suite against a live server, such as the backend:
 *
 *   ZUPFLOW_CONTRACT_URL=http://localhost:8080 \
 *   ZUPFLOW_CONTRACT_RESET_COMMAND="…restores the reference fixture…" \
 *   ZUPFLOW_CONTRACT_TOKEN_OWNER=… ZUPFLOW_CONTRACT_TOKEN_DEVELOPER=… \
 *   ZUPFLOW_CONTRACT_TOKEN_VIEWER=… ZUPFLOW_CONTRACT_TOKEN_OUTSIDER=… pnpm test --filter @zupflow/contract-tests
 *
 * The reset is a shell command run before every test, not an HTTP route: the
 * server under test never exposes a way to wipe its data. Turbo passes these
 * variables through (`turbo.json`) and keys its cache on them.
 *
 * Without `ZUPFLOW_CONTRACT_URL` the suite is skipped; the mock runs it
 * in-process from the Console's tests. With it, every other variable is
 * required.
 */
import { execSync } from "node:child_process";
import { describe, it } from "vitest";
import { contractSuite } from "./suite";
import type { Persona } from "./target";

const env = (name: string) => {
  const value = process.env[name];
  if (!value) throw new Error(`${name} is required to run the contract suite against a server`);
  return value;
};

const url = process.env.ZUPFLOW_CONTRACT_URL;
if (url) {
  const tokens: Record<Persona, string> = {
    owner: env("ZUPFLOW_CONTRACT_TOKEN_OWNER"),
    developer: env("ZUPFLOW_CONTRACT_TOKEN_DEVELOPER"),
    viewer: env("ZUPFLOW_CONTRACT_TOKEN_VIEWER"),
    outsider: env("ZUPFLOW_CONTRACT_TOKEN_OUTSIDER"),
  };
  const reset = env("ZUPFLOW_CONTRACT_RESET_COMMAND");
  contractSuite({
    baseUrl: url,
    token: async (persona) => tokens[persona],
    reset: async () => {
      execSync(reset, { stdio: "inherit" });
    },
  });
} else {
  describe.skip("the contract suite against a server (set ZUPFLOW_CONTRACT_URL to run)", () => {
    it("needs a server", () => {});
  });
}
