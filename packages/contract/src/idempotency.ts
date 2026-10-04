/**
 * Idempotency for `POST` (decision 0016): one header, the same rules for
 * every route. A key is 1–255 printable ASCII characters, scoped to the
 * caller that sent it, and kept for 24 hours.
 */
export const IDEMPOTENCY_HEADER = "Idempotency-Key";

/** 1–255 printable ASCII characters. */
export const IDEMPOTENCY_KEY_PATTERN = /^[\x20-\x7E]{1,255}$/;

/** How long a key lasts from its first use. */
export const IDEMPOTENCY_KEY_TTL_MS = 24 * 60 * 60 * 1000;

/** How long a request in flight holds its key before another may take it over. */
export const IDEMPOTENCY_LOCK_MS = 60 * 1000;
