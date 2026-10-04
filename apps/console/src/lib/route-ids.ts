import type { Id, IdKind } from "@zupflow/sdk";
import { z } from "zod";

/**
 * Route params that carry an ID, typed as that ID. The segment goes to the
 * API unchanged: the API decides whether it exists, and a malformed one
 * answers 404. The Console never parses or checks an ID itself.
 */
export function idParam<const P extends string, K extends IdKind>(name: P, _kind: K) {
  return {
    parse: (raw: Record<P, string>) => ({ [name]: raw[name] }) as Record<P, Id<K>>,
    stringify: (params: Record<P, Id<K>>) => ({ [name]: params[name] }) as Record<P, string>,
  };
}

/** A search param that carries an ID, typed as that ID; like path IDs, the API judges it. */
export function idSearch<K extends IdKind>(_kind: K) {
  return z.custom<Id<K>>((value) => typeof value === "string");
}
