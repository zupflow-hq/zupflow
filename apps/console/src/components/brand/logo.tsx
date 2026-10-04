import { cn } from "@/lib/utils";

/**
 * Zupflow B1 Precision mark and wordmark, from the brand kit's outlined
 * paths. Geometry is fixed: do not stretch, round or re-letter.
 */
const LOOP = "M30 12H64L84 32V50H68V39L57 28H37L28 37V59L39 70H50V86H32L12 66V30Z";
const PLAY = "M60 56L90 73L60 90Z";
const WORDMARK =
  "M3.77 -36.09H35.26V-28.04L16.24 -8.25H35.26V0H2.96V-8.06L21.98 -27.84H3.77Z M40.71 -14.05V-36.09H52.31V-32.48Q52.31 -29.55 52.28 -25.12Q52.25 -20.69 52.25 -19.21Q52.25 -14.86 52.47 -12.94Q52.7 -11.02 53.25 -10.15Q53.96 -9.02 55.1 -8.41Q56.24 -7.8 57.73 -7.8Q61.34 -7.8 63.4 -10.57Q65.46 -13.34 65.46 -18.27V-36.09H77V0H65.46V-5.22Q62.85 -2.06 59.93 -0.56Q57.02 0.93 53.5 0.93Q47.25 0.93 43.98 -2.9Q40.71 -6.74 40.71 -14.05Z M96.76 -5.22V13.73H85.22V-36.09H96.76V-30.81Q99.15 -33.97 102.05 -35.47Q104.95 -36.96 108.72 -36.96Q115.39 -36.96 119.67 -31.66Q123.96 -26.36 123.96 -18.01Q123.96 -9.67 119.67 -4.37Q115.39 0.93 108.72 0.93Q104.95 0.93 102.05 -0.56Q99.15 -2.06 96.76 -5.22ZM104.43 -28.58Q100.72 -28.58 98.74 -25.86Q96.76 -23.14 96.76 -18.01Q96.76 -12.89 98.74 -10.17Q100.72 -7.44 104.43 -7.44Q108.14 -7.44 110.09 -10.15Q112.04 -12.86 112.04 -18.01Q112.04 -23.17 110.09 -25.88Q108.14 -28.58 104.43 -28.58Z M153.36 -50.14V-42.57H146.98Q144.53 -42.57 143.56 -41.69Q142.59 -40.8 142.59 -38.61V-36.09H152.46V-27.84H142.59V0H131.06V-27.84H125.32V-36.09H131.06V-38.61Q131.06 -44.5 134.34 -47.32Q137.63 -50.14 144.53 -50.14Z M155.46 -50.14H167V0H155.46Z M192.4 -28.71Q188.57 -28.71 186.55 -25.96Q184.54 -23.2 184.54 -18.01Q184.54 -12.83 186.55 -10.07Q188.57 -7.32 192.4 -7.32Q196.17 -7.32 198.17 -10.07Q200.17 -12.83 200.17 -18.01Q200.17 -23.2 198.17 -25.96Q196.17 -28.71 192.4 -28.71ZM192.4 -36.96Q201.71 -36.96 206.95 -31.94Q212.19 -26.91 212.19 -18.01Q212.19 -9.12 206.95 -4.09Q201.71 0.93 192.4 0.93Q183.06 0.93 177.79 -4.09Q172.52 -9.12 172.52 -18.01Q172.52 -26.91 177.79 -31.94Q183.06 -36.96 192.4 -36.96Z M214.48 -36.09H225.7L231.76 -11.21L237.85 -36.09H247.48L253.54 -11.47L259.63 -36.09H270.85L261.34 0H248.74L242.65 -24.81L236.59 0H223.99Z";

interface MarkProps {
  className?: string;
  /** Loop color. Defaults to brand cyan. */
  loop?: string;
  /** Play-triangle color. Defaults to the loop color. */
  play?: string;
  title?: string;
}

export function ZupflowMark({ className, loop = "var(--brand)", play, title = "Zupflow" }: MarkProps) {
  return (
    <svg viewBox="10 10 82 82" className={cn("shrink-0", className)} role="img" aria-label={title}>
      <path fill={loop} d={LOOP} />
      <path fill={play ?? loop} d={PLAY} />
    </svg>
  );
}

export function ZupflowWordmark({ className }: { className?: string }) {
  return (
    <svg viewBox="2.9 -50.2 268 64" className={cn("shrink-0 fill-current", className)} role="img" aria-label="Zupflow">
      <path d={WORDMARK} />
    </svg>
  );
}

/** Horizontal lockup: cyan mark, wordmark in the current text color. */
export function ZupflowLogo({ className }: { className?: string }) {
  return (
    <span className={cn("inline-flex items-center gap-[0.32em] text-foreground", className)}>
      <ZupflowMark className="h-[1.05em] w-auto" />
      <ZupflowWordmark className="h-[0.86em] w-auto translate-y-[0.12em]" />
    </span>
  );
}
