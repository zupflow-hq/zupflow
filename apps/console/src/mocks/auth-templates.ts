/**
 * Checking an app's new authentication template (`auth` on PATCH), with the
 * same rules and messages as the backend: a whole template of the app's own
 * type, its URLs public HTTPS, its token placement never a header Zupflow
 * sets itself, and no scope removed while a configuration still asks for it.
 */
import type { AuthTemplate, OAuthScope, TokenInput } from "@zupflow/sdk";
import { ApiFailure, assertPublicHttps } from "./http";

/** Headers the outbound path sets itself (decision 0032). */
const RESERVED_HEADERS = new Set([
  "host",
  "accept-encoding",
  "connection",
  "keep-alive",
  "proxy-authorization",
  "proxy-connection",
  "te",
  "trailer",
  "transfer-encoding",
  "upgrade",
  "content-length",
]);

const MESSAGES = {
  scopes: "Up to 50 scopes, each with a unique name of 1–300 characters and a description of up to 300.",
  inputs:
    "1 to 10 inputs, each with a unique key (a lowercase letter, then lowercase letters, digits or _, up to 40 characters), a label of 1–60 characters, and secret set; help up to 500 characters, placeholder up to 200, pattern up to 500.",
  pattern:
    "Use a simple regular expression: characters, [classes] with ranges, (groups), \\d \\w \\s, and one quantifier (* + ? {m,n}) each.",
} as const;

type Fields = Record<string, string>;

const label = (environment: string) => (environment === "production" ? "Production" : "Sandbox");
type Raw = Record<string, unknown>;

const isObject = (value: unknown): value is Raw => typeof value === "object" && value !== null && !Array.isArray(value);
const length = (value: string) => [...value].length;

function url(raw: Raw, key: string, fields: Fields): string | null | undefined {
  if (!(key in raw)) {
    fields[`auth.${key}`] ??= "Required; send null for none.";
    return undefined;
  }
  const value = raw[key];
  if (value === null) return null;
  if (typeof value !== "string") {
    fields[`auth.${key}`] ??= "Must be a URL or null.";
    return undefined;
  }
  const trimmed = value.trim();
  return trimmed === "" ? null : trimmed;
}

/** Issuer identifiers compare exactly; never trim or normalize one. */
function issuer(raw: Raw, fields: Fields): string | null {
  const value = raw.issuer;
  if (value === undefined || value === null) return null;
  if (
    typeof value !== "string" ||
    value === "" ||
    /[\s\\]|\p{Cc}/u.test(value) ||
    !/^[a-z][a-z0-9+.-]*:\/\/[^/]/i.test(value)
  ) {
    fields["auth.issuer"] = "Use a full issuer URL without whitespace, or null for none.";
    return null;
  }
  try {
    new URL(value);
  } catch {
    fields["auth.issuer"] = "Enter a full URL, including https://.";
    return null;
  }
  const authority = value.split("/")[2] ?? "";
  if (value.includes("?") || value.includes("#") || authority.includes("@")) {
    fields["auth.issuer"] = "Use an issuer URL without credentials, a query or a fragment.";
    return null;
  }
  return value;
}

function text(raw: Raw, key: string, max: number, fields: Fields, field = `auth.${key}`): string | undefined {
  const value = raw[key];
  if (typeof value !== "string") {
    fields[field] ??= key in raw ? "Must be a string." : "Required.";
    return undefined;
  }
  const trimmed = value.trim();
  if (length(trimmed) > max) {
    fields[field] ??= `Use ${max.toLocaleString("en-US")} characters or fewer.`;
    return undefined;
  }
  return trimmed;
}

/** A string up to `max`, or null; blank is null. `false` when invalid. */
function optionalText(value: unknown, max: number): string | null | false {
  if (value === null || value === undefined) return null;
  if (typeof value !== "string") return false;
  const trimmed = value.trim();
  if (length(trimmed) > max) return false;
  return trimmed === "" ? null : trimmed;
}

function scopes(raw: Raw, fields: Fields, key = "available_scopes"): OAuthScope[] | undefined {
  const value = raw[key];
  if (!Array.isArray(value) || value.length > 50) {
    fields[`auth.${key}`] ??= MESSAGES.scopes;
    return undefined;
  }
  const out: OAuthScope[] = [];
  for (const item of value) {
    const name = isObject(item) && typeof item.name === "string" ? item.name.trim() : null;
    const description = isObject(item) && typeof item.description === "string" ? item.description.trim() : null;
    const valid =
      name !== null &&
      description !== null &&
      length(name) >= 1 &&
      length(name) <= 300 &&
      length(description) <= 300 &&
      !out.some((scope) => scope.name === name);
    if (!valid) {
      fields[`auth.${key}`] ??= MESSAGES.scopes;
      return undefined;
    }
    out.push({ name, description });
  }
  return out;
}

