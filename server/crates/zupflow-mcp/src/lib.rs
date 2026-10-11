//! A minimal MCP client (decision 0038): lists a server's tools over the
//! Streamable HTTP transport, through [`OutboundHttp`] and nothing else.
//! [`sign_in`] finds a server's authorization server and registers a client
//! with it (decision 0039).
//!
//! [`list_tools`] initializes a session, says it's initialized, then pages
//! through `tools/list`. [`call_tool`] does the same, then sends one
//! `tools/call`, and classifies a failure by whether the tool may have acted. Every request carries the installation's credential
//! as the outbound path's credential, so it's injected last and scrubbed from
//! what comes back. An answer may be JSON or a `text/event-stream`; either
//! way only the JSON-RPC response with the request's ID is read.
//!
//! Each tool becomes a [`ListedTool`]: its definition is canonical JSON of
//! what the model reads (name, title, description, input and output schema)
//! and its annotations, keys sorted, hashed with SHA-256, so the same
//! definition hashes the same however the server orders it. A listing that
//! breaks the protocol or the registry's limits is refused whole: a partial
//! listing would mark tools gone that aren't.
//!
//! The outbound path scrubs the credential from the raw answer, but a server
//! can repeat it encoded (`\/` in JSON, `\u0041`), and parsing decodes it.
//! So a listing in which any decoded string or key holds the credential's
//! value or secret is refused whole too: what's pinned is what a model reads.

pub mod sign_in;

use std::collections::HashSet;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;
use zupflow_core::apps::ToolEffect;
use zupflow_core::outbound::{
    Credential, Limits, Method, OutboundError, OutboundHttp, OutboundRequest, OutboundResponse, Refusal,
};
use zupflow_core::tools::{ListedTool, MAX_DEFINITION_BYTES, MAX_NAME, MAX_TOOLS, ToolRevision};

/// The protocol versions this client speaks, newest first. The first is the
/// one it asks for.
pub const PROTOCOL_VERSIONS: [&str; 2] = ["2025-06-18", "2025-03-26"];

/// The most `tools/list` pages one listing reads.
const MAX_PAGES: usize = 20;

/// The longest session ID kept.
const MAX_SESSION_ID: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum McpError {
    /// The server refused the credential (`401`): the installation
    /// needs reconnecting.
    #[error("the server refused the credential")]
    Unauthorized,
    /// No answer, a timeout, a `429` or a `5xx`: worth trying again later.
    #[error("the server is unavailable")]
    Unavailable,
    /// The safe path refused the address before sending.
    #[error("the address was refused: {0:?}")]
    Refused(Refusal),
    /// An answer this client can't use, with what was wrong. Never the
    /// server's own words.
    #[error("the server broke the protocol: {0}")]
    Protocol(&'static str),
}

/// The header and value an installation's credential travels in, and the
/// secret inside that value (a token without its prefix).
#[derive(Clone, Copy)]
pub struct McpCredential<'a> {
    pub header: &'a str,
    pub value: &'a str,
    pub secret: &'a str,
}

impl std::fmt::Debug for McpCredential<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpCredential")
            .field("header", &self.header)
            .finish_non_exhaustive()
    }
}

/// A session with one server, as far as listing goes.
struct Session<'a> {
    http: &'a dyn OutboundHttp,
    url: &'a str,
    credential: Option<McpCredential<'a>>,
    id: Option<String>,
    protocol_version: Option<String>,
    next_request: u64,
    limits: Limits,
}

