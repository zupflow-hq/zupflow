/**
 * IDs exactly as the backend issues them: `{prefix}_{body}`, where the body is
 * a UUIDv7 written in base62 and padded to 22 characters. The alphabet is in
 * ASCII order and the width is fixed, so IDs sort by creation time as plain
 * strings.
 *
 * The mock stores IDs in this wire form. The backend stores the UUID and adds
 * the prefix at the edge; both show the same string.
 */
import { ID_PREFIXES, type Id, type IdKind } from "@zupflow/sdk";
import type { Rng } from "./random";

const BASE62 = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const BODY_LENGTH = 22;
const BODY = /^[0-9A-Za-z]{22}$/;
const MAX = (1n << 128n) - 1n;

type RandomBytes = (length: number) => Uint8Array;

const secureBytes: RandomBytes = (length) => crypto.getRandomValues(new Uint8Array(length));

function bytesFrom(rng: Rng): RandomBytes {
  return (length) => Uint8Array.from({ length }, () => Math.floor(rng() * 256));
}

/** A UUIDv7 as 16 bytes: 48 bits of Unix milliseconds, then version, variant and randomness. */
export function uuidv7(ms: number, random: RandomBytes = secureBytes): Uint8Array {
  const bytes = random(16);
  let time = BigInt(Math.max(0, Math.floor(ms)));
  for (let i = 5; i >= 0; i--) {
    bytes[i] = Number(time & 0xffn);
    time >>= 8n;
  }
  bytes[6] = ((bytes[6] ?? 0) & 0x0f) | 0x70;
  bytes[8] = ((bytes[8] ?? 0) & 0x3f) | 0x80;
  return bytes;
}

function toBase62(bytes: Uint8Array): string {
  let n = 0n;
  for (const b of bytes) n = (n << 8n) | BigInt(b);
  let out = "";
  while (n > 0n) {
    out = BASE62[Number(n % 62n)] + out;
    n /= 62n;
  }
  return out.padStart(BODY_LENGTH, "0");
}

function fromBase62(body: string): bigint | null {
  if (!BODY.test(body)) return null;
  let n = 0n;
  for (const c of body) n = n * 62n + BigInt(BASE62.indexOf(c));
  return n <= MAX ? n : null;
}

interface NewIdOptions {
  /** Creation time, so seeded objects sort by when they were made. Defaults to now. */
  at?: number;
  /** Deterministic randomness for the seed. Defaults to `crypto.getRandomValues`. */
  rng?: Rng;
}

export function newId<K extends IdKind>(kind: K, options: NewIdOptions = {}): Id<K> {
  const random = options.rng ? bytesFrom(options.rng) : secureBytes;
  const body = toBase62(uuidv7(options.at ?? Date.now(), random));
  return `${ID_PREFIXES[kind]}_${body}` as Id<K>;
}

/** Whether `value` is a well-formed ID of `kind`: the right prefix and a valid 128-bit body. */
export function isId<K extends IdKind>(kind: K, value: unknown): value is Id<K> {
  if (typeof value !== "string") return false;
  const prefix = `${ID_PREFIXES[kind]}_`;
  return value.startsWith(prefix) && fromBase62(value.slice(prefix.length)) !== null;
}

/** The canonical UUID inside an ID, for tests that prove the encoding is lossless. */
export function uuidOf(id: string): string | null {
  const body = id.slice(id.indexOf("_") + 1);
  const n = fromBase62(body);
  if (n === null) return null;
  const hex = n.toString(16).padStart(32, "0");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/** What's stored for a possession reference: its SHA-256, hex. The reference itself never is. */
export async function hashReference(reference: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(reference));
  return Array.from(new Uint8Array(digest), (b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * A random, unguessable reference for links that work by possession, such as
 * hosted Connect. Never an object's ID: IDs carry a timestamp and are meant to
 * be shown, logged and shared.
 */
export function newReference(): string {
  let out = "";
  while (out.length < 32) {
    for (const b of secureBytes(32)) {
      // 248 is the largest multiple of 62 below 256; skipping above it keeps every character equally likely.
      if (b < 248 && out.length < 32) out += BASE62[b % 62];
    }
  }
  return out;
}
