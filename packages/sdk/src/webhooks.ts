/**
 * Verifying a webhook delivery (decision 0017): check the `Zupflow-Signature`
 * over the raw body, reject a stale timestamp, then read the event. Use the
 * body exactly as received — parsing and re-serializing it changes the bytes
 * and fails the signature.
 */
import {
  WEBHOOK_DELIVERY_TYPES,
  type WebhookDeliveryType,
  WebhookEvent,
  WebhookEventEnvelope,
} from "@zupflow/contract/schemas";
import { WEBHOOK_TOLERANCE_SECONDS } from "@zupflow/contract/webhooks";

export type WebhookErrorCode =
  | "missing_signature"
  | "malformed_signature"
  | "timestamp_out_of_range"
  | "signature_mismatch"
  | "invalid_payload";

export class ZupflowWebhookError extends Error {
  constructor(
    readonly code: WebhookErrorCode,
    message: string,
  ) {
    super(message);
    this.name = "ZupflowWebhookError";
  }
}

export interface VerifyWebhookOptions {
  /** Seconds a timestamp may be from `now`. Defaults to 300. */
  toleranceSeconds?: number;
  /** The current time in milliseconds, for tests. */
  now?: () => number;
}

const encoder = new TextEncoder();
const bytesOf = (body: string | Uint8Array) => (typeof body === "string" ? encoder.encode(body) : body);

async function hmacHex(secret: string, t: string, body: Uint8Array): Promise<string> {
  const key = await crypto.subtle.importKey("raw", encoder.encode(secret), { name: "HMAC", hash: "SHA-256" }, false, [
    "sign",
  ]);
  const prefix = encoder.encode(`${t}.`);
  const message = new Uint8Array(prefix.length + body.length);
  message.set(prefix);
  message.set(body, prefix.length);
  const mac = await crypto.subtle.sign("HMAC", key, message);
  return Array.from(new Uint8Array(mac), (b) => b.toString(16).padStart(2, "0")).join("");
}

/** Compares in time that depends only on the length, so a mismatch reveals nothing about where it is. */
function constantTimeEqual(a: string, b: string): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return diff === 0;
}

/**
 * A verified delivery. `known` events are checked against the contract's full
 * schema for their type. An unknown one is a type added after this SDK was
 * built: it's genuine (the signature holds), but only its envelope is checked,
 * so it's typed as such. Answer it with a `2xx` and ignore it, or upgrade.
 */
export type VerifiedWebhook = { known: true; event: WebhookEvent } | { known: false; event: WebhookEventEnvelope };

const KNOWN: ReadonlySet<string> = new Set<WebhookDeliveryType>(WEBHOOK_DELIVERY_TYPES);

/**
 * Verify a delivery and return its event. Throws {@link ZupflowWebhookError}
 * when the signature is missing, malformed, stale or wrong, or the body isn't
 * an event; answer such a request with a `4xx` and don't act on it.
 *
 * ```ts
 * const verified = await verifyWebhook(rawBody, request.headers.get("Zupflow-Signature"), secret);
 * if (!verified.known) return; // a newer event type: acknowledge it
 * switch (verified.event.type) { case "run.completed": … }
 * ```
 *
 * @param rawBody The request body exactly as received.
 * @param signature The `Zupflow-Signature` header.
 * @param secret The endpoint's signing secret.
 */
export async function verifyWebhook(
  rawBody: string | Uint8Array,
  signature: string | null | undefined,
  secret: string,
  options: VerifyWebhookOptions = {},
): Promise<VerifiedWebhook> {
  if (!secret) throw new TypeError("verifyWebhook needs the endpoint's signing secret.");
  const tolerance = options.toleranceSeconds ?? WEBHOOK_TOLERANCE_SECONDS;
  const now = (options.now ?? Date.now)() / 1000;
  // Fail closed: a tolerance or clock that isn't a real number must not turn the replay check off.
  if (!Number.isFinite(tolerance) || tolerance < 0 || !Number.isFinite(now)) {
    throw new TypeError("verifyWebhook needs a finite, non-negative tolerance and a finite clock.");
  }
  if (!signature) throw new ZupflowWebhookError("missing_signature", "The delivery has no Zupflow-Signature header.");
  let timestamp: string | undefined;
  const candidates: string[] = [];
  for (const part of signature.split(",")) {
    const [key, value, ...rest] = part.trim().split("=");
    if (!key || value === undefined || rest.length > 0) {
      throw new ZupflowWebhookError("malformed_signature", "The Zupflow-Signature header isn't `t=…,v1=…`.");
    }
    if (key === "t") {
      // One timestamp only: a second would make "which one was signed" ambiguous.
      if (timestamp !== undefined) timestamp = "";
      else timestamp = value;
    } else if (key === "v1" && value) candidates.push(value);
  }
  if (!timestamp || !/^\d+$/.test(timestamp) || candidates.length === 0) {
    throw new ZupflowWebhookError("malformed_signature", "The Zupflow-Signature header isn't `t=…,v1=…`.");
  }
  if (!(Math.abs(now - Number(timestamp)) <= tolerance)) {
    throw new ZupflowWebhookError(
      "timestamp_out_of_range",
      `The delivery is signed more than ${tolerance} seconds from now; it may be a replay.`,
    );
  }
  const body = bytesOf(rawBody);
  const expected = await hmacHex(secret, timestamp, body);
  // During a rotation a delivery carries a signature per secret; any one matching is enough.
  if (!candidates.some((candidate) => constantTimeEqual(candidate, expected))) {
    throw new ZupflowWebhookError("signature_mismatch", "The signature doesn't match this endpoint's secret.");
  }
  let json: unknown;
  try {
    json = JSON.parse(new TextDecoder().decode(body));
  } catch {
    throw new ZupflowWebhookError("invalid_payload", "The body isn't JSON.");
  }
  const envelope = WebhookEventEnvelope.safeParse(json);
  if (!envelope.success) throw new ZupflowWebhookError("invalid_payload", "The body isn't a Zupflow event.");
  if (!KNOWN.has(envelope.data.type)) return { known: false, event: envelope.data };
  const event = WebhookEvent.safeParse(json);
  if (!event.success) {
    throw new ZupflowWebhookError("invalid_payload", `The body isn't a valid ${envelope.data.type} event.`);
  }
  return { known: true, event: event.data };
}

/**
 * The `Zupflow-Signature` value for a body, as Zupflow sends it: for tests and
 * local tools that need to produce a delivery. `secrets` are signed in order.
 */
export async function signWebhook(
  rawBody: string | Uint8Array,
  secrets: string | readonly string[],
  timestampSeconds: number = Math.floor(Date.now() / 1000),
): Promise<string> {
  const t = String(Math.floor(timestampSeconds));
  const body = bytesOf(rawBody);
  const all = typeof secrets === "string" ? [secrets] : secrets;
  const signatures = await Promise.all(all.map((secret) => hmacHex(secret, t, body)));
  return [`t=${t}`, ...signatures.map((s) => `v1=${s}`)].join(",");
}
