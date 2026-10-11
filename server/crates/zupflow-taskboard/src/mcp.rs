//! Taskboard's MCP endpoint (Streamable HTTP, JSON answers): sessions, and
//! three tools, one per tool level of decision 0040's hints.

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

use crate::oauth::{Bearer, bearer, oauth_error};
use crate::{McpSession, Task, Taskboard, ToolCall, random};

/// The protocol versions Taskboard speaks, newest first.
const PROTOCOL_VERSIONS: [&str; 2] = ["2025-06-18", "2025-03-26"];

const SESSION_HEADER: &str = "mcp-session-id";

/// The tools, as `tools/list` answers them.
fn tools() -> Value {
    json!([
        {
            "name": "list_tasks",
            "title": "List tasks",
            "description": "Lists the tasks on the board, oldest first. Optionally only open or done ones.",
            "inputSchema": {
                "type": "object",
                "properties": { "status": { "type": "string", "enum": ["open", "done"] } },
                "additionalProperties": false,
            },
            "annotations": { "title": "List tasks", "readOnlyHint": true, "openWorldHint": false },
        },
        {
            "name": "create_task",
            "title": "Create a task",
            "description": "Adds an open task to the board.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "minLength": 1, "maxLength": 200 },
                    "notes": { "type": "string", "maxLength": 2000 },
                },
                "required": ["title"],
                "additionalProperties": false,
            },
            "annotations": {
                "title": "Create a task",
                "readOnlyHint": false,
                "destructiveHint": false,
                "idempotentHint": false,
                "openWorldHint": false,
            },
        },
        {
            "name": "delete_task",
            "title": "Delete a task",
            "description": "Deletes a task from the board for good.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false,
            },
            "annotations": {
                "title": "Delete a task",
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": true,
                "openWorldHint": false,
            },
        },
    ])
}

/// The scope each tool needs.
fn needs(tool: &str) -> Option<&'static str> {
    match tool {
        "list_tasks" => Some("tasks:read"),
        "create_task" | "delete_task" => Some("tasks:write"),
        _ => None,
    }
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(rpc_error(&Value::Null, -32600, message))).into_response()
}

fn accepts(headers: &HeaderMap, kind: &str) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|item| {
            let item = item.split(';').next().unwrap_or_default().trim();
            item.eq_ignore_ascii_case(kind) || item == "*/*"
        })
}

pub(crate) async fn get() -> Response {
    // No server-initiated stream: answers come on each POST.
    (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST, DELETE")]).into_response()
}

/// Ends a session.
pub(crate) async fn delete(State(board): State<Taskboard>, headers: HeaderMap) -> Response {
    let bearer = match bearer(&board, &headers) {
        Ok(bearer) => bearer,
        Err(refused) => return refused.answer(&board),
    };
    let Some(session) = headers.get(SESSION_HEADER).and_then(|value| value.to_str().ok()) else {
        return bad_request("Name the session with Mcp-Session-Id.");
    };
    let mut state = board.state();
    match state.sessions.get(session) {
        Some(open) if open.client_id == bearer.client_id => {
            state.sessions.remove(session);
            StatusCode::NO_CONTENT.into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(crate) async fn post(State(board): State<Taskboard>, headers: HeaderMap, body: Bytes) -> Response {
    // The token first: a client finds the authorization server from this `401`.
    let bearer = match bearer(&board, &headers) {
        Ok(bearer) => bearer,
        Err(refused) => return refused.answer(&board),
    };
    let json_body = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"));
    if !json_body {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "Send JSON-RPC as application/json.").into_response();
    }
    if !(accepts(&headers, "application/json") && accepts(&headers, "text/event-stream")) {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "Accept both application/json and text/event-stream.",
        )
            .into_response();
    }
    if let Some(version) = headers.get("mcp-protocol-version")
        && !PROTOCOL_VERSIONS
            .iter()
            .any(|known| version.as_bytes() == known.as_bytes())
    {
        return bad_request("An MCP-Protocol-Version Taskboard doesn't speak.");
    }
    let message: Value = match serde_json::from_slice(&body) {
        Ok(message) => message,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(rpc_error(&Value::Null, -32700, "Not JSON.")),
            )
                .into_response();
        }
    };
    let Value::Object(message) = message else {
        return bad_request("Send one JSON-RPC message per request; batches aren't supported.");
    };
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return bad_request("Not a JSON-RPC 2.0 message.");
    }
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        // A response or something else a client never sends here.
        return StatusCode::ACCEPTED.into_response();
    };
    let id = message.get("id").cloned();
    let params = match message.get("params") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(params)) => params.clone(),
        Some(_) => return bad_request("params must be an object."),
    };

    if method == "initialize" {
        let Some(id) = id else {
            return bad_request("initialize is a request: give it an id.");
        };
        let asked = params.get("protocolVersion").and_then(Value::as_str);
        let version = asked
            .and_then(|asked| PROTOCOL_VERSIONS.into_iter().find(|known| *known == asked))
            .unwrap_or(PROTOCOL_VERSIONS[0]);
        let session = random("tbs_");
        board.state().sessions.insert(
            session.clone(),
            McpSession {
                client_id: bearer.client_id.clone(),
                protocol_version: version,
            },
        );
        let mut response = Json(rpc_result(
            &id,
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "taskboard", "title": "Taskboard", "version": "1.0.0" },
                "instructions": "Taskboard keeps a simple list of tasks.",
            }),
        ))
        .into_response();
        if let Ok(value) = HeaderValue::try_from(session) {
            response.headers_mut().insert(SESSION_HEADER, value);
        }
        return response;
    }

    // Everything else belongs to a session this client opened.
    let Some(session) = headers.get(SESSION_HEADER).and_then(|value| value.to_str().ok()) else {
        return bad_request("Initialize first, then send Mcp-Session-Id.");
    };
    let agreed = match board.state().sessions.get(session) {
        Some(open) if open.client_id == bearer.client_id => open.protocol_version,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(rpc_error(&Value::Null, -32001, "No such session.")),
            )
                .into_response();
        }
    };
    // After initialize, every request names the version agreed.
    if headers.get("mcp-protocol-version").map(HeaderValue::as_bytes) != Some(agreed.as_bytes()) {
        return bad_request("Send the MCP-Protocol-Version agreed at initialize.");
    }
    let Some(id) = id else {
        // Notifications, `notifications/initialized` among them: accepted.
        return StatusCode::ACCEPTED.into_response();
    };
    let answer = match method {
        "ping" => rpc_result(&id, json!({})),
        "tools/list" => rpc_result(&id, json!({ "tools": tools() })),
        "tools/call" => return call(&board, &bearer, &id, &params),
        _ => rpc_error(&id, -32601, "Taskboard doesn't serve that method."),
    };
    Json(answer).into_response()
}

