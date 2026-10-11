//! Checking an app's new authentication template (`auth` on PATCH): a whole
//! template of the app's own type, its URLs public HTTPS, its token placement
//! never a header Zupflow sets itself, and no scope removed while one of the
//! app's configurations still requests it. The mock applies the same rules
//! with the same messages, so the shared suite holds both to them.

use serde_json::{Map, Value};
use zupflow_core::apps::{AuthTemplate, OAuthScope, TokenInput, TokenLocation, TokenPlacement};
use zupflow_core::environment::Environment;
use zupflow_core::error::ApiError;
use zupflow_core::outbound::RESERVED_HEADERS;

use crate::app_configurations::check_public_https;
use crate::http::Failure;
use crate::input::Fields;

const SCOPES: &str = "Up to 50 scopes, each with a unique name of 1–300 characters and a description of up to 300.";
const INPUTS: &str = "1 to 10 inputs, each with a unique key (a lowercase letter, then lowercase letters, digits or _, \
                      up to 40 characters), a label of 1–60 characters, and secret set; help up to 500 characters, \
                      placeholder up to 200, pattern up to 500.";
const PATTERN: &str = "Use a simple regular expression: characters, [classes] with ranges, (groups), \\d \\w \\s, and one quantifier (* + ? {m,n}) each.";

/// Whether two URLs share an origin: scheme, host and port. An `mcp_oauth`
/// app's identity URL must share its MCP server's, so the token it is sent
/// stays with the server it was issued for.
pub(crate) fn same_origin(a: &str, b: &str) -> bool {
    match (url::Url::parse(a), url::Url::parse(b)) {
        (Ok(a), Ok(b)) => a.origin().is_tuple() && a.origin() == b.origin(),
        _ => false,
    }
}

fn count(value: &str) -> usize {
    value.chars().count()
}

/// A nullable URL field: required, a string or `null`; blank is `null`.
/// `None` when it's invalid, with the field recorded.
fn url(raw: &Map<String, Value>, key: &str, fields: &mut Fields) -> Option<Option<String>> {
    let field = format!("auth.{key}");
    match raw.get(key) {
        None => {
            fields.invalid(&field, "Required; send null for none.");
            None
        }
        Some(Value::Null) => Some(None),
        Some(Value::String(value)) => {
            let value = value.trim();
            Some((!value.is_empty()).then(|| value.to_owned()))
        }
        Some(_) => {
            fields.invalid(&field, "Must be a URL or null.");
            None
        }
    }
}

/// Issuer identifiers compare exactly; never trim or normalize one.
fn issuer(raw: &Map<String, Value>, fields: &mut Fields) -> Option<Option<String>> {
    match raw.get("issuer") {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(value))
            if !value.is_empty()
                && !value
                    .chars()
                    .any(|c| c.is_whitespace() || c.is_control() || c == '\u{feff}' || c == '\\')
                && value.split_once("://").is_some_and(|(scheme, authority)| {
                    let mut chars = scheme.chars();
                    !authority.is_empty()
                        && !authority.starts_with('/')
                        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
                        && chars.all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
                }) =>
        {
            if url::Url::parse(value).is_err() {
                fields.invalid("auth.issuer", "Enter a full URL, including https://.");
                return None;
            }
            let authority = value.split('/').nth(2).unwrap_or_default();
            if value.contains(['?', '#']) || authority.contains('@') {
                fields.invalid(
                    "auth.issuer",
                    "Use an issuer URL without credentials, a query or a fragment.",
                );
                return None;
            }
            Some(Some(value.clone()))
        }
        _ => {
            fields.invalid(
                "auth.issuer",
                "Use a full issuer URL without whitespace, or null for none.",
            );
            None
        }
    }
}

/// A string up to `max` characters, or `null`; blank is `null`. `Err` when invalid.
fn optional_text(value: Option<&Value>, max: usize) -> Result<Option<String>, ()> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            let value = value.trim();
            if count(value) > max {
                Err(())
            } else {
                Ok((!value.is_empty()).then(|| value.to_owned()))
            }
        }
        Some(_) => Err(()),
    }
}

