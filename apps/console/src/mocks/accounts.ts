/**
 * The mock's simulated sign-up: who an address signs in as. It stands in for
 * an identity provider and decision 0027's claim of an invitee by verified
 * email; every address the simulation takes counts as verified.
 */
import type { UserId } from "@zupflow/sdk";
import { db, persist } from "./db";
import { newId } from "./ids";

/**
 * The user an address signs in as: the existing one with that address (an
 * invitee, say), or a new one. It joins nothing: invitations wait for the
 * person to accept each one.
 */
export function signUp(input: { name: string; email: string }): UserId {
  const email = input.email.trim().toLowerCase();
  const existing = db().users.find((u) => u.email === email);
  if (existing) return existing.id;
  const user = { id: newId("user"), name: input.name.trim(), email, avatar_url: null };
  db().users.push(user);
  persist();
  return user.id;
}