/** Punctuation a pattern may escape, outside and inside classes: the characters both engines treat as special. */
const ESCAPABLE = "\\.+*?()|[]{}^$-&~";

/**
 * A pattern in the small grammar both the browser and the server read the
 * same way (decision 0035), the backend's `valid_pattern` character for
 * character: literals and escaped punctuation, `.`, `^`, `$`, `|`, groups
 * `(…)` and `(?:…)`, classes of literals, escaped punctuation, ranges and
 * `\d \w \s`, and one quantifier per atom (`*`, `+`, `?`, `{m}`, `{m,}`,
 * `{m,n}` up to 1000). The server's check is the one that counts.
 */
class Pattern {
  private at = 0;
  private depth = 0;
  constructor(private readonly chars: string[]) {}

  get done() {
    return this.at === this.chars.length;
  }
  private peek(offset = 0): string | undefined {
    return this.chars[this.at + offset];
  }
  private eat(expected: string): boolean {
    const matched = this.peek() === expected;
    if (matched) this.at++;
    return matched;
  }

  alternation(): boolean {
    for (;;) {
      for (let c = this.peek(); c !== undefined && c !== "|" && c !== ")"; c = this.peek()) {
        if (!this.term()) return false;
      }
      if (!this.eat("|")) return true;
    }
  }

  private term(): boolean {
    const c = this.peek();
    if (c === undefined) return false;
    this.at++;
    let quantifiable: boolean;
    if (c === "^" || c === "$") {
      quantifiable = false;
    } else if (c === ".") {
      quantifiable = true;
    } else if (c === "\\") {
      const e = this.peek();
      if (e === undefined || !("dDwWsS".includes(e) || ESCAPABLE.includes(e))) return false;
      this.at++;
      quantifiable = true;
    } else if (c === "(") {
      if (this.eat("?") && !this.eat(":")) return false;
      this.depth++;
      if (this.depth > 10 || !this.alternation() || !this.eat(")")) return false;
      this.depth--;
      quantifiable = true;
    } else if (c === "[") {
      if (!this.charClass()) return false;
      quantifiable = true;
    } else if (")]{}?*+".includes(c)) {
      return false;
    } else {
      quantifiable = true;
    }
    if (quantifiable && !this.quantifier()) return false;
    const next = this.peek();
    return !(next !== undefined && "*+?{".includes(next));
  }

  private number(): number | undefined {
    const start = this.at;
    for (let c = this.peek(); c !== undefined && c >= "0" && c <= "9"; c = this.peek()) this.at++;
    const digits = this.chars.slice(start, this.at).join("");
    if (digits.length === 0 || digits.length > 4) return undefined;
    const n = Number(digits);
    return n <= 1000 ? n : undefined;
  }

  private quantifier(): boolean {
    const c = this.peek();
    if (c === "*" || c === "+" || c === "?") {
      this.at++;
      return true;
    }
    if (c !== "{") return true;
    this.at++;
    const least = this.number();
    if (least === undefined) return false;
    if (this.eat(",") && this.peek() !== "}") {
      const most = this.number();
      if (most === undefined || most < least) return false;
    }
    return this.eat("}");
  }

  /** One literal inside a class: a plain character or escaped punctuation. */
  private classLiteral(): string | undefined {
    const c = this.peek();
    if (c === undefined) return undefined;
    if (c === "\\") {
      const escaped = this.peek(1);
      if (escaped === undefined || !ESCAPABLE.includes(escaped)) return undefined;
      this.at += 2;
      return escaped;
    }
    if ("[]-&~".includes(c)) return undefined;
    this.at++;
    return c;
  }

  private charClass(): boolean {
    this.eat("^");
    let empty = true;
    for (;;) {
      const c = this.peek();
      if (c === undefined) return false;
      if (c === "]") {
        this.at++;
        return !empty;
      }
      const next = this.peek(1);
      if (c === "\\" && next !== undefined && "dws".includes(next)) {
        this.at += 2;
      } else {
        const low = this.classLiteral();
        if (low === undefined) return false;
        if (this.eat("-")) {
          const high = this.classLiteral();
          // Code points, as Rust compares `char`s.
          if (high === undefined || (high.codePointAt(0) ?? 0) < (low.codePointAt(0) ?? 0)) return false;
        }
      }
      empty = false;
    }
  }
}