/// Lists every tool the server at `url` offers.
pub async fn list_tools(
    http: &dyn OutboundHttp,
    url: &str,
    credential: Option<McpCredential<'_>>,
) -> Result<Vec<ListedTool>, McpError> {
    let mut session = Session {
        http,
        url,
        credential,
        id: None,
        protocol_version: None,
        next_request: 1,
        limits: Limits::default(),
    };
    session.initialize().await?;
    let mut tools: Vec<ListedTool> = Vec::new();
    let mut names = HashSet::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let params = match &cursor {
            Some(cursor) => json!({ "cursor": cursor }),
            None => json!({}),
        };
        let result = session.request("tools/list", params).await?;
        let listed = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or(McpError::Protocol("tools/list answered without tools"))?;
        for tool in listed {
            let tool = listed_tool(tool, credential)?;
            if !names.insert(tool.name.clone()) {
                return Err(McpError::Protocol("a tool name listed twice"));
            }
            tools.push(tool);
            if tools.len() > MAX_TOOLS {
                return Err(McpError::Protocol("more tools than a listing keeps"));
            }
        }
        match result.get("nextCursor") {
            None | Some(Value::Null) => return Ok(tools),
            Some(Value::String(next)) if !next.is_empty() && cursor.as_deref() != Some(next.as_str()) => {
                cursor = Some(next.clone());
            }
            Some(_) => return Err(McpError::Protocol("a cursor that isn't a new string")),
        }
    }
    Err(McpError::Protocol("more pages than a listing reads"))
}

/// How a tool call failed, by what it means for whatever the tool does: a
/// call that may have acted is never sent again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    /// The call itself was never sent: the address was refused, the server
    /// couldn't be reached, or the session couldn't be set up.
    #[error("the call wasn't sent: {0}")]
    NotSent(McpError),
    /// The server refused the credential (`401`): the installation
    /// needs reconnecting. The tool didn't act.
    #[error("the server refused the credential")]
    Unauthorized,
    /// The server refused the call without acting: a JSON-RPC error, or a
    /// `4xx`.
    #[error("the server refused the call: {0}")]
    Rejected(&'static str),
    /// The call was sent and no usable answer came back (a timeout, a
    /// broken connection, a `5xx`, an answer that isn't one): it may have
    /// acted, or not.
    #[error("the call's outcome is unknown: {0}")]
    Unknown(&'static str),
    /// The answer repeated the credential, encoded where the outbound path's
    /// scrubbing can't see it: it's withheld whole.
    #[error("the answer repeated the credential")]
    Withheld,
}

/// What a tool answered: the call's `result`, as the server wrote it
/// (`content`, and `structuredContent` when given), and whether the tool
/// reported an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub result: Map<String, Value>,
    pub is_error: bool,
}

/// Calls `name` with `arguments` on the server at `url`, in a session of its
/// own, through the outbound path with the credential injected last.
pub async fn call_tool(
    http: &dyn OutboundHttp,
    url: &str,
    credential: Option<McpCredential<'_>>,
    name: &str,
    arguments: &Map<String, Value>,
    limits: Limits,
) -> Result<ToolResult, CallError> {
    let mut session = Session {
        http,
        url,
        credential,
        id: None,
        protocol_version: None,
        next_request: 1,
        limits,
    };
    session.initialize().await.map_err(|error| match error {
        McpError::Unauthorized => CallError::Unauthorized,
        error => CallError::NotSent(error),
    })?;
    let id = session.next_request;
    session.next_request += 1;
    let message = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    });
    let response = session.post(message).await.map_err(|error| match error {
        OutboundError::Refused(Refusal::Unresolvable) => CallError::NotSent(McpError::Unavailable),
        OutboundError::Refused(refusal) => CallError::NotSent(McpError::Refused(refusal)),
        OutboundError::Unreachable => CallError::NotSent(McpError::Unavailable),
        OutboundError::Redirected => CallError::Unknown("a redirect, which isn't followed"),
        OutboundError::TooLarge => CallError::Unknown("an answer larger than allowed"),
        OutboundError::TimedOut => CallError::Unknown("no answer in time"),
        OutboundError::Failed => CallError::Unknown("the exchange failed"),
    })?;
    match response.status {
        200..=299 => {}
        401 => return Err(CallError::Unauthorized),
        400..=499 => return Err(CallError::Rejected("an unexpected status")),
        _ => return Err(CallError::Unknown("a server error")),
    }
    let message = response_for(&response, id).map_err(|_| CallError::Unknown("no usable answer"))?;
    if message.contains_key("error") {
        return Err(CallError::Rejected("the server answered with an error"));
    }
    let Some(Value::Object(result)) = message.get("result") else {
        return Err(CallError::Unknown("an answer without a result"));
    };
    if let Some(credential) = credential
        && repeats(
            &Value::Object(result.clone()),
            &[credential.value, bare_secret(credential.secret)],
        )
    {
        return Err(CallError::Withheld);
    }
    let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
    let mut kept = Map::new();
    for key in ["content", "structuredContent"] {
        if let Some(value) = result.get(key) {
            kept.insert(key.to_owned(), value.clone());
        }
    }
    Ok(ToolResult { result: kept, is_error })
}

