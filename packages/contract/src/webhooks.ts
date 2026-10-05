/**
 * Webhook delivery headers and signing parameters (decision 0017). Every
 * delivery is signed `t=<unix seconds>,v1=<hex HMAC-SHA256>` over
 * `"<t>.<raw body>"`, in lowercase hex; during a secret rotation it carries two
 * `v1` values.
 */
export const WEBHOOK_SIGNATURE_HEADER = "Zupflow-Signature";
export const WEBHOOK_EVENT_ID_HEADER = "Zupflow-Event-Id";
export const WEBHOOK_EVENT_TYPE_HEADER = "Zupflow-Event-Type";
export const WEBHOOK_ATTEMPT_HEADER = "Zupflow-Delivery-Attempt";
export const WEBHOOK_USER_AGENT = "Zupflow-Webhooks/1";

/** How far a delivery's timestamp may be from the receiver's clock, in seconds. */
export const WEBHOOK_TOLERANCE_SECONDS = 300;
