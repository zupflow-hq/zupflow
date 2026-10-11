import { createServer } from "node:http";
import type { Page } from "@playwright/test";
import { expect, test } from "./fixtures";

// Static downstream-product fixture: its server never receives a secret or
// puts a Connect URL in its served HTML, logs or query string. The test sets
// the target in browser memory; the browser then opens it or creates a frame.
const document = `<!doctype html><html lang="en"><head><meta name="viewport" content="width=device-width,initial-scale=1"><title>Connect fixture</title><link rel="stylesheet" href="/host.css"></head><body><h1>Connect fixture</h1><button id="popup">Open Connect</button><button id="frame">Embed Connect</button><div id="frame-host"></div><script src="/host.js"></script></body></html>`;
const script = `
window.receivedMessages = [];
window.addEventListener("message", (event) => {
  if (event.origin === window.connectOrigin) window.receivedMessages.push(event.data);
});
document.getElementById("popup").addEventListener("click", () => {
  window.open(window.connectURL, "connect", "popup,width=440,height=740");
});
document.getElementById("frame").addEventListener("click", () => {
  const frame = document.createElement("iframe");
  frame.title = "Connect account";
  frame.src = window.connectURL;
  document.getElementById("frame-host").replaceChildren(frame);
});`;

export async function embedder() {
  const server = createServer((request, response) => {
    response.setHeader("Cache-Control", "no-store");
    response.setHeader("Referrer-Policy", "no-referrer");
    response.setHeader(
      "Content-Security-Policy",
      "default-src 'none'; script-src 'self'; style-src 'self'; frame-src http://localhost:*; base-uri 'none'",
    );
    const pathname = new URL(request.url ?? "/", "http://localhost").pathname;
    if (pathname === "/host.js") {
      response.setHeader("Content-Type", "text/javascript");
      response.end(script);
    } else if (pathname === "/host.css") {
      response.setHeader("Content-Type", "text/css");
      response.end(
        "iframe { width:100%; height:650px; border:0 } body { margin:8px; font-family:system-ui } button { min-height:44px }",
      );
    } else {
      response.setHeader("Content-Type", "text/html");
      response.end(
        pathname === "/return"
          ? "<!doctype html><html><title>Returned</title><body><h1>Back in the app</h1></body></html>"
          : document,
      );
    }
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("Embedder fixture did not bind a port.");
  return {
    origin: `http://localhost:${address.port}`,
    async close() {
      server.closeAllConnections();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

export async function prepare(page: Page, origin: string, connectURL: string, connectOrigin: string) {
  await page.goto(origin);
  await page.evaluate(
    (values) => {
      Object.assign(window, values);
    },
    { connectURL, connectOrigin },
  );
}

export async function messageCount(page: Page) {
  return page.evaluate(() => (window as unknown as { receivedMessages: unknown[] }).receivedMessages.length);
}

export async function finishedMessage(page: Page, session: string, status: string) {
  await expect.poll(() => messageCount(page)).toBe(1);
  const safe = await page.evaluate(
    ({ session, status }) => {
      const messages = (window as unknown as { receivedMessages: unknown[] }).receivedMessages;
      const message = messages[0];
      if (typeof message !== "object" || message === null) return false;
      const value = message as Record<string, unknown>;
      return (
        Object.keys(value).sort().join(",") === "session_id,status,type" &&
        value.type === "zupflow.connect.finished" &&
        value.session_id === session &&
        value.status === status
      );
    },
    { session, status },
  );
  await test.step("message-payload", () => expect(safe).toBe(true));
}
