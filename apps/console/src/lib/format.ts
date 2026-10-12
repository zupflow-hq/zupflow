import { format, formatDistanceToNowStrict, isToday, isYesterday } from "date-fns";

export function relative(iso: string | null | undefined, fallback = "Never"): string {
  if (!iso) return fallback;
  const date = new Date(iso);
  const diff = Date.now() - date.getTime();
  if (diff >= 0 && diff < 45_000) return "just now";
  return formatDistanceToNowStrict(date, { addSuffix: true });
}

export function dateTime(iso: string | null | undefined): string {
  if (!iso) return "—";
  const date = new Date(iso);
  if (isToday(date)) return `Today, ${format(date, "HH:mm")}`;
  if (isYesterday(date)) return `Yesterday, ${format(date, "HH:mm")}`;
  return format(date, "MMM d, HH:mm");
}

export function fullDateTime(iso: string | null | undefined): string {
  if (!iso) return "—";
  return format(new Date(iso), "MMM d, yyyy 'at' HH:mm:ss");
}

export function shortDate(iso: string | null | undefined): string {
  if (!iso) return "—";
  return format(new Date(iso), "MMM d, yyyy");
}

export function duration(fromIso: string | null, toIso: string | null): string {
  if (!fromIso) return "—";
  const ms = (toIso ? Date.parse(toIso) : Date.now()) - Date.parse(fromIso);
  if (ms < 1000) return `${ms} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)} s`;
  const m = Math.floor(ms / 60_000);
  const s = Math.round((ms % 60_000) / 1000);
  return `${m}m ${s}s`;
}

export function compact(n: number): string {
  return new Intl.NumberFormat("en", { notation: "compact", maximumFractionDigits: 1 }).format(n);
}

export function count(n: number, singular: string, plural = `${singular}s`): string {
  return `${n.toLocaleString("en")} ${n === 1 ? singular : plural}`;
}

/**
 * A compact label for an ID: its prefix and the last six characters. IDs start
 * with their creation time, so the head of IDs made close together is the
 * same; the tail is random and tells them apart. Display only: the full ID is
 * what's copied and sent.
 */
export function shortId(id: string): string {
  const separator = id.indexOf("_");
  if (separator < 1 || id.length - separator < 10) return id;
  return `${id.slice(0, separator + 1)}…${id.slice(-6)}`;
}

export function slugify(value: string): string {
  return value
    .toLowerCase()
    .normalize("NFKD")
    .replace(/[̀-ͯ]/g, "")
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 40)
    .replace(/-+$/g, "");
}

/**
 * A suggested app key from an app's name (decision 0047): snake case, starting
 * with a letter, at most 24 characters. Empty when the name has no letter.
 */
export function appKeyFrom(value: string): string {
  return value
    .toLowerCase()
    .normalize("NFKD")
    .replace(/[\u0300-\u036f]/g, "")
    .replace(/[^a-z0-9]+/g, "_")
    .replace(/^[^a-z]+/, "")
    .slice(0, 24)
    .replace(/_+$/g, "");
}
