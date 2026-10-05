/// <reference types="node" />
import { createHmac } from "node:crypto";
import { describe, expect, it } from "vitest";
import { signWebhook, verifyWebhook, ZupflowWebhookError } from "./webhooks";

const event = {
  id: "evt_034HXHH2LZFzvoaSAhfGVC",
  type: "run.completed",
  created_at: "2026-10-04T21:04:11Z",
  environment: "production",
  organization_id: "org_034CmCMi0M7gup3a0htwn5",
  workspace: { id: "ws_034FpGDIRLk8Of6gikx8DG", external_id: "acct_1042" },
  data: {
    object: "run",
    id: "run_034HXHH2LZFzvoaSAhfGVD",
    agent_id: "agt_034HXHH2LZFzvoaSAhfGVC",
    status: "completed",
  },
};
const body = JSON.stringify(event);
const secret = "whsec_test_secret";
const t = 1_791_000_000;
const now = () => t * 1000;

/** The signature computed independently of the code under test, exactly as the standard defines it. */
const expected = (raw: string, key: string, at: number) =>
  `t=${at},v1=${createHmac("sha256", key).update(`${at}.${raw}`).digest("hex")}`;

async function outcome(promise: Promise<unknown>) {
  try {
    await promise;
  } catch (error) {
    if (error instanceof ZupflowWebhookError) return error.code;
    if (error instanceof TypeError) return "TypeError";
    throw error;
  }
  return "verified";
}

describe("signWebhook", () => {
  it("signs `<t>.<raw body>` with HMAC-SHA-256 in lowercase hex", async () => {
    expect(await signWebhook(body, secret, t)).toBe(expected(body, secret, t));
  });
});