/// The scopes under `key`: `available_scopes` for an OAuth app's offer,
/// `scopes` for what an `mcp_oauth` app's sign-in asks for.
fn scopes(raw: &Map<String, Value>, key: &str, fields: &mut Fields) -> Option<Vec<OAuthScope>> {
    let invalid = |fields: &mut Fields| {
        fields.invalid(&format!("auth.{key}"), SCOPES);
        None
    };
    let Some(Value::Array(items)) = raw.get(key) else {
        return invalid(fields);
    };
    if items.len() > 50 {
        return invalid(fields);
    }
    let mut out: Vec<OAuthScope> = Vec::with_capacity(items.len());
    for item in items {
        let (Some(Value::String(name)), Some(Value::String(description))) = (item.get("name"), item.get("description"))
        else {
            return invalid(fields);
        };
        let (name, description) = (name.trim(), description.trim());
        if !(1..=300).contains(&count(name)) || count(description) > 300 || out.iter().any(|scope| scope.name == name) {
            return invalid(fields);
        }
        out.push(OAuthScope {
            name: name.to_owned(),
            description: description.to_owned(),
        });
    }
    Some(out)
}

fn valid_key(key: &str) -> bool {
    let mut bytes = key.bytes();
    key.len() <= 40
        && bytes.next().is_some_and(|first| first.is_ascii_lowercase())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn inputs(raw: &Map<String, Value>, fields: &mut Fields) -> Option<Vec<TokenInput>> {
    let invalid = |fields: &mut Fields| {
        fields.invalid("auth.inputs", INPUTS);
        None
    };
    let Some(Value::Array(items)) = raw.get("inputs") else {
        return invalid(fields);
    };
    if !(1..=10).contains(&items.len()) {
        return invalid(fields);
    }
    let mut out: Vec<TokenInput> = Vec::with_capacity(items.len());
    for item in items {
        let key = item.get("key").and_then(Value::as_str).unwrap_or_default();
        let label = item.get("label").and_then(Value::as_str).unwrap_or_default().trim();
        let secret = item.get("secret").and_then(Value::as_bool);
        let help = optional_text(item.get("help"), 500);
        let placeholder = optional_text(item.get("placeholder"), 200);
        let pattern = optional_text(item.get("pattern"), 500);
        let (Some(secret), Ok(help), Ok(placeholder), Ok(pattern)) = (secret, help, placeholder, pattern) else {
            return invalid(fields);
        };
        if !item.is_object()
            || !valid_key(key)
            || !(1..=60).contains(&count(label))
            || out.iter().any(|input| input.key == key)
        {
            return invalid(fields);
        }
        if pattern.as_deref().is_some_and(|pattern| !valid_pattern(pattern)) {
            fields.invalid("auth.inputs", PATTERN);
            return None;
        }
        out.push(TokenInput {
            key: key.to_owned(),
            label: label.to_owned(),
            secret,
            help,
            placeholder,
            pattern,
        });
    }
    Some(out)
}

/// Punctuation a pattern may escape, outside and inside classes: the
/// characters both engines treat as special.
const ESCAPABLE: &str = "\\.+*?()|[]{}^$-&~";

/// A pattern in the small grammar both the browser and the server read the
/// same way (decision 0035): literals and escaped punctuation, `.`, `^`, `$`,
/// `|`, groups `(…)` and `(?:…)`, classes of literals, escaped punctuation,
/// ranges and `\d \w \s`, and one quantifier per atom (`*`, `+`, `?`,
/// `{m}`, `{m,}`, `{m,n}` up to 1000). The mock applies the same grammar,
/// character for character. Connect's check on the server is the one that
/// counts; the browser's is a convenience.
struct Pattern {
    chars: Vec<char>,
    at: usize,
    depth: usize,
}

impl Pattern {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn eat(&mut self, expected: char) -> bool {
        let matched = self.peek() == Some(expected);
        if matched {
            self.at += 1;
        }
        matched
    }

    fn alternation(&mut self) -> bool {
        loop {
            while let Some(c) = self.peek() {
                if c == '|' || c == ')' {
                    break;
                }
                if !self.term() {
                    return false;
                }
            }
            if !self.eat('|') {
                return true;
            }
        }
    }

    fn term(&mut self) -> bool {
        let Some(c) = self.peek() else { return false };
        self.at += 1;
        let quantifiable = match c {
            '^' | '$' => false,
            '.' => true,
            '\\' => match self.peek() {
                Some(e) if "dDwWsS".contains(e) || ESCAPABLE.contains(e) => {
                    self.at += 1;
                    true
                }
                _ => return false,
            },
            '(' => {
                if self.eat('?') && !self.eat(':') {
                    return false;
                }
                self.depth += 1;
                if self.depth > 10 || !self.alternation() || !self.eat(')') {
                    return false;
                }
                self.depth -= 1;
                true
            }
            '[' => {
                if !self.class() {
                    return false;
                }
                true
            }
            ')' | ']' | '{' | '}' | '?' | '*' | '+' => return false,
            _ => true,
        };
        if quantifiable && !self.quantifier() {
            return false;
        }
        !matches!(self.peek(), Some('*' | '+' | '?' | '{'))
    }

    fn number(&mut self) -> Option<u32> {
        let start = self.at;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.at += 1;
        }
        let digits: String = self.chars[start..self.at].iter().collect();
        if digits.is_empty() || digits.len() > 4 {
            return None;
        }
        digits.parse().ok().filter(|n| *n <= 1000)
    }

    fn quantifier(&mut self) -> bool {
        match self.peek() {
            Some('*' | '+' | '?') => {
                self.at += 1;
                true
            }
            Some('{') => {
                self.at += 1;
                let Some(least) = self.number() else { return false };
                if self.eat(',') && self.peek() != Some('}') {
                    match self.number() {
                        Some(most) if most >= least => {}
                        _ => return false,
                    }
                }
                self.eat('}')
            }
            _ => true,
        }
    }

    /// One literal inside a class: a plain character or escaped punctuation.
    fn class_literal(&mut self) -> Option<char> {
        match self.peek()? {
            '\\' => {
                let escaped = *self.chars.get(self.at + 1)?;
                if !ESCAPABLE.contains(escaped) {
                    return None;
                }
                self.at += 2;
                Some(escaped)
            }
            '[' | ']' | '-' | '&' | '~' => None,
            c => {
                self.at += 1;
                Some(c)
            }
        }
    }

    fn class(&mut self) -> bool {
        self.eat('^');
        let mut empty = true;
        loop {
            match self.peek() {
                None => return false,
                Some(']') => {
                    self.at += 1;
                    return !empty;
                }
                Some('\\') if self.chars.get(self.at + 1).is_some_and(|e| "dws".contains(*e)) => {
                    self.at += 2;
                }
                Some(_) => {
                    let Some(low) = self.class_literal() else { return false };
                    if self.eat('-') {
                        match self.class_literal() {
                            Some(high) if high >= low => {}
                            _ => return false,
                        }
                    }
                }
            }
            empty = false;
        }
    }
}