/** Whether a token input's pattern may be stored: in the portable grammar, and accepted by the browser's engine. */
export function validPattern(pattern: string): boolean {
  const chars = [...pattern];
  // Outside the BMP, the browser sees two UTF-16 halves where the server sees one character.
  if (chars.some((c) => (c.codePointAt(0) ?? 0) > 0xffff)) return false;
  const parser = new Pattern(chars);
  if (pattern.length === 0 || !parser.alternation() || !parser.done) return false;
  try {
    new RegExp(pattern);
    return true;
  } catch {
    return false;
  }
}

function inputs(raw: Raw, fields: Fields): TokenInput[] | undefined {
  const value = raw.inputs;
  if (!Array.isArray(value) || value.length < 1 || value.length > 10) {
    fields["auth.inputs"] ??= MESSAGES.inputs;
    return undefined;
  }
  const out: TokenInput[] = [];
  for (const item of value) {
    if (!isObject(item)) {
      fields["auth.inputs"] ??= MESSAGES.inputs;
      return undefined;
    }
    const key = typeof item.key === "string" ? item.key : "";
    const label = typeof item.label === "string" ? item.label.trim() : "";
    const help = optionalText(item.help, 500);
    const placeholder = optionalText(item.placeholder, 200);
    const pattern = optionalText(item.pattern, 500);
    const valid =
      /^[a-z][a-z0-9_]{0,39}$/.test(key) &&
      length(label) >= 1 &&
      length(label) <= 60 &&
      typeof item.secret === "boolean" &&
      help !== false &&
      placeholder !== false &&
      pattern !== false &&
      !out.some((input) => input.key === key);
    if (!valid) {
      fields["auth.inputs"] ??= MESSAGES.inputs;
      return undefined;
    }
    if (pattern && !validPattern(pattern)) {
      fields["auth.inputs"] ??= MESSAGES.pattern;
      return undefined;
    }
    out.push({ key, label, secret: item.secret as boolean, help, placeholder, pattern });
  }
  return out;
}

