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
 * `ZUPFLOW_CONTRACT_OPERATIONS`, optional, lists the operations the server
 * serves (comma-separated `operationId`s); a case that needs another is
 * skipped and names it.
 *
 * `ZUPFLOW_CONTRACT_CHANNEL_APP_SECRETS_COMMAND`, optional, is a shell
 * command that prints what the channel provider holds for the app whose ID
 * replaces `{app}`, as JSON (`inbound_ref`, `signing_secret`); without it,
 * cases that deliver provider events are skipped.
 *
 * Without `ZUPFLOW_CONTRACT_URL` the suite is skipped; the mock runs it
 * in-process from the Console's tests. With it, every other variable but
 * `ZUPFLOW_CONTRACT_OPERATIONS` is required.
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
  const operations = process.env.ZUPFLOW_CONTRACT_OPERATIONS?.split(",")
    .map((id) => id.trim())
    .filter((id) => id !== "");
  const secretsCommand = process.env.ZUPFLOW_CONTRACT_CHANNEL_APP_SECRETS_COMMAND;
  contractSuite({
    baseUrl: url,
    operations,
    channelAppSecrets: secretsCommand
      ? async (app) => {
          if (!/^chapp_[0-9A-Za-z]+$/.test(app)) throw new Error(`not a channel app ID: ${app}`);
          return JSON.parse(execSync(secretsCommand.replaceAll("{app}", app), { encoding: "utf8" }));
        }
      : undefined,
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
