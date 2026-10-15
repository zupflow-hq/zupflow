import type { RunUsage } from "@zupflow/sdk";
import { cn } from "@/lib/utils";

type Tokens = Pick<RunUsage, "input_tokens" | "cache_read_input_tokens" | "cache_write_input_tokens">;

/**
 * Input tokens in their three parts: read from the provider's prompt cache,
 * written to it, and the rest, sent at full price. Cached and written are
 * parts of `input_tokens`, never added to it.
 */
export function inputParts(usage: Tokens) {
  const cached = Math.max(0, usage.cache_read_input_tokens);
  const written = Math.max(0, usage.cache_write_input_tokens);
  const fresh = Math.max(0, usage.input_tokens - cached - written);
  const total = Math.max(usage.input_tokens, cached + written);
  return { cached, written, fresh, total, cachedShare: total > 0 ? cached / total : 0 };
}

const n = (value: number) => value.toLocaleString("en");

/** The share of input read from the cache, as a percentage people can compare across runs. */
export function cachedPercent(usage: Tokens): string {
  const { cachedShare, total } = inputParts(usage);
  if (total === 0) return "—";
  const pct = cachedShare * 100;
  return `${pct > 0 && pct < 1 ? "<1" : Math.round(pct)}%`;
}

/**
 * A run's input tokens: one bar split into cached, written and new, with the
 * numbers beside it, so a run that stopped hitting the cache stands out.
 */
export function InputTokens({ usage, className }: { usage: Tokens; className?: string }) {
  const { cached, written, fresh, total } = inputParts(usage);
  const width = (part: number) => `${total > 0 ? (part / total) * 100 : 0}%`;
  return (
    <div className={cn("space-y-1.5", className)}>
      <p className="tabular">
        {n(total)} <span className="text-muted-foreground">in</span>
        {total > 0 ? <span className="text-muted-foreground"> · {cachedPercent(usage)} from cache</span> : null}
      </p>
      {total > 0 ? (
        <>
          <div className="flex h-1.5 overflow-hidden rounded-full bg-muted" aria-hidden>
            <span className="bg-success" style={{ width: width(cached) }} />
            <span className="bg-warning" style={{ width: width(written) }} />
            <span className="bg-muted-foreground/40" style={{ width: width(fresh) }} />
          </div>
          <ul className="tabular space-y-0.5 text-xs text-muted-foreground">
            <li className="flex items-center gap-1.5">
              <span aria-hidden className="size-1.5 rounded-full bg-success" />
              {n(cached)} read from cache
            </li>
            <li className="flex items-center gap-1.5">
              <span aria-hidden className="size-1.5 rounded-full bg-warning" />
              {n(written)} written to cache
            </li>
            <li className="flex items-center gap-1.5">
              <span aria-hidden className="size-1.5 rounded-full bg-muted-foreground/40" />
              {n(fresh)} new
            </li>
          </ul>
        </>
      ) : null}
    </div>
  );
}