/// Whether a token input's pattern may be stored: in the portable grammar,
/// and accepted by the server's engine.
fn valid_pattern(pattern: &str) -> bool {
    let mut parser = Pattern {
        chars: pattern.chars().collect(),
        at: 0,
        depth: 0,
    };
    // Outside the BMP, the browser sees two UTF-16 halves where the server sees one character.
    !parser.chars.is_empty()
        && parser.chars.iter().all(|c| u32::from(*c) <= 0xFFFF)
        && parser.alternation()
        && parser.at == parser.chars.len()
        && regex::RegexBuilder::new(pattern).size_limit(1 << 16).build().is_ok()
}

/// An HTTP header name: a token of the characters RFC 9110 allows.
fn header_name(name: &str) -> bool {
    (1..=60).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn placement(raw: &Map<String, Value>, fields: &mut Fields) -> Option<TokenPlacement> {
    let Some(Value::Object(value)) = raw.get("placement") else {
        fields.invalid("auth.placement", "Required.");
        return None;
    };
    let mut valid = true;
    if value.get("location").and_then(Value::as_str) != Some("header") {
        fields.invalid("auth.placement.location", "Only header is supported.");
        valid = false;
    }
    let name = value.get("name").and_then(Value::as_str).unwrap_or_default().trim();
    if !header_name(name) {
        fields.invalid("auth.placement.name", "Use a header name, like Authorization.");
        valid = false;
    } else if RESERVED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
        fields.invalid("auth.placement.name", "Zupflow sets this header itself.");
        valid = false;
    }
    let prefix = value.get("prefix").and_then(Value::as_str);
    let prefix_valid =
        prefix.is_some_and(|prefix| count(prefix) <= 40 && prefix.bytes().all(|b| (0x20..=0x7e).contains(&b)));
    if !prefix_valid {
        fields.invalid("auth.placement.prefix", "Use up to 40 printable characters.");
        valid = false;
    }
    match (valid, prefix) {
        (true, Some(prefix)) => Some(TokenPlacement {
            location: TokenLocation::Header,
            name: name.to_owned(),
            prefix: prefix.to_owned(),
        }),
        _ => None,
    }
}