fn call(board: &Taskboard, bearer: &Bearer, id: &Value, params: &Map<String, Value>) -> Response {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Json(rpc_error(id, -32602, "Name the tool.")).into_response();
    };
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(arguments)) => arguments.clone(),
        Some(_) => return Json(rpc_error(id, -32602, "arguments must be an object.")).into_response(),
    };
    let Some(scope) = needs(name) else {
        return Json(rpc_error(id, -32602, "Taskboard has no such tool.")).into_response();
    };
    if !bearer.scopes.iter().any(|granted| granted == scope) {
        let mut response = oauth_error(
            StatusCode::FORBIDDEN,
            "insufficient_scope",
            "The token wasn't granted the scope this tool needs.",
        );
        if let Ok(value) = HeaderValue::try_from(format!(r#"Bearer error="insufficient_scope", scope="{scope}""#)) {
            response.headers_mut().insert(header::WWW_AUTHENTICATE, value);
        }
        return response;
    }
    // Recorded once allowed: what reached a tool, never what was refused.
    board.state().tool_calls.push(ToolCall {
        client_id: bearer.client_id.clone(),
        access_token: bearer.token.clone(),
        tool: name.to_owned(),
        arguments: Value::Object(arguments.clone()),
    });
    let result = match run(board, &bearer.client_id, name, &arguments) {
        Ok((text, structured)) => json!({
            "content": [{ "type": "text", "text": text }],
            "structuredContent": structured,
            "isError": false,
        }),
        // A tool's own error is the model's to read (MCP: errors in the result).
        Err(message) => json!({
            "content": [{ "type": "text", "text": message }],
            "isError": true,
        }),
    };
    Json(rpc_result(id, result)).into_response()
}

/// A string argument, if present; an error if it isn't a string.
fn text<'a>(arguments: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, String> {
    match arguments.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.as_str())),
        Some(_) => Err(format!("{key} must be a string.")),
    }
}

fn only(arguments: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    match arguments.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(format!("Unknown argument {key}.")),
        None => Ok(()),
    }
}

fn run(
    board: &Taskboard,
    client_id: &str,
    tool: &str,
    arguments: &Map<String, Value>,
) -> Result<(String, Value), String> {
    let mut state = board.state();
    let board = state.boards.entry(client_id.to_owned()).or_default();
    match tool {
        "list_tasks" => {
            only(arguments, &["status"])?;
            let status = text(arguments, "status")?;
            if status.is_some_and(|status| status != "open" && status != "done") {
                return Err("status must be open or done.".to_owned());
            }
            let tasks: Vec<&Task> = board
                .tasks
                .iter()
                .filter(|task| status.is_none_or(|status| task.status == status))
                .collect();
            let text = if tasks.is_empty() {
                "No tasks.".to_owned()
            } else {
                tasks
                    .iter()
                    .map(|task| format!("{} [{}] {}", task.id, task.status, task.title))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok((text, json!({ "tasks": tasks })))
        }
        "create_task" => {
            only(arguments, &["title", "notes"])?;
            let title = text(arguments, "title")?
                .map(str::trim)
                .filter(|title| !title.is_empty() && title.chars().count() <= 200)
                .ok_or("title is required: 1 to 200 characters.")?
                .to_owned();
            let notes = text(arguments, "notes")?.map(str::to_owned);
            if notes.as_ref().is_some_and(|notes| notes.chars().count() > 2000) {
                return Err("notes may be up to 2000 characters.".to_owned());
            }
            board.next += 1;
            let task = Task {
                id: format!("TB-{}", board.next),
                title,
                notes,
                status: "open".to_owned(),
            };
            board.tasks.push(task.clone());
            Ok((format!("Created {}: {}", task.id, task.title), json!({ "task": task })))
        }
        "delete_task" => {
            only(arguments, &["id"])?;
            let id = text(arguments, "id")?.ok_or("id is required.")?;
            let Some(index) = board.tasks.iter().position(|task| task.id == id) else {
                return Err(format!("There's no task {id}."));
            };
            let task = board.tasks.remove(index);
            Ok((format!("Deleted {}", task.id), json!({ "deleted": task.id })))
        }
        _ => Err("Taskboard has no such tool.".to_owned()),
    }
}
