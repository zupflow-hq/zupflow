/**
 * The mock's simulated sign-in, for the in-browser mock API only. Loaded
 * dynamically, and only when `__ZUPFLOW_MOCK__` is true, so no production
 * build contains it or the mock it starts.
 */

import { config } from "@/lib/env";
import { signUp } from "@/mocks/accounts";
import { startMockApi } from "@/mocks/browser";
import { resetDb } from "@/mocks/db";
import { MOCK_SESSION_KEY } from "@/mocks/http";
import { DEMO_USER_ID } from "@/mocks/seed";
import type { AuthAdapter } from "./adapter";

function read(): string | null {
  try {
    return localStorage.getItem(MOCK_SESSION_KEY);
  } catch {
    return null;
  }
}

export async function createMockAdapter(changed: () => void): Promise<AuthAdapter> {
  await startMockApi(config.apiUrl);
  let userId: string | null = read();

  const write = (value: string | null) => {
    userId = value;
    try {
      if (value) localStorage.setItem(MOCK_SESSION_KEY, value);
      else localStorage.removeItem(MOCK_SESSION_KEY);
    } catch {
      // Private mode: the session lasts for this tab only.
    }
    changed();
  };

  return {
    controls: {
      mode: "mock",
      signInAsDemo: () => write(DEMO_USER_ID),
      signUp: (input) => write(signUp(input)),
      resetDemoData() {
        resetDb();
        write(DEMO_USER_ID);
      },
    },
    isSignedIn: () => userId !== null,
    token: async () => (userId ? `mock_${userId}` : null),
    signOut: () => write(null),
  };
}
