import { signWebhook, verifyWebhook, ZupflowWebhookError } from "@zupflow/sdk";
import { useState } from "react";
import "./WebhookPlayground.css";

const SAMPLE = JSON.stringify(
  {
    id: "evt_034HXHH2LZFzvoaSAhfGVC",
    type: "run.completed",
    created_at: "2026-10-04T21:04:11Z",
    environment: "sandbox",
    organization_id: "org_034CmCMi0M7gup3a0htwn5",
    workspace: { id: "ws_034FpGDIRLk8Of6gikx8DG", external_id: "acct_1042" },
    data: {
      object: "run",
      id: "run_034HXHH2LZFzvoaSAhfGVD",
      agent_id: "agt_034HXHH2LZFzvoaSAhfGVC",
      status: "completed",
    },
  },
  null,
  2,
);

type Outcome = { ok: true; text: string } | { ok: false; text: string } | null;

/**
 * Signs and verifies a delivery in the browser with the SDK's own
 * `signWebhook` and `verifyWebhook`. Nothing is sent anywhere.
 */
export default function WebhookPlayground() {
  const [secret, setSecret] = useState("whsec_test_example");
  const [body, setBody] = useState(SAMPLE);
  const [signature, setSignature] = useState("");
  const [outcome, setOutcome] = useState<Outcome>(null);

  async function sign() {
    setSignature(await signWebhook(body, secret));
    setOutcome(null);
  }

  async function verify() {
    try {
      const verified = await verifyWebhook(body, signature, secret);
      setOutcome({
        ok: true,
        text: verified.known
          ? `Verified: ${verified.event.type} (${verified.event.id})`
          : `Verified, but ${verified.event.type} is newer than this SDK: acknowledge it with a 2xx.`,
      });
    } catch (error) {
      const code = error instanceof ZupflowWebhookError ? error.code : "error";
      setOutcome({ ok: false, text: `${code}: ${(error as Error).message}` });
    }
  }

  return (
    <div className="zf-hook not-content">
      <label>
        Signing secret
        <input value={secret} onChange={(e) => setSecret(e.target.value)} spellCheck={false} />
      </label>
      <label>
        Raw body
        <textarea value={body} onChange={(e) => setBody(e.target.value)} rows={12} spellCheck={false} />
      </label>
      <label>
        Signature header
        <input
          value={signature}
          onChange={(e) => setSignature(e.target.value)}
          placeholder="t=…,v1=…"
          spellCheck={false}
        />
      </label>
      <div className="zf-hook-actions">
        <button type="button" onClick={sign}>
          Sign body
        </button>
        <button type="button" className="primary" onClick={verify}>
          Verify
        </button>
      </div>
      {outcome && (
        <p className={`zf-hook-outcome ${outcome.ok ? "ok" : "bad"}`} role="status">
          {outcome.text}
        </p>
      )}
    </div>
  );
}