impl Session<'_> {
    async fn initialize(&mut self) -> Result<(), McpError> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSIONS[0],
                    "capabilities": {},
                    "clientInfo": { "name": "zupflow", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await?;
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .filter(|version| PROTOCOL_VERSIONS.contains(version))
            .ok_or(McpError::Protocol("a protocol version this client doesn't speak"))?;
        self.protocol_version = Some(version.to_owned());
        let response = self
            .send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await?;
        check_status(response.status)?;
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Map<String, Value>, McpError> {
        let id = self.next_request;
        self.next_request += 1;
        let response = self
            .send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await?;
        check_status(response.status)?;
        if self.id.is_none() {
            self.id = session_id(&response)?;
        }
        let message = response_for(&response, id)?;
        if message.contains_key("error") {
            return Err(McpError::Protocol("the server answered with an error"));
        }
        match message.get("result") {
            Some(Value::Object(result)) => Ok(result.clone()),
            _ => Err(McpError::Protocol("a response without a result")),
        }
    }

    async fn send(&self, message: Value) -> Result<OutboundResponse, McpError> {
        self.post(message).await.map_err(|error| match error {
            // A name that doesn't resolve may resolve later.
            OutboundError::Refused(Refusal::Unresolvable) => McpError::Unavailable,
            OutboundError::Refused(refusal) => McpError::Refused(refusal),
            OutboundError::Redirected => McpError::Protocol("a redirect, which isn't followed"),
            OutboundError::TooLarge => McpError::Protocol("an answer larger than allowed"),
            OutboundError::TimedOut | OutboundError::Unreachable | OutboundError::Failed => McpError::Unavailable,
        })
    }

    /// One message, through the outbound path, with the credential.
    async fn post(&self, message: Value) -> Result<OutboundResponse, OutboundError> {
        let mut headers = vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("Accept".to_owned(), "application/json, text/event-stream".to_owned()),
        ];
        if let Some(id) = &self.id {
            headers.push(("Mcp-Session-Id".to_owned(), id.clone()));
        }
        if let Some(version) = &self.protocol_version {
            headers.push(("MCP-Protocol-Version".to_owned(), version.clone()));
        }
        let request = OutboundRequest {
            method: Method::Post,
            url: self.url.to_owned(),
            headers,
            body: Some(message.to_string().into_bytes()),
            credential: self.credential.map(|credential| Credential {
                header: credential.header.to_owned(),
                value: Zeroizing::new(credential.value.to_owned()),
            }),
        };
        self.http.send(request, self.limits).await
    }
}

fn check_status(status: u16) -> Result<(), McpError> {
    match status {
        200..=299 => Ok(()),
        401 => Err(McpError::Unauthorized),
        403 => Err(McpError::Protocol("access forbidden")),
        429 | 500..=599 => Err(McpError::Unavailable),
        _ => Err(McpError::Protocol("an unexpected status")),
    }
}

fn header<'a>(response: &'a OutboundResponse, name: &str) -> Option<&'a str> {
    response
        .headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// The session ID the server assigned, if any: visible ASCII only.
fn session_id(response: &OutboundResponse) -> Result<Option<String>, McpError> {
    match header(response, "mcp-session-id") {
        None => Ok(None),
        Some(id)
            if !id.is_empty() && id.len() <= MAX_SESSION_ID && id.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) =>
        {
            Ok(Some(id.to_owned()))
        }
        Some(_) => Err(McpError::Protocol("a session ID that isn't visible ASCII")),
    }
}

