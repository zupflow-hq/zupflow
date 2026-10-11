import type { AuthTemplate } from "@zupflow/sdk";
import { describe, expect, it } from "vitest";
import { checkedAuthTemplate, validPattern } from "./auth-templates";

describe("configured OAuth issuer", () => {
  const legacy: AuthTemplate = {
    type: "oauth2",
    authorize_url: null,
    token_url: null,
    identity_url: null,
    pkce: true,
    available_scopes: [],
  };

  it("keeps old templates compatible and preserves an issuer's exact spelling", () => {
    expect(checkedAuthTemplate(legacy, legacy, [])).toEqual({ ...legacy, issuer: null });
    const issuer = "https://AUTH.provider.example/Tenant/";
    expect(checkedAuthTemplate(legacy, { ...legacy, issuer }, [])).toEqual({ ...legacy, issuer });
  });

  it("refuses ambiguous, private or non-HTTPS issuer identifiers", () => {
    for (const issuer of [
      "",
      " https://auth.provider.example",
      "https://auth.provider.example ",
      "https://auth.provider.example?",
      "https://auth.provider.example#",
      "https://auth.provider.example/?tenant=x",
      "https://auth.provider.example/#tenant",
      "https://user:secret@auth.provider.example",
      "https://@auth.provider.example",
      "http://auth.provider.example",
      "https://localhost",
      "https://127.0.0.1",
      "https://auth.provider.example\\tenant",
      "https:/auth.provider.example",
      42,
    ]) {
      expect(() => checkedAuthTemplate(legacy, { ...legacy, issuer }, []), String(issuer)).toThrow();
    }
  });
});

// The same table as the backend's `accepts_only_the_portable_grammar`.
describe("validPattern", () => {
  it("accepts the portable grammar", () => {
    for (const pattern of [
      String.raw`^tk_[a-z0-9]{8,}$`,
      String.raw`^(?:sk|rk)_(test|live)_\w+$`,
      String.raw`\d{4}`,
      String.raw`[A-Za-z0-9\-_]+`,
      "a{2,5}b?",
      String.raw`\.\$\(\)`,
      String.raw`[^\s]`,
      "(a|b|)",
    ]) {
      expect(validPattern(pattern), pattern).toBe(true);
    }
  });

  it("refuses what only one engine understands, or reads differently", () => {
    for (const pattern of [
      "",
      "(",
      "(?=tk)tk",
      "(?i)tk",
      "(?<name>tk)",
      String.raw`(tk)\1`,
      String.raw`(?P<n>tk)`,
      "a{",
      "a{,3}",
      String.raw`\Z`,
      String.raw`\x`,
      String.raw`\c`,
      String.raw`\e`,
      String.raw`[a-\d]`,
      "[^]",
      String.raw`\0`,
      String.raw`\cA`,
      String.raw`[\b]`,
      String.raw`\K`,
      String.raw`\R`,
      String.raw`\h`,
      "a**",
      "a{1}{2}",
      "a++",
      "a?+",
      String.raw`\x{41}`,
      String.raw`\p{L}`,
      "[[:alpha:]]",
      "[]a]",
      String.raw`\A`,
      String.raw`\z`,
      "a{5,2}",
      "a{1001}",
      "[a&&b]",
      "[z-a]",
      "^*",
      "a|*",
      String.raw`\/`,
      "[😀-😂]",
      "😀+",
    ]) {
      expect(validPattern(pattern), pattern).toBe(false);
    }
  });
});