fn label(environment: Environment) -> &'static str {
    match environment {
        Environment::Sandbox => "Sandbox",
        Environment::Production => "Production",
    }
}

fn validation(fields: Fields) -> Failure {
    match fields.finish() {
        Err(failure) => failure,
        Ok(()) => ApiError::internal().into(),
    }
}

/// The template to store for an app whose template is `current`, or a
/// failure: `400` for its shape, `422` for its type or its URLs, `409` for a
/// scope one of `used` (each configuration's environment and the scopes it
/// requests) still asks for. An `mcp_oauth` identity URL must share the
/// origin of each of `servers` (each configuration's environment and MCP
/// server URL).
pub fn checked_auth_template(
    current: &AuthTemplate,
    raw: &Value,
    used: &[(Environment, Vec<String>)],
    servers: &[(Environment, String)],
) -> Result<AuthTemplate, Failure> {
    let raw = match raw {
        Value::Object(raw)
            if matches!(
                raw.get("type").and_then(Value::as_str),
                Some("oauth2" | "mcp_oauth" | "token")
            ) =>
        {
            raw
        }
        _ => {
            let mut error = ApiError::new(400, "validation_failed", "Some fields are invalid.");
            error.fields = Some([("auth.type".to_owned(), "Choose oauth2, mcp_oauth or token.".to_owned())].into());
            return Err(error.into());
        }
    };
    let wanted = raw.get("type").and_then(Value::as_str).unwrap_or_default();
    let wants_oauth = wanted == "oauth2";
    let current_type = current.type_name();
    if wanted != current_type {
        let mut error = ApiError::new(
            422,
            "auth_type_immutable",
            "An app's authentication type can't change. Create another app instead.",
        );
        error.fields = Some([("auth.type".to_owned(), format!("This app uses {current_type}."))].into());
        return Err(error.into());
    }
    let mut fields = Fields::default();
    if wanted == "mcp_oauth" {
        // No configuration requests an `mcp_oauth` app's scopes: its sign-in asks for these.
        let requested = scopes(raw, "scopes", &mut fields);
        let identity_url = url(raw, "identity_url", &mut fields);
        let (Some(scopes), Some(identity_url)) = (requested, identity_url) else {
            return Err(validation(fields));
        };
        if let Some(identity_url) = &identity_url {
            check_public_https(identity_url, "auth.identity_url")?;
            if let Some((environment, server)) = servers.iter().find(|(_, server)| !same_origin(identity_url, server)) {
                let origin = url::Url::parse(server)
                    .map(|server| server.origin().ascii_serialization())
                    .unwrap_or_else(|_| server.clone());
                let mut error = ApiError::new(
                    422,
                    "identity_off_server",
                    "The identity URL must be on the app's MCP server, so its token stays there.",
                );
                error.fields = Some(
                    [(
                        "auth.identity_url".to_owned(),
                        format!(
                            "Use an address on {origin}, the {} configuration's MCP server.",
                            label(*environment)
                        ),
                    )]
                    .into(),
                );
                return Err(error.into());
            }
        }
        return Ok(AuthTemplate::McpOAuth { scopes, identity_url });
    }
    if wants_oauth {
        let authorize_url = url(raw, "authorize_url", &mut fields);
        let token_url = url(raw, "token_url", &mut fields);
        let identity_url = url(raw, "identity_url", &mut fields);
        let issuer = issuer(raw, &mut fields);
        let available_scopes = scopes(raw, "available_scopes", &mut fields);
        let pkce = raw.get("pkce").and_then(Value::as_bool);
        if pkce.is_none() {
            fields.invalid("auth.pkce", "Must be true or false.");
        }
        let (
            Some(authorize_url),
            Some(token_url),
            Some(identity_url),
            Some(issuer),
            Some(available_scopes),
            Some(pkce),
        ) = (authorize_url, token_url, identity_url, issuer, available_scopes, pkce)
        else {
            return Err(validation(fields));
        };
        for (environment, requested) in used {
            if let Some(missing) = requested
                .iter()
                .find(|scope| !available_scopes.iter().any(|offered| &offered.name == *scope))
            {
                let environment = label(*environment);
                let mut error = ApiError::new(
                    409,
                    "scope_in_use",
                    format!("{missing} is requested by the {environment} configuration. Remove it there first."),
                );
                error.fields = Some(
                    [(
                        "auth.available_scopes".to_owned(),
                        format!("{missing} is still requested by the {environment} configuration."),
                    )]
                    .into(),
                );
                return Err(error.into());
            }
        }
        // After the scopes, as the API standard orders statuses: 409 before 422.
        for (key, value) in [
            ("authorize_url", &authorize_url),
            ("token_url", &token_url),
            ("identity_url", &identity_url),
        ] {
            if let Some(value) = value {
                check_public_https(value, &format!("auth.{key}"))?;
            }
        }
        if let Some(issuer) = &issuer {
            check_public_https(issuer, "auth.issuer")?;
        }
        return Ok(AuthTemplate::OAuth2 {
            authorize_url,
            token_url,
            issuer,
            available_scopes,
            pkce,
            identity_url,
        });
    }
    let parsed_inputs = inputs(raw, &mut fields);
    let parsed_placement = placement(raw, &mut fields);
    let verify_url = url(raw, "verify_url", &mut fields);
    let docs_url = url(raw, "docs_url", &mut fields);
    let instructions = match raw.get("instructions") {
        Some(Value::String(value)) if count(value.trim()) <= 1000 => Some(value.trim().to_owned()),
        Some(Value::String(_)) => {
            fields.invalid("auth.instructions", "Use 1,000 characters or fewer.");
            None
        }
        Some(_) => {
            fields.invalid("auth.instructions", "Must be a string.");
            None
        }
        None => {
            fields.invalid("auth.instructions", "Required.");
            None
        }
    };
    let (Some(inputs), Some(placement), Some(verify_url), Some(docs_url), Some(instructions)) =
        (parsed_inputs, parsed_placement, verify_url, docs_url, instructions)
    else {
        return Err(validation(fields));
    };
    for (key, value) in [("verify_url", &verify_url), ("docs_url", &docs_url)] {
        if let Some(value) = value {
            check_public_https(value, &format!("auth.{key}"))?;
        }
    }
    Ok(AuthTemplate::Token {
        inputs,
        placement,
        verify_url,
        instructions,
        docs_url,
    })
}

