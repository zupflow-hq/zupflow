/** Secret material for the local mock; only keyed hashes reach its stored rows. */
import type { Environment } from "@zupflow/sdk";
import { db } from "./db";

const BASE62 = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/** Uniform CSPRNG base62, as the backend uses for a new API key. */
function randomBase62(length: number): string {
  let value = "";
  while (value.length < length) {
    for (const byte of crypto.getRandomValues(new Uint8Array(64))) {
      if (byte < 248 && value.length < length) value += BASE62[byte % 62];
    }
  }
  return value;
}

export function newApiKeySecret(environment: Environment) {
  const prefix = `zf_${environment === "production" ? "live" : "test"}_${randomBase62(4)}`;
  const secret = `${prefix}${randomBase62(40)}`;
  return { prefix, secret, last4: secret.slice(-4) };
}

/** One local server key, with a separate domain for each kind of hash. */
export async function keyedDigest(domain: "api_key" | "idempotency", value: string): Promise<string> {
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(db().secret_key),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const mac = await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(`zupflow.${domain}.v1\0${value}`));
  return Array.from(new Uint8Array(mac), (byte) => byte.toString(16).padStart(2, "0")).join("");
}
