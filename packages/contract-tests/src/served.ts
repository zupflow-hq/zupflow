/**
 * A server that serves part of the API yet: the suite never sends a request
 * for an operation the target doesn't list, and a case that would is skipped,
 * naming the operation, instead of failing or passing by accident.
 */
export class NotServed extends Error {
  constructor(readonly operationId: string) {
    super(`${operationId} isn't served by this server yet`);
    this.name = "NotServed";
  }
}

/** The `NotServed` behind an error, whether thrown directly or wrapped by the SDK's transport. */
export function notServedIn(error: unknown): NotServed | undefined {
  if (error instanceof NotServed) return error;
  if (error instanceof Error && error.cause instanceof NotServed) return error.cause;
  return undefined;
}
