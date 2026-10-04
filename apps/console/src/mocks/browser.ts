import { setupWorker } from "msw/browser";
import { resumeLiveRunsCrossTenant } from "./db";
import { createHandlers } from "./handlers";

/** Start the in-browser mock API. Resolves once requests are intercepted. */
export async function startMockApi(apiBase: string) {
  const worker = setupWorker(...createHandlers(apiBase));
  await worker.start({
    onUnhandledFrame: "bypass",
    quiet: true,
    serviceWorker: { url: `${import.meta.env.BASE_URL}mockServiceWorker.js` },
  });
  resumeLiveRunsCrossTenant();
  return worker;
}
