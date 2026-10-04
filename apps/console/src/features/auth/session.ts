/**
 * The Console's session boundary.
 *
 * Today it is backed by the mock identity provider. When the backend lands
 * this module delegates to the Console's auth adapter: in `oidc` mode
 * `getToken` returns a fresh token from the provider for every request; in
 * `local` mode there is none. Nothing is ever written to browser storage.
 */
import { setTokenSource } from "@/lib/api";
import { db, persist } from "@/mocks/db";
import { MOCK_SESSION_KEY } from "@/mocks/http";
import { newId } from "@/mocks/ids";
import { DEMO_USER_ID } from "@/mocks/seed";

type Listener = () => void;
const listeners = new Set<Listener>();

function read(): string | null {
  try {
    return localStorage.getItem(MOCK_SESSION_KEY);
  } catch {
    return null;
  }
}

let userId: string | null = read();

function write(value: string | null) {
  userId = value;
  try {
    if (value) localStorage.setItem(MOCK_SESSION_KEY, value);
    else localStorage.removeItem(MOCK_SESSION_KEY);
  } catch {
    // Private mode: the session lasts for this tab only.
  }
  for (const listener of listeners) listener();
}

setTokenSource(async () => (userId ? `mock_${userId}` : null));

export const session = {
  isSignedIn: () => userId !== null,
  userId: () => userId,
  subscribe(listener: Listener) {
    listeners.add(listener);
    return () => listeners.delete(listener);
  },
  signInAsDemo() {
    write(DEMO_USER_ID);
  },
  /** Mock sign-up: a brand new identity with no organizations yet. */
  signUp(input: { name: string; email: string }) {
    const existing = db().users.find((u) => u.email === input.email.toLowerCase());
    if (existing) {
      // Signing up with an invited address accepts the invitations.
      const now = new Date().toISOString();
      for (const m of db().memberships) {
        if (m.user_id === existing.id && m.status === "invited") {
          m.status = "active";
          m.joined_at = now;
        }
      }
      persist();
      write(existing.id);
      return;
    }
    const user = {
      id: newId("user"),
      name: input.name.trim(),
      email: input.email.trim().toLowerCase(),
      avatar_url: null,
    };
    db().users.push(user);
    persist();
    write(user.id);
  },
  signOut() {
    write(null);
  },
};
