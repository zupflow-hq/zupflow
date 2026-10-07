/**
 * Which hosts a provider URL may name: the backend's rules (decision 0032),
 * so the mock refuses exactly what the API refuses when a URL is saved.
 * IP literals that aren't globally routable unicast, and a few names that
 * always mean this network, are refused; any other name passes here, and the
 * backend checks every address it resolves to on every call.
 */

/** `a.b.c.d` → its four octets, or `null`. The URL parser has already normalised shorthand forms. */
function ipv4(host: string): [number, number, number, number] | null {
  const parts = host.split(".");
  if (parts.length !== 4 || parts.some((p) => !/^\d{1,3}$/.test(p))) return null;
  const octets = parts.map(Number);
  if (octets.some((o) => o > 255)) return null;
  return octets as [number, number, number, number];
}

function isPublicV4([a, b, c]: [number, number, number, number]): boolean {
  const special =
    a === 0 ||
    a === 10 ||
    (a === 100 && b >= 64 && b <= 127) ||
    a === 127 ||
    (a === 169 && b === 254) ||
    (a === 172 && b >= 16 && b <= 31) ||
    (a === 192 && b === 0 && c === 0) ||
    (a === 192 && b === 0 && c === 2) ||
    (a === 192 && b === 88 && c === 99) ||
    (a === 192 && b === 168) ||
    (a === 198 && (b === 18 || b === 19)) ||
    (a === 198 && b === 51 && c === 100) ||
    (a === 203 && b === 0 && c === 113) ||
    a >= 224;
  return !special;
}

/** An IPv6 literal (as the URL parser writes it, without brackets) → its eight segments, or `null`. */
function ipv6(host: string): number[] | null {
  if (!host.includes(":")) return null;
  let text = host;
  // A trailing dotted IPv4 becomes two segments.
  const dotted = text.match(/^(.*:)(\d+\.\d+\.\d+\.\d+)$/);
  if (dotted) {
    const v4 = ipv4(dotted[2] ?? "");
    if (!v4) return null;
    text = `${dotted[1]}${((v4[0] << 8) | v4[1]).toString(16)}:${((v4[2] << 8) | v4[3]).toString(16)}`;
  }
  const halves = text.split("::");
  if (halves.length > 2) return null;
  const parse = (part: string) => (part === "" ? [] : part.split(":").map((h) => Number.parseInt(h, 16)));
  const head = parse(halves[0] ?? "");
  const tail = halves.length === 2 ? parse(halves[1] ?? "") : [];
  const missing = 8 - head.length - tail.length;
  if (halves.length === 1 ? missing !== 0 : missing < 1) return null;
  const segments = [...head, ...Array<number>(halves.length === 2 ? missing : 0).fill(0), ...tail];
  return segments.every((s) => Number.isInteger(s) && s >= 0 && s <= 0xffff) ? segments : null;
}

function isPublicV6(s: number[]): boolean {
  const [s0 = 0, s1 = 0, s2 = 0, s3 = 0, s4 = 0, s5 = 0, s6 = 0, s7 = 0] = s;
  const embedded = (): [number, number, number, number] => [s6 >> 8, s6 & 0xff, s7 >> 8, s7 & 0xff];
  const zeros = (n: number) => s.slice(0, n).every((x) => x === 0);
  // IPv4-mapped (::ffff:a.b.c.d) and IPv4-translated (::ffff:0:a.b.c.d) are judged by the address they carry.
  if (zeros(5) && s5 === 0xffff) return isPublicV4(embedded());
  if (zeros(4) && s4 === 0xffff && s5 === 0) return isPublicV4(embedded());
  const special =
    zeros(8) ||
    (zeros(7) && s7 === 1) ||
    zeros(6) || // IPv4-compatible
    (s0 === 0x64 && s1 === 0xff9b) || // NAT64
    (s0 === 0x100 && s1 === 0 && s2 === 0 && s3 === 0) || // discard
    (s0 === 0x2001 && s1 < 0x200) || // IETF protocol assignments, Teredo
    (s0 === 0x2001 && s1 === 0xdb8) || // documentation
    s0 === 0x2002 || // 6to4
    (s0 === 0x3fff && s1 < 0x1000) || // documentation
    s0 === 0x5f00 || // SRv6
    (s0 & 0xfe00) === 0xfc00 || // unique local
    (s0 & 0xffc0) === 0xfe80 || // link-local
    (s0 & 0xffc0) === 0xfec0 || // site-local
    (s0 & 0xff00) === 0xff00; // multicast
  return !special;
}

/** Whether a URL's hostname (as `new URL(...).hostname` gives it) may be saved as a provider destination. */
export function isPublicHost(hostname: string): boolean {
  const host = hostname.toLowerCase().replace(/\.+$/, "");
  if (host.startsWith("[") && host.endsWith("]")) {
    const segments = ipv6(host.slice(1, -1));
    return segments !== null && isPublicV6(segments);
  }
  const v4 = ipv4(host);
  if (v4) return isPublicV4(v4);
  if (host === "" || host === "localhost") return false;
  if ([".localhost", ".local", ".internal"].some((suffix) => host.endsWith(suffix))) return false;
  return !host.startsWith("metadata.");
}