function placement(raw: Raw, fields: Fields) {
  const value = raw.placement;
  if (!isObject(value)) {
    fields["auth.placement"] ??= "Required.";
    return undefined;
  }
  let valid = true;
  if (value.location !== "header") {
    fields["auth.placement.location"] ??= "Only header is supported.";
    valid = false;
  }
  const name = typeof value.name === "string" ? value.name.trim() : "";
  if (!/^[!#$%&'*+\-.^_`|~0-9A-Za-z]{1,60}$/.test(name)) {
    fields["auth.placement.name"] ??= "Use a header name, like Authorization.";
    valid = false;
  } else if (RESERVED_HEADERS.has(name.toLowerCase())) {
    fields["auth.placement.name"] ??= "Zupflow sets this header itself.";
    valid = false;
  }
  const prefix = value.prefix;
  if (typeof prefix !== "string" || length(prefix) > 40 || !/^[\x20-\x7e]*$/.test(prefix)) {
    fields["auth.placement.prefix"] ??= "Use up to 40 printable characters.";
    valid = false;
  }
  return valid ? { location: "header" as const, name, prefix: prefix as string } : undefined;
}

/**
 * The template to store for `current`, or a failure: `400` for its shape,
 * `422` for its type or its URLs, `409` for a scope still in use by one of
 * `usedScopes` (environment → scopes its configuration asks for). An
 * `mcp_oauth` identity URL must share the origin of each of `servers`
 * (environment → its configuration's MCP server URL).
 */
function originOf(url: string): string {
  try {
    return new URL(url).origin;
  } catch {
    return url;
  }
}

/**
 * Whether two URLs share an origin: scheme, host and port. An `mcp_oauth`
 * app's identity URL must share its MCP server's, so the token it is sent
 * stays with the server it was issued for.
 */
export function sameOrigin(a: string, b: string): boolean {
  try {
    const [x, y] = [new URL(a), new URL(b)];
    return x.protocol === y.protocol && x.host === y.host && x.origin !== "null";
  } catch {
    return false;
  }
}

export function checkedAuthTemplate(
  current: AuthTemplate,
  raw: unknown,
  usedScopes: { environment: string; scopes: string[] }[],
  servers: { environment: string; url: string }[] = [],
): AuthTemplate {
  if (!isObject(raw) || (raw.type !== "oauth2" && raw.type !== "mcp_oauth" && raw.type !== "token")) {
    throw new ApiFailure(400, "validation_failed", "Some fields are invalid.", {
      "auth.type": "Choose oauth2, mcp_oauth or token.",
    });
  }
  if (raw.type !== current.type) {
    throw new ApiFailure(
      422,
      "auth_type_immutable",
      "An app's authentication type can't change. Create another app instead.",
      { "auth.type": `This app uses ${current.type}.` },
    );
  }
  const fields: Fields = {};
  if (raw.type === "oauth2") {
    const authorize_url = url(raw, "authorize_url", fields);
    const token_url = url(raw, "token_url", fields);
    const identity_url = url(raw, "identity_url", fields);
    const trustedIssuer = issuer(raw, fields);
    const available_scopes = scopes(raw, fields);
    if (typeof raw.pkce !== "boolean") fields["auth.pkce"] ??= "Must be true or false.";
    if (Object.keys(fields).length > 0) {
      throw new ApiFailure(400, "validation_failed", "Some fields are invalid.", fields);
    }
    const offered = new Set((available_scopes ?? []).map((scope) => scope.name));
    for (const { environment, scopes: used } of usedScopes) {
      const missing = used.find((scope) => !offered.has(scope));
      if (missing) {
        throw new ApiFailure(
          409,
          "scope_in_use",
          `${missing} is requested by the ${label(environment)} configuration. Remove it there first.`,
          { "auth.available_scopes": `${missing} is still requested by the ${label(environment)} configuration.` },
        );
      }
    }
    // After the scopes, as the API standard orders statuses: 409 before 422.
    for (const [key, value] of [
      ["authorize_url", authorize_url],
      ["token_url", token_url],
      ["identity_url", identity_url],
    ] as const) {
      if (value) assertPublicHttps(value, `auth.${key}`);
    }
    if (trustedIssuer !== null) assertPublicHttps(trustedIssuer, "auth.issuer");
    return {
      type: "oauth2",
      authorize_url: authorize_url ?? null,
      token_url: token_url ?? null,
      issuer: trustedIssuer,
      identity_url: identity_url ?? null,
      available_scopes: available_scopes ?? [],
      pkce: raw.pkce as boolean,
    };
  }
  if (raw.type === "mcp_oauth") {
    // No configuration requests scopes of an `mcp_oauth` app: the template's are what sign-in asks for.
    const requested = scopes(raw, fields, "scopes");
    const identity_url = url(raw, "identity_url", fields);
    if (Object.keys(fields).length > 0 || !requested) {
      throw new ApiFailure(400, "validation_failed", "Some fields are invalid.", fields);
    }
    if (identity_url) {
      assertPublicHttps(identity_url, "auth.identity_url");
      const elsewhere = servers.find((server) => !sameOrigin(identity_url, server.url));
      if (elsewhere) {
        throw new ApiFailure(
          422,
          "identity_off_server",
          "The identity URL must be on the app's MCP server, so its token stays there.",
          {
            "auth.identity_url": `Use an address on ${originOf(elsewhere.url)}, the ${label(elsewhere.environment)} configuration's MCP server.`,
          },
        );
      }
    }
    return { type: "mcp_oauth", scopes: requested, identity_url: identity_url ?? null };
  }
  const parsedInputs = inputs(raw, fields);
  const parsedPlacement = placement(raw, fields);
  const verify_url = url(raw, "verify_url", fields);
  const docs_url = url(raw, "docs_url", fields);
  const instructions = text(raw, "instructions", 1000, fields);
  if (Object.keys(fields).length > 0 || !parsedInputs || !parsedPlacement || instructions === undefined) {
    throw new ApiFailure(400, "validation_failed", "Some fields are invalid.", fields);
  }
  for (const [key, value] of [
    ["verify_url", verify_url],
    ["docs_url", docs_url],
  ] as const) {
    if (value) assertPublicHttps(value, `auth.${key}`);
  }
  return {
    type: "token",
    inputs: parsedInputs,
    placement: parsedPlacement,
    verify_url: verify_url ?? null,
    instructions,
    docs_url: docs_url ?? null,
  };
}
