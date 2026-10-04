import type { Pagination } from "@zupflow/contract";

/** One page of a paginated list, as the SDK returns it. */
export interface Page<T> {
  items: T[];
  pagination: Pagination;
}