#[cfg(test)]
mod tests {
    use super::{same_origin, valid_pattern};

    #[test]
    fn compares_scheme_host_and_port() {
        let server = "https://tools.example/v1/mcp";
        for (url, same) in [
            ("https://tools.example/me", true),
            ("https://TOOLS.example:443/v2/me?x=1", true),
            ("https://tools.example:8443/me", false),
            ("https://auth.tools.example/me", false),
            ("https://other.example/me", false),
            ("http://tools.example/me", false),
            ("not a url", false),
        ] {
            assert_eq!(same_origin(url, server), same, "{url}");
        }
    }

    /// The same table as the mock's `auth-templates.test.ts`.
    #[test]
    fn accepts_only_the_portable_grammar() {
        for pattern in [
            r"^tk_[a-z0-9]{8,}$",
            r"^(?:sk|rk)_(test|live)_\w+$",
            r"\d{4}",
            r"[A-Za-z0-9\-_]+",
            r"a{2,5}b?",
            r"\.\$\(\)",
            r"[^\s]",
            r"(a|b|)",
        ] {
            assert!(valid_pattern(pattern), "{pattern}");
        }
        for pattern in [
            "",
            "(",
            "(?=tk)tk",
            "(?i)tk",
            "(?<name>tk)",
            r"(tk)\1",
            r"(?P<n>tk)",
            "a{",
            "a{,3}",
            r"\Z",
            r"\x",
            r"\c",
            r"\e",
            r"[a-\d]",
            "[^]",
            r"\0",
            r"\cA",
            r"[\b]",
            r"\K",
            r"\R",
            r"\h",
            "a**",
            "a{1}{2}",
            "a++",
            "a?+",
            r"\x{41}",
            r"\p{L}",
            "[[:alpha:]]",
            "[]a]",
            r"\A",
            r"\z",
            "a{5,2}",
            "a{1001}",
            "[a&&b]",
            "[z-a]",
            "^*",
            "a|*",
            r"\/",
            "[😀-😂]",
            "😀+",
        ] {
            assert!(!valid_pattern(pattern), "{pattern}");
        }
    }
}