describe("verifyWebhook", () => {
  it("returns a known event for a delivery signed as the standard defines", async () => {
    const verified = await verifyWebhook(body, expected(body, secret, t), secret, { now });
    expect(verified).toEqual({ known: true, event });
    // The raw bytes verify the same as the string.
    expect(await verifyWebhook(new TextEncoder().encode(body), expected(body, secret, t), secret, { now })).toEqual(
      verified,
    );
  });

  it("refuses a changed body, another secret, or a re-serialized body", async () => {
    const signature = expected(body, secret, t);
    expect(await outcome(verifyWebhook(body.replace("acct_1042", "acct_1043"), signature, secret, { now }))).toBe(
      "signature_mismatch",
    );
    expect(await outcome(verifyWebhook(body, signature, "another_secret", { now }))).toBe("signature_mismatch");
    expect(await outcome(verifyWebhook(JSON.stringify(event, null, 2), signature, secret, { now }))).toBe(
      "signature_mismatch",
    );
  });

  it("binds the signature to its timestamp: a valid v1 re-stamped with a fresh t fails", async () => {
    const old = expected(body, secret, t - 3600).split(",v1=")[1];
    expect(await outcome(verifyWebhook(body, `t=${t},v1=${old}`, secret, { now }))).toBe("signature_mismatch");
  });

  it("compares every character: one flipped hex digit fails, at the end as at the start", async () => {
    const v1 = expected(body, secret, t).split(",v1=")[1] ?? "";
    const flip = (c: string) => (c === "0" ? "1" : "0");
    for (const tampered of [`${flip(v1[0] ?? "")}${v1.slice(1)}`, `${v1.slice(0, -1)}${flip(v1.at(-1) ?? "")}`]) {
      expect(await outcome(verifyWebhook(body, `t=${t},v1=${tampered}`, secret, { now }))).toBe("signature_mismatch");
    }
    expect(await outcome(verifyWebhook(body, `t=${t},v1=${v1.toUpperCase()}`, secret, { now }))).toBe(
      "signature_mismatch",
    );
    // A prefix of the real signature, or the real one with more after it, is not the signature.
    for (const wrongLength of [v1.slice(0, 1), v1.slice(0, -1), `${v1}0`]) {
      expect(await outcome(verifyWebhook(body, `t=${t},v1=${wrongLength}`, secret, { now }))).toBe(
        "signature_mismatch",
      );
    }
  });

  it("refuses a timestamp outside the tolerance, either way, and honours a custom one", async () => {
    for (const offset of [-301, 301]) {
      expect(await outcome(verifyWebhook(body, expected(body, secret, t + offset), secret, { now }))).toBe(
        "timestamp_out_of_range",
      );
    }
    expect(await outcome(verifyWebhook(body, expected(body, secret, t - 300), secret, { now }))).toBe("verified");
    const strict = { now, toleranceSeconds: 10 };
    expect(await outcome(verifyWebhook(body, expected(body, secret, t - 11), secret, strict))).toBe(
      "timestamp_out_of_range",
    );
    expect(await outcome(verifyWebhook(body, expected(body, secret, t - 10), secret, strict))).toBe("verified");
  });

  it("fails closed when the tolerance, the clock or the secret is missing", async () => {
    const stale = expected(body, secret, t - 3600);
    expect(await outcome(verifyWebhook(body, stale, secret, { now, toleranceSeconds: Number.NaN }))).toBe("TypeError");
    expect(await outcome(verifyWebhook(body, stale, secret, { now, toleranceSeconds: -1 }))).toBe("TypeError");
    expect(await outcome(verifyWebhook(body, stale, secret, { now: () => Number.NaN }))).toBe("TypeError");
    expect(await outcome(verifyWebhook(body, stale, "", { now }))).toBe("TypeError");
  });

  it("accepts either signature during a rotation", async () => {
    const signature = await signWebhook(body, ["old_secret", "new_secret"], t);
    expect(signature).toBe(`${expected(body, "old_secret", t)},v1=${expected(body, "new_secret", t).split(",v1=")[1]}`);
    expect(await outcome(verifyWebhook(body, signature, "old_secret", { now }))).toBe("verified");
    expect(await outcome(verifyWebhook(body, signature, "new_secret", { now }))).toBe("verified");
  });

  it("refuses a missing or malformed header", async () => {
    const v1 = expected(body, secret, t).split(",v1=")[1];
    expect(await outcome(verifyWebhook(body, null, secret, { now }))).toBe("missing_signature");
    expect(await outcome(verifyWebhook(body, `v1=${v1}`, secret, { now }))).toBe("malformed_signature");
    expect(await outcome(verifyWebhook(body, `t=soon,v1=${v1}`, secret, { now }))).toBe("malformed_signature");
    expect(await outcome(verifyWebhook(body, `t=${t},t=${t},v1=${v1}`, secret, { now }))).toBe("malformed_signature");
    expect(await outcome(verifyWebhook(body, `t=${t},v1=`, secret, { now }))).toBe("malformed_signature");
    expect(await outcome(verifyWebhook(body, `t=${t}=x,v1=${v1}`, secret, { now }))).toBe("malformed_signature");
    // A scheme this SDK doesn't know is skipped, so a future v2 can be sent beside v1.
    expect(await outcome(verifyWebhook(body, `t=${t},v2=abc,v1=${v1}`, secret, { now }))).toBe("verified");
  });

  it("checks a known type against its full schema", async () => {
    const wrong = JSON.stringify({ ...event, data: { ...event.data, status: "queued" } });
    expect(await outcome(verifyWebhook(wrong, expected(wrong, secret, t), secret, { now }))).toBe("invalid_payload");
    const notEvent = JSON.stringify({ hello: "world" });
    expect(await outcome(verifyWebhook(notEvent, expected(notEvent, secret, t), secret, { now }))).toBe(
      "invalid_payload",
    );
  });

  it("returns a type newer than this SDK as unknown, envelope checked and fields kept", async () => {
    const newer = JSON.stringify({
      ...event,
      type: "agent.updated",
      data: { object: "agent", id: "agt_034HXHH2LZFzvoaSAhfGVC", name: "x" },
      added_later: true,
    });
    const verified = await verifyWebhook(newer, expected(newer, secret, t), secret, { now });
    expect(verified.known).toBe(false);
    expect(verified.event).toEqual(JSON.parse(newer));
  });
});