/// The JSON-RPC response to request `id`, from a JSON body (one message or a
/// batch) or an event stream.
fn response_for(response: &OutboundResponse, id: u64) -> Result<Map<String, Value>, McpError> {
    let body = std::str::from_utf8(&response.body).map_err(|_| McpError::Protocol("an answer that isn't UTF-8"))?;
    let streamed = header(response, "content-type")
        .is_some_and(|kind| kind.trim().to_ascii_lowercase().starts_with("text/event-stream"));
    let messages: Vec<Value> = if streamed {
        events(body)
            .into_iter()
            .filter_map(|data| serde_json::from_str(&data).ok())
            .collect()
    } else {
        vec![serde_json::from_str(body).map_err(|_| McpError::Protocol("an answer that isn't JSON"))?]
    };
    messages
        .into_iter()
        .flat_map(|message| match message {
            Value::Array(batch) => batch,
            message => vec![message],
        })
        .filter_map(|message| match message {
            Value::Object(message) => Some(message),
            _ => None,
        })
        .find(|message| message.get("id").and_then(Value::as_u64) == Some(id))
        .ok_or(McpError::Protocol("no response to the request"))
}

/// The `data` of each event in a `text/event-stream` body.
fn events(body: &str) -> Vec<String> {
    let mut events = Vec::new();
    let mut data: Vec<&str> = Vec::new();
    for line in body.lines() {
        if line.is_empty() {
            if !data.is_empty() {
                events.push(data.join("\n"));
                data.clear();
            }
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }
    if !data.is_empty() {
        events.push(data.join("\n"));
    }
    events
}

/// The shortest bare secret looked for alone, as the outbound path's
/// scrubbing does: a shorter one would match ordinary text. The full value,
/// prefix included, is always looked for.
const MIN_BARE_SECRET: usize = 8;

fn bare_secret(secret: &str) -> &str {
    if secret.len() >= MIN_BARE_SECRET { secret } else { "" }
}

/// Whether any string or key in `value` holds one of `secrets`.
fn repeats(value: &Value, secrets: &[&str]) -> bool {
    let holds = |text: &str| secrets.iter().any(|secret| !secret.is_empty() && text.contains(secret));
    match value {
        Value::String(text) => holds(text),
        Value::Array(items) => items.iter().any(|item| repeats(item, secrets)),
        Value::Object(fields) => fields.iter().any(|(key, value)| holds(key) || repeats(value, secrets)),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

/// One listed tool, checked and made canonical.
fn listed_tool(tool: &Value, credential: Option<McpCredential<'_>>) -> Result<ListedTool, McpError> {
    if let Some(credential) = credential
        && repeats(tool, &[credential.value, bare_secret(credential.secret)])
    {
        return Err(McpError::Protocol("a listing that repeats the credential"));
    }
    let tool = tool
        .as_object()
        .ok_or(McpError::Protocol("a tool that isn't an object"))?;
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty() && name.chars().count() <= MAX_NAME)
        .ok_or(McpError::Protocol("a tool without a usable name"))?;
    let mut definition = Map::new();
    definition.insert("name".to_owned(), Value::String(name.to_owned()));
    for (key, valid) in [
        ("title", tool.get("title").is_some_and(Value::is_string)),
        ("description", tool.get("description").is_some_and(Value::is_string)),
        ("inputSchema", tool.get("inputSchema").is_some_and(Value::is_object)),
        ("outputSchema", tool.get("outputSchema").is_some_and(Value::is_object)),
        ("annotations", tool.get("annotations").is_some_and(Value::is_object)),
    ] {
        match tool.get(key) {
            None | Some(Value::Null) => {}
            Some(value) if valid => {
                definition.insert(key.to_owned(), value.clone());
            }
            Some(_) => return Err(McpError::Protocol("a tool field of the wrong type")),
        }
    }
    let effect = effect(definition.get("annotations"));
    let definition = canonical(&Value::Object(definition));
    if definition.len() > MAX_DEFINITION_BYTES {
        return Err(McpError::Protocol("a tool definition larger than kept"));
    }
    let hash = Sha256::digest(definition.as_bytes()).into();
    Ok(ListedTool {
        name: name.to_owned(),
        revision: ToolRevision { definition, hash },
        effect,
    })
}

/// The effect a person starts from, read from the server's hints, which are
/// only hints: read-only only when the server says so; otherwise destructive
/// unless the server says it isn't (the protocol's own default).
fn effect(annotations: Option<&Value>) -> ToolEffect {
    let hint = |name: &str| {
        annotations
            .and_then(|annotations| annotations.get(name))
            .and_then(Value::as_bool)
    };
    if hint("readOnlyHint") == Some(true) {
        ToolEffect::Read
    } else if hint("destructiveHint") == Some(false) {
        ToolEffect::Write
    } else {
        ToolEffect::Destructive
    }
}

/// JSON with every object's keys sorted and no whitespace: the same value
/// writes the same however it was ordered.
pub fn canonical(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                if let Some(value) = map.get(key) {
                    write_canonical(value, out);
                }
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;

    /// What the fake server was sent: the method, headers and credential.
    #[derive(Debug, Clone)]
    struct Sent {
        body: Value,
        headers: Vec<(String, String)>,
        credential: Option<(String, String)>,
    }

    /// Answers each request with the next scripted response.
    struct Server {
        answers: Mutex<Vec<Result<OutboundResponse, OutboundError>>>,
        sent: Mutex<Vec<Sent>>,
    }

    impl Server {
        fn new(answers: Vec<Result<OutboundResponse, OutboundError>>) -> Self {
            Self {
                answers: Mutex::new(answers.into_iter().rev().collect()),
                sent: Mutex::new(Vec::new()),
            }
        }

        fn sent(&self) -> Vec<Sent> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl OutboundHttp for Server {
        async fn send(&self, request: OutboundRequest, _: Limits) -> Result<OutboundResponse, OutboundError> {
            self.sent.lock().unwrap().push(Sent {
                body: serde_json::from_slice(request.body.as_deref().unwrap()).unwrap(),
                headers: request.headers,
                credential: request
                    .credential
                    .map(|credential| (credential.header, credential.value.to_string())),
            });
            self.answers.lock().unwrap().pop().unwrap()
        }
    }

    fn json_answer(body: Value, headers: &[(&str, &str)]) -> Result<OutboundResponse, OutboundError> {
        Ok(OutboundResponse {
            status: 200,
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .chain([("content-type".to_owned(), "application/json".to_owned())])
                .collect(),
            body: body.to_string().into_bytes(),
        })
    }

    fn status(status: u16) -> Result<OutboundResponse, OutboundError> {
        Ok(OutboundResponse {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        })
    }

    fn initialized(id: u64) -> Result<OutboundResponse, OutboundError> {
        json_answer(
            json!({ "jsonrpc": "2.0", "id": id, "result": { "protocolVersion": "2025-06-18", "capabilities": { "tools": {} } } }),
            &[("mcp-session-id", "session-1")],
        )
    }

    fn page(id: u64, tools: Value, next: Option<&str>) -> Result<OutboundResponse, OutboundError> {
        json_answer(
            json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools, "nextCursor": next } }),
            &[],
        )
    }

    const TOKEN: McpCredential<'static> = McpCredential {
        header: "Authorization",
        value: "Bearer secret-token",
        secret: "secret-token",
    };

    #[tokio::test]
    async fn lists_every_page_within_one_session() {
        let server = Server::new(vec![
            initialized(1),
            status(202),
            page(
                2,
                json!([{ "name": "search", "description": "Find things", "inputSchema": { "type": "object" }, "annotations": { "readOnlyHint": true } }]),
                Some("page-2"),
            ),
            page(
                3,
                json!([{ "name": "create", "inputSchema": { "type": "object" }, "annotations": { "destructiveHint": false } }, { "name": "drop", "inputSchema": { "type": "object" } }]),
                None,
            ),
        ]);
        let tools = list_tools(&server, "https://tools.example/mcp", Some(TOKEN))
            .await
            .unwrap();
        assert_eq!(
            tools
                .iter()
                .map(|tool| (tool.name.as_str(), tool.effect))
                .collect::<Vec<_>>(),
            [
                ("search", ToolEffect::Read),
                ("create", ToolEffect::Write),
                ("drop", ToolEffect::Destructive)
            ],
            "effects seeded from the hints, destructive unless said otherwise"
        );
        assert_eq!(
            tools[0].revision.definition,
            r#"{"annotations":{"readOnlyHint":true},"description":"Find things","inputSchema":{"type":"object"},"name":"search"}"#
        );

        let sent = server.sent();
        assert_eq!(
            sent.iter()
                .map(|sent| sent.body["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["initialize", "notifications/initialized", "tools/list", "tools/list"]
        );
        assert_eq!(sent[3].body["params"], json!({ "cursor": "page-2" }));
        assert!(sent[1].body.get("id").is_none(), "a notification has no ID");
        for later in &sent[1..] {
            assert!(
                later
                    .headers
                    .contains(&("Mcp-Session-Id".to_owned(), "session-1".to_owned()))
            );
            assert!(
                later
                    .headers
                    .contains(&("MCP-Protocol-Version".to_owned(), "2025-06-18".to_owned()))
            );
        }
        assert!(
            sent.iter()
                .all(|sent| sent.credential == Some(("Authorization".to_owned(), "Bearer secret-token".to_owned()))),
            "every request carries the credential, as the outbound credential"
        );
    }

    #[tokio::test]
    async fn reads_an_event_stream() {
        let streamed = Ok(OutboundResponse {
            status: 200,
            headers: vec![("Content-Type".to_owned(), "text/event-stream".to_owned())],
            body: concat!(
                "event: message\n",
                "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\n",
                "id: 7\n",
                "data: {\"jsonrpc\":\"2.0\",\"id\":2,\n",
                "data: \"result\":{\"tools\":[{\"name\":\"echo\",\"inputSchema\":{}}]}}\n\n",
            )
            .as_bytes()
            .to_vec(),
        });
        let server = Server::new(vec![initialized(1), status(202), streamed]);
        let tools = list_tools(&server, "https://tools.example/mcp", None).await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(server.sent()[0].credential, None);
    }

    #[test]
    fn hashes_the_same_definition_the_same_however_ordered() {
        let a = listed_tool(
            &json!({ "name": "t", "inputSchema": { "b": 1, "a": [1, { "y": 2, "x": 1 }] }, "description": "d" }),
            None,
        )
        .unwrap();
        let b = listed_tool(&json!({ "description": "d", "inputSchema": { "a": [1, { "x": 1, "y": 2 }], "b": 1 }, "name": "t", "_meta": { "ignored": true } }), None).unwrap();
        assert_eq!(a.revision, b.revision);
        let c = listed_tool(
            &json!({ "name": "t", "inputSchema": { "b": 1, "a": [1, { "y": 2, "x": 1 }] }, "description": "e" }),
            None,
        )
        .unwrap();
        assert_ne!(
            a.revision.hash, c.revision.hash,
            "anything the model reads changes the hash"
        );
    }

    #[tokio::test]
    async fn classifies_what_goes_wrong() {
        let cases: Vec<(Vec<Result<OutboundResponse, OutboundError>>, McpError)> = vec![
            (vec![status(401)], McpError::Unauthorized),
            (vec![status(403)], McpError::Protocol("access forbidden")),
            (vec![status(503)], McpError::Unavailable),
            (vec![status(429)], McpError::Unavailable),
            (vec![Err(OutboundError::TimedOut)], McpError::Unavailable),
            (
                vec![Err(OutboundError::Refused(Refusal::NotPublic))],
                McpError::Refused(Refusal::NotPublic),
            ),
            (vec![status(404)], McpError::Protocol("an unexpected status")),
            (
                vec![json_answer(
                    json!({ "jsonrpc": "2.0", "id": 1, "result": { "protocolVersion": "1999-01-01" } }),
                    &[],
                )],
                McpError::Protocol("a protocol version this client doesn't speak"),
            ),
            (
                vec![json_answer(
                    json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": -32600, "message": "words from the server" } }),
                    &[],
                )],
                McpError::Protocol("the server answered with an error"),
            ),
            (
                vec![
                    initialized(1),
                    status(202),
                    page(2, json!([{ "name": "a" }, { "name": "a" }]), None),
                ],
                McpError::Protocol("a tool name listed twice"),
            ),
            (
                vec![
                    initialized(1),
                    status(202),
                    page(2, json!([{ "description": "no name" }]), None),
                ],
                McpError::Protocol("a tool without a usable name"),
            ),
            (
                vec![
                    initialized(1),
                    status(202),
                    page(2, json!([{ "name": "a", "inputSchema": "not an object" }]), None),
                ],
                McpError::Protocol("a tool field of the wrong type"),
            ),
            (
                vec![
                    initialized(1),
                    status(202),
                    page(2, json!([]), Some("same")),
                    page(3, json!([]), Some("same")),
                ],
                McpError::Protocol("a cursor that isn't a new string"),
            ),
            (vec![initialized(1), status(202), status(401)], McpError::Unauthorized),
        ];
        for (answers, expected) in cases {
            let server = Server::new(answers);
            assert_eq!(
                list_tools(&server, "https://tools.example/mcp", Some(TOKEN)).await,
                Err(expected)
            );
        }
    }

    #[tokio::test]
    async fn refuses_a_listing_larger_than_kept() {
        let tools: Vec<Value> = (0..=MAX_TOOLS)
            .map(|index| json!({ "name": format!("t{index}") }))
            .collect();
        let server = Server::new(vec![initialized(1), status(202), page(2, Value::Array(tools), None)]);
        assert_eq!(
            list_tools(&server, "https://tools.example/mcp", None).await,
            Err(McpError::Protocol("more tools than a listing keeps"))
        );
    }

    /// A server that repeats the credential encoded, which the outbound
    /// path's scrubbing of the raw bytes can't see, gets nothing pinned.
    #[tokio::test]
    async fn refuses_a_listing_that_repeats_the_credential() {
        const KEY: McpCredential<'static> = McpCredential {
            header: "X-Api-Key",
            value: "Key ab/cd+EF12",
            secret: "ab/cd+EF12",
        };
        let raw = |content_type: &str, body: String| {
            Ok(OutboundResponse {
                status: 200,
                headers: vec![("content-type".to_owned(), content_type.to_owned())],
                body: body.into_bytes(),
            })
        };
        let json_page = |tool: &str| {
            raw(
                "application/json",
                format!(r#"{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{tool}]}}}}"#),
            )
        };
        for answer in [
            json_page(r#"{"name":"a","description":"your key is ab\/cd+EF12"}"#),
            json_page(r#"{"name":"a","description":"\u0061b/cd+EF12"}"#),
            json_page(r#"{"name":"a","inputSchema":{"properties":{"Key ab\/cd+EF12":{}}}}"#),
            json_page(r#"{"name":"a","annotations":{"title":"ab/cd+EF12"}}"#),
            raw(
                "text/event-stream",
                "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"a\",\"title\":\"ab\\/cd+EF12\"}]}}\n\n"
                    .to_owned(),
            ),
        ] {
            let server = Server::new(vec![initialized(1), status(202), answer]);
            assert_eq!(
                list_tools(&server, "https://tools.example/mcp", Some(KEY)).await,
                Err(McpError::Protocol("a listing that repeats the credential"))
            );
        }
    }

    fn call_answer(id: u64, result: Value) -> Result<OutboundResponse, OutboundError> {
        json_answer(json!({ "jsonrpc": "2.0", "id": id, "result": result }), &[])
    }

    async fn call(server: &Server) -> Result<ToolResult, CallError> {
        let arguments = json!({ "text": "hi" }).as_object().unwrap().clone();
        call_tool(
            server,
            "https://tools.example/mcp",
            Some(TOKEN),
            "echo",
            &arguments,
            Limits::default(),
        )
        .await
    }

    #[tokio::test]
    async fn calls_a_tool_within_its_own_session() {
        let server = Server::new(vec![
            initialized(1),
            status(202),
            call_answer(
                2,
                json!({ "content": [{ "type": "text", "text": "hi" }], "isError": false, "_meta": { "trace": 1 } }),
            ),
        ]);
        let result = call(&server).await.unwrap();
        assert_eq!(
            Value::Object(result.result),
            json!({ "content": [{ "type": "text", "text": "hi" }] }),
            "the content, nothing else the server added"
        );
        assert!(!result.is_error);
        let sent = server.sent();
        assert_eq!(
            sent.iter()
                .map(|sent| sent.body["method"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["initialize", "notifications/initialized", "tools/call"]
        );
        assert_eq!(
            sent[2].body["params"],
            json!({ "name": "echo", "arguments": { "text": "hi" } })
        );
        assert!(
            sent[2]
                .headers
                .contains(&("Mcp-Session-Id".to_owned(), "session-1".to_owned()))
        );
        assert!(
            sent.iter()
                .all(|sent| sent.credential == Some(("Authorization".to_owned(), "Bearer secret-token".to_owned())))
        );

        let failed = Server::new(vec![
            initialized(1),
            status(202),
            call_answer(
                2,
                json!({ "content": [{ "type": "text", "text": "no such issue" }], "isError": true }),
            ),
        ]);
        assert!(call(&failed).await.unwrap().is_error, "a tool's own error is an answer");
    }

    #[tokio::test]
    async fn classifies_a_call_by_whether_it_may_have_acted() {
        let setup = || vec![initialized(1), status(202)];
        let with = |answer: Result<OutboundResponse, OutboundError>| {
            let mut answers = setup();
            answers.push(answer);
            answers
        };
        let cases: Vec<(Vec<Result<OutboundResponse, OutboundError>>, CallError)> = vec![
            (vec![status(401)], CallError::Unauthorized),
            (
                vec![Err(OutboundError::TimedOut)],
                CallError::NotSent(McpError::Unavailable),
            ),
            (
                vec![Err(OutboundError::Refused(Refusal::NotPublic))],
                CallError::NotSent(McpError::Refused(Refusal::NotPublic)),
            ),
            (
                with(Err(OutboundError::Unreachable)),
                CallError::NotSent(McpError::Unavailable),
            ),
            (
                with(Err(OutboundError::TimedOut)),
                CallError::Unknown("no answer in time"),
            ),
            (
                with(Err(OutboundError::Failed)),
                CallError::Unknown("the exchange failed"),
            ),
            (with(status(500)), CallError::Unknown("a server error")),
            (with(status(403)), CallError::Rejected("an unexpected status")),
            (with(status(404)), CallError::Rejected("an unexpected status")),
            (
                with(json_answer(
                    json!({ "jsonrpc": "2.0", "id": 2, "error": { "code": -32602, "message": "words" } }),
                    &[],
                )),
                CallError::Rejected("the server answered with an error"),
            ),
            (
                with(json_answer(json!({ "jsonrpc": "2.0", "id": 7, "result": {} }), &[])),
                CallError::Unknown("no usable answer"),
            ),
            (
                with(Ok(OutboundResponse {
                    status: 200,
                    headers: vec![("content-type".to_owned(), "application/json".to_owned())],
                    body:
                        br#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"secret\u002dtoken"}]}}"#
                            .to_vec(),
                })),
                CallError::Withheld,
            ),
        ];
        for (answers, expected) in cases {
            let server = Server::new(answers);
            assert_eq!(call(&server).await, Err(expected));
        }
    }
}
