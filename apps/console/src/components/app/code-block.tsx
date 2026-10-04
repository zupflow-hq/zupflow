import { useState } from "react";
import { cn } from "@/lib/utils";
import { CopyButton } from "./misc";

/** A tabbed code sample with copy. Plain text: no highlighting dependency. */
export function CodeTabs({ samples, className }: { samples: { label: string; code: string }[]; className?: string }) {
  const [active, setActive] = useState(0);
  const current = samples[active] ?? samples[0];
  if (!current) return null;
  return (
    <div className={cn("overflow-hidden rounded-xl border bg-[#0d1518] text-[#d7e2e7]", className)}>
      <div className="flex items-center justify-between border-b border-white/10 pr-1.5 pl-2">
        <div role="tablist" className="flex">
          {samples.map((s, i) => (
            <button
              key={s.label}
              type="button"
              role="tab"
              aria-selected={i === active}
              onClick={() => setActive(i)}
              className={cn(
                "relative px-2.5 py-2.5 font-mono text-[11.5px] text-white/50 outline-none hover:text-white/80 focus-visible:text-white",
                i === active &&
                  "text-white after:absolute after:inset-x-2.5 after:bottom-[-1px] after:h-px after:bg-[#35C8F5]",
              )}
            >
              {s.label}
            </button>
          ))}
        </div>
        <CopyButton
          value={current.code}
          label="Copy code"
          className="text-white/60 hover:bg-white/10 hover:text-white"
        />
      </div>
      <pre className="overflow-x-auto p-4 text-[12.5px] leading-relaxed">
        <code>{current.code}</code>
      </pre>
    </div>
  );
}
