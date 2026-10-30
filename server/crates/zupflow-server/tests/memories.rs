//! What agents remember, through the API: an agent's memories in its
//! workspace, listed one memory type at a time, newest first, and read by
//! their IDs in whichever type keeps them. Every member and the workspace's
//! API keys read them; owners, admins, developers and API keys save, change
//! and forget them, the Console in Sandbox only. Each organization's,
//! workspace's and agent's are its own, and a kind this server doesn't serve
//! keeps its memories out of reach. Each change is sent to the endpoints of
//! its workspace's environment and audited, never with its text.

#![allow(clippy::unwrap_used)]

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::{Method, StatusCode};
use common::{Server, call, code};
use serde_json::{Map, Value, json};
use zupflow_core::ids::{MemoryId, MemoryTypeId, OrganizationId};
use zupflow_core::memory::notes::Notes;
use zupflow_core::memory::registry::MemoryProviders;
use zupflow_core::memory::{
    Audience, CallContext, CallResult, Forget, KindDescription, Manage, Memory, MemoryError, MemoryFilter, MemoryKind,
    MemoryProvider, MemoryScope, MemoryTool, MemoryWrites, Origin, Recall, Rewritten, Settings, SettingsRefusal,
};
use zupflow_core::time::Timestamp;
use zupflow_core::workspaces::Page;
use zupflow_server::webhook_delivery;
use zupflow_sqlite::SqliteNoteStore;

/// IDs that name no memory: one of a memory's form that doesn't exist, and
/// a malformed one.
const MISSING_MEMORIES: [&str; 2] = ["memo_0000000000000000000000", "not-a-memory"];

/// IDs that name no type: one of a type's form that doesn't exist, and a
/// malformed one.
const MISSING_TYPES: [&str; 2] = ["mtype_0000000000000000000000", "not-a-memory-type"];

const HOOK: &str = "https://hooks.example/memories";

/// A request: its method, path and body.
type Request = (Method, String, Option<Value>);

fn id(value: &Value) -> &str {
    value["id"].as_str().unwrap()
}

fn memories_path(ws: &str, agent: &str) -> String {
    format!("/v1/workspaces/{ws}/agents/{agent}/memories")
}

fn memory_path(ws: &str, agent: &str, memory: &str) -> String {
    format!("/v1/workspaces/{ws}/agents/{agent}/memories/{memory}")
}

/// A refusal as its status, its code and the fields it names.
fn refusal(status: StatusCode, body: &Value) -> (StatusCode, String, Vec<String>) {
    let fields = body["error"]["fields"]
        .as_object()
        .map(|fields| fields.keys().cloned().collect())
        .unwrap_or_default();
    (status, code(body).to_owned(), fields)
}

fn refused(status: StatusCode, code: &str, fields: &[&str]) -> (StatusCode, String, Vec<String>) {
    (
        status,
        code.to_owned(),
        fields.iter().map(|field| (*field).to_owned()).collect(),
    )
}

fn not_found() -> (StatusCode, String, Vec<String>) {
    refused(StatusCode::NOT_FOUND, "not_found", &[])
}

/// Where a case works: Northwind, with its owner, a viewer and a
/// developer; a Sandbox and a Production workspace; a `notes` type; and an
/// agent remembering in it in each workspace.
struct Tenant {
    org: String,
    sandbox: String,
    production: String,
    memory_type: String,
    agent: String,
    production_agent: String,
}

async fn tenant(server: &Server) -> Tenant {
    let org = server.organization("owner", "northwind").await;
    server.member("owner", &org, "viewer", "viewer").await;
    server.member("owner", &org, "developer", "developer").await;
    let sandbox = id(&server.workspace("owner", &org, "acme-sandbox", "sandbox").await).to_owned();
    let production = id(&server.workspace("owner", &org, "acme", "production").await).to_owned();
    let memory_type = notes_type(server, &org, "Notes", json!({})).await;
    let agent = agent(server, &org, &sandbox, "Keeper", &memory_type).await;
    let production_agent = agent_with(server, &org, &production, "Reporter", type_memory(&memory_type)).await;
    Tenant {
        org,
        sandbox,
        production,
        memory_type,
        agent,
        production_agent,
    }
}

/// A `notes` type of `org` with `settings`: its ID.
async fn notes_type(server: &Server, org: &str, name: &str, settings: Value) -> String {
    let (status, body) = server
        .send(
            call("owner", Method::POST, format!("/v1/organizations/{org}/memory-types"))
                .body(json!({ "name": name, "kind": "notes", "settings": settings })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    id(&body["data"]).to_owned()
}

fn type_memory(memory_type: &str) -> Value {
    json!({ "memory": "type", "memory_type_id": memory_type })
}

/// An agent of `ws` remembering in `memory_type`: its ID.
async fn agent(server: &Server, org: &str, ws: &str, name: &str, memory_type: &str) -> String {
    agent_with(server, org, ws, name, type_memory(memory_type)).await
}

/// An agent of `ws` with `memory`, its `memory` and `memory_type_id` as
/// the body sends them: its ID.
async fn agent_with(server: &Server, org: &str, ws: &str, name: &str, memory: Value) -> String {
    let mut body = json!({ "name": name, "description": "", "instructions": "" });
    body.as_object_mut()
        .unwrap()
        .extend(memory.as_object().unwrap().clone());
    let (status, body) = server
        .send(
            call("owner", Method::POST, format!("/v1/workspaces/{ws}/agents"))
                .naming(org)
                .body(body),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    id(&body["data"]).to_owned()
}

/// A key of `org`'s `environment`: its secret and ID.
async fn api_key(server: &Server, org: &str, environment: &str) -> (String, String) {
    let (status, body) = server
        .send(
            call("owner", Method::POST, format!("/v1/organizations/{org}/api-keys"))
                .body(json!({ "name": "Backend", "environment": environment })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    (
        body["data"]["secret"].as_str().unwrap().to_owned(),
        body["data"]["key"]["id"].as_str().unwrap().to_owned(),
    )
}

/// Asks `who` to save `body` for the agent: the status and the answer.
async fn save(server: &Server, who: &str, org: &str, ws: &str, agent: &str, body: Value) -> (StatusCode, Value) {
    server
        .send(call(who, Method::POST, memories_path(ws, agent)).naming(org).body(body))
        .await
}

/// Saves `text` for the agent as `who`: the memory.
async fn saved(server: &Server, who: &str, org: &str, ws: &str, agent: &str, text: &str) -> Value {
    let (status, body) = save(server, who, org, ws, agent, json!({ "text": text })).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["data"].clone()
}

/// Lists the agent's memories as `who`, with `query`: the whole answer.
async fn listed(server: &Server, who: &str, org: &str, ws: &str, agent: &str, query: &str) -> Value {
    let (status, body) = server
        .send(call(who, Method::GET, format!("{}{query}", memories_path(ws, agent))).naming(org))
        .await;
    assert_eq!(status, StatusCode::OK, "{query}: {body}");
    body
}

async fn texts(server: &Server, org: &str, ws: &str, agent: &str, query: &str) -> Vec<String> {
    listed(server, "owner", org, ws, agent, query).await["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|memory| memory["text"].as_str().unwrap().to_owned())
        .collect()
}

async fn update(
    server: &Server,
    who: &str,
    org: &str,
    ws: &str,
    agent: &str,
    memory: &str,
    body: Value,
) -> (StatusCode, Value) {
    server
        .send(
            call(who, Method::PATCH, memory_path(ws, agent, memory))
                .naming(org)
                .body(body),
        )
        .await
}

async fn get(server: &Server, who: &str, org: &str, ws: &str, agent: &str, memory: &str) -> (StatusCode, Value) {
    server
        .send(call(who, Method::GET, memory_path(ws, agent, memory)).naming(org))
        .await
}

async fn forget(server: &Server, who: &str, org: &str, ws: &str, agent: &str, memory: &str) -> (StatusCode, Value) {
    server
        .send(call(who, Method::DELETE, memory_path(ws, agent, memory)).naming(org))
        .await
}

async fn forget_all(server: &Server, who: &str, org: &str, ws: &str, agent: &str) -> (StatusCode, Value) {
    server
        .send(call(who, Method::DELETE, memories_path(ws, agent)).naming(org))
        .await
}

/// The signed-in person's user ID.
async fn user_id(server: &Server, who: &str) -> String {
    let (_, me) = server.send(call(who, Method::GET, "/v1/me")).await;
    me["data"]["user"]["id"].as_str().unwrap().to_owned()
}

/// Serves `provider` as the only memory kind, from now on.
fn serve(server: &mut Server, providers: MemoryProviders) {
    server.state.memory_providers = providers;
    server.app = zupflow_server::app(server.state.clone(), zupflow_server::RequestGuard::default());
}

#[tokio::test]
async fn saves_a_memory_as_its_caller_wrote_it_and_reads_it_back() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let (org, ws, agent) = (t.org.as_str(), t.sandbox.as_str(), t.agent.as_str());
    let owner_id = user_id(&server, "owner").await;
    let first = saved(
        &server,
        "owner",
        org,
        ws,
        agent,
        "  Prefers answers\r\nin Portuguese.\u{7}  ",
    )
    .await;
    let mut fields: Vec<&str> = first.as_object().unwrap().keys().map(String::as_str).collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        [
            "agent_id",
            "channel_conversation_id",
            "created_at",
            "id",
            "memory_type_id",
            "origin",
            "short_id",
            "text",
            "updated_at",
            "workspace_id"
        ],
        "never its organization's ID"
    );
    assert!(id(&first).starts_with("memo_"));
    assert_eq!(
        (
            &first["workspace_id"],
            &first["agent_id"],
            &first["memory_type_id"],
            &first["text"],
            &first["channel_conversation_id"],
            &first["origin"],
        ),
        (
            &json!(ws),
            &json!(agent),
            &json!(t.memory_type),
            &json!("Prefers answers in Portuguese."),
            &Value::Null,
            &json!({ "type": "member", "user_id": owner_id }),
        ),
        "one line, for the whole workspace, written by its caller"
    );
    let short_id = first["short_id"].as_str().unwrap();
    assert!(
        short_id.len() == 6
            && short_id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b)),
        "{short_id}"
    );
    assert_eq!(first["created_at"], first["updated_at"]);
    // An API key writes as itself.
    let (key, key_id) = api_key(&server, org, "sandbox").await;
    let second = saved(&server, &key, org, ws, agent, "Ships on Thursdays.").await;
    assert_eq!(second["origin"], json!({ "type": "api_key", "api_key_id": key_id }));
    assert_ne!(second["short_id"], first["short_id"]);

    // Newest first; every member and key reads the same.
    let all = listed(&server, "owner", org, ws, agent, "").await;
    assert_eq!(all["data"], json!([second, first]));
    assert_eq!(all["meta"]["pagination"], json!({ "next_cursor": null, "total": 2 }));
    let named = format!("?memory_type={}", t.memory_type);
    assert_eq!(listed(&server, "owner", org, ws, agent, &named).await, all);
    for who in ["viewer", "developer", key.as_str()] {
        assert_eq!(listed(&server, who, org, ws, agent, "").await, all, "{who}");
        for memory in [&first, &second] {
            let (status, body) = get(&server, who, org, ws, agent, id(memory)).await;
            assert_eq!((status, &body["data"]), (StatusCode::OK, memory), "{who}");
        }
    }

    // The text it holds already changes nothing, not even who wrote it; a
    // real change is the caller's, now.
    let (status, same) = update(
        &server,
        &key,
        org,
        ws,
        agent,
        id(&first),
        json!({ "text": " Prefers answers\tin Portuguese.\n" }),
    )
    .await;
    assert_eq!((status, &same["data"]), (StatusCode::OK, &first));
    let (status, changed) = update(
        &server,
        &key,
        org,
        ws,
        agent,
        id(&first),
        json!({ "text": "Prefers answers in English." }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    let changed = changed["data"].clone();
    assert_eq!(changed["text"], "Prefers answers in English.");
    assert_eq!(changed["origin"], second["origin"], "written by the key now");
    assert_eq!(
        (&changed["created_at"], &changed["short_id"]),
        (&first["created_at"], &first["short_id"])
    );
    assert!(changed["updated_at"].as_str().unwrap() >= first["updated_at"].as_str().unwrap());
    assert_eq!(
        get(&server, "viewer", org, ws, agent, id(&first)).await.1["data"],
        changed
    );
    assert_eq!(
        listed(&server, "owner", org, ws, agent, "").await["data"],
        json!([second, changed]),
        "a change moves nothing"
    );

    // Forgotten once, then not there to read, change or forget.
    let (status, body) = forget(&server, "developer", org, ws, agent, id(&first)).await;
    assert_eq!((status, body), (StatusCode::NO_CONTENT, Value::Null));
    let (status, body) = get(&server, "owner", org, ws, agent, id(&first)).await;
    assert_eq!(refusal(status, &body), not_found());
    let (status, body) = update(&server, "owner", org, ws, agent, id(&first), json!({ "text": "Back." })).await;
    assert_eq!(refusal(status, &body), not_found());
    let (status, body) = forget(&server, "owner", org, ws, agent, id(&first)).await;
    assert_eq!(refusal(status, &body), not_found());
    assert_eq!(
        listed(&server, "owner", org, ws, agent, "").await["data"],
        json!([second])
    );

    // Everything at once, in every type, then nothing left to forget.
    let other_type = notes_type(&server, org, "Other", json!({})).await;
    let elsewhere = save(
        &server,
        "owner",
        org,
        ws,
        agent,
        json!({ "text": "Kept in another type.", "memory_type_id": other_type }),
    )
    .await
    .1["data"]
        .clone();
    assert_eq!(elsewhere["memory_type_id"], json!(other_type));
    assert_eq!(
        get(&server, "owner", org, ws, agent, id(&elsewhere)).await.1["data"],
        elsewhere,
        "found in whichever type keeps it"
    );
    for _ in 0..2 {
        let (status, body) = forget_all(&server, "developer", org, ws, agent).await;
        assert_eq!((status, body), (StatusCode::NO_CONTENT, Value::Null));
    }
    for memory in [&second, &elsewhere] {
        let (status, body) = get(&server, "owner", org, ws, agent, id(memory)).await;
        assert_eq!(refusal(status, &body), not_found());
    }
    assert!(texts(&server, org, ws, agent, "").await.is_empty());
    let other = format!("?memory_type={other_type}");
    assert!(texts(&server, org, ws, agent, &other).await.is_empty());
}

#[tokio::test]
async fn keeps_text_as_the_notes_kind_does_and_refuses_what_it_does_not_keep() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let (org, ws, agent) = (t.org.as_str(), t.sandbox.as_str(), t.agent.as_str());
    for (sent, kept) in [
        ("a\r\nb\nc\rd\te", "a b c d e"),
        ("tag\u{E0001}\u{E0041}s", "tags"),
        ("zero\u{200B}width", "zerowidth"),
        ("👩\u{200D}💻 codes", "👩\u{200D}💻 codes"),
        (
            "\u{A0}\u{3000} Prefers Português 🙂\u{2003}\u{85}",
            "Prefers Português 🙂",
        ),
    ] {
        assert_eq!(
            saved(&server, "owner", org, ws, agent, sent).await["text"],
            kept,
            "{sent:?}"
        );
    }
    let longest = saved(&server, "owner", org, ws, agent, &format!("  {}\n ", "é".repeat(500))).await;
    assert_eq!(longest["text"].as_str().unwrap().chars().count(), 500);
    let before = texts(&server, org, ws, agent, "?limit=100").await;
    for text in [
        "",
        "   ",
        "\n\t \u{0}",
        "\u{200B}\u{FEFF}",
        &"x".repeat(501),
        &"🙂".repeat(501),
    ] {
        let (status, body) = save(&server, "owner", org, ws, agent, json!({ "text": text })).await;
        assert_eq!(
            refusal(status, &body),
            refused(StatusCode::UNPROCESSABLE_ENTITY, "invalid_memory_text", &["text"]),
            "{text:?}"
        );
        assert_eq!(
            body["error"]["fields"]["text"], "Write a note of 1 to 500 characters on one line.",
            "what a note is"
        );
    }
    // What isn't text at all is the contract's refusal, as is a type that
    // isn't an ID, every field named at once.
    for (body, fields) in [
        (json!({}), vec!["text"]),
        (json!({ "text": 42 }), vec!["text"]),
        (json!({ "text": null }), vec!["text"]),
        (json!({ "text": "x", "memory_type_id": 42 }), vec!["memory_type_id"]),
        (json!({ "text": "x", "memory_type_id": null }), vec!["memory_type_id"]),
        (json!({ "memory_type_id": false }), vec!["memory_type_id", "text"]),
    ] {
        let (status, answer) = save(&server, "owner", org, ws, agent, body.clone()).await;
        assert_eq!(
            refusal(status, &answer),
            refused(StatusCode::BAD_REQUEST, "validation_failed", &fields),
            "{body}"
        );
    }
    assert_eq!(
        texts(&server, org, ws, agent, "?limit=100").await,
        before,
        "nothing saved"
    );

    // A change is kept the same way, and a refused one changes nothing.
    let memory = saved(&server, "owner", org, ws, agent, "Prefers email.").await;
    let (status, body) = update(
        &server,
        "owner",
        org,
        ws,
        agent,
        id(&memory),
        json!({ "text": "One\nline." }),
    )
    .await;
    assert_eq!(
        (status, body["data"]["text"].as_str()),
        (StatusCode::OK, Some("One line."))
    );
    for (body, expected) in [
        (
            json!({ "text": " \n " }),
            refused(StatusCode::UNPROCESSABLE_ENTITY, "invalid_memory_text", &["text"]),
        ),
        (
            json!({ "text": "x".repeat(501) }),
            refused(StatusCode::UNPROCESSABLE_ENTITY, "invalid_memory_text", &["text"]),
        ),
        (
            json!({ "text": 7 }),
            refused(StatusCode::BAD_REQUEST, "validation_failed", &["text"]),
        ),
        (
            json!({}),
            refused(StatusCode::BAD_REQUEST, "validation_failed", &["text"]),
        ),
    ] {
        let (status, answer) = update(&server, "owner", org, ws, agent, id(&memory), body.clone()).await;
        assert_eq!(refusal(status, &answer), expected, "{body}");
    }
    assert_eq!(
        get(&server, "owner", org, ws, agent, id(&memory)).await.1["data"]["text"],
        "One line."
    );
    // A memory that isn't there is that, before anything about the text.
    for missing in MISSING_MEMORIES {
        for body in [
            json!({ "text": "x" }),
            json!({ "text": " \n " }),
            json!({ "text": "x".repeat(501) }),
            json!({ "text": 7 }),
        ] {
            let (status, answer) = update(&server, "owner", org, ws, agent, missing, body.clone()).await;
            assert_eq!(refusal(status, &answer), not_found(), "{missing} {body}");
        }
        let (status, answer) = get(&server, "owner", org, ws, agent, missing).await;
        assert_eq!(refusal(status, &answer), not_found(), "{missing}");
        let (status, answer) = forget(&server, "owner", org, ws, agent, missing).await;
        assert_eq!(refusal(status, &answer), not_found(), "{missing}");
    }
}

#[tokio::test]
async fn saves_in_the_type_named_or_else_the_agents_own() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let (org, ws) = (t.org.as_str(), t.sandbox.as_str());
    let other_type = notes_type(&server, org, "Other", json!({})).await;
    let off = agent_with(&server, org, ws, "Forgetful", json!({ "memory": "off" })).await;
    let on_default = agent_with(&server, org, ws, "Usual", json!({ "memory": "default" })).await;

    // No type to save to: its memory is off, or the organization has no default.
    for agent in [&off, &on_default] {
        let (status, body) = save(&server, "owner", org, ws, agent, json!({ "text": "Nowhere." })).await;
        assert_eq!(
            refusal(status, &body),
            refused(StatusCode::UNPROCESSABLE_ENTITY, "memory_off", &[])
        );
        // An empty page, whatever else the list asks.
        for query in ["", "?cursor=not-a-cursor&q=anything&limit=0"] {
            let page = listed(&server, "owner", org, ws, agent, query).await;
            assert_eq!(
                (&page["data"], &page["meta"]["pagination"]),
                (&json!([]), &json!({ "next_cursor": null, "total": 0 })),
                "{query}"
            );
        }
    }
    // A type named is used, even while the agent's memory is off.
    let (status, body) = save(
        &server,
        "owner",
        org,
        ws,
        &off,
        json!({ "text": "Named.", "memory_type_id": other_type }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["data"]["memory_type_id"], json!(other_type));
    let named = format!("?memory_type={other_type}");
    assert_eq!(texts(&server, org, ws, &off, &named).await, ["Named."]);
    assert!(texts(&server, org, ws, &off, "").await.is_empty());

    // An agent on the default saves in it once there is one.
    let (status, body) = server
        .send(
            call(
                "owner",
                Method::PUT,
                format!("/v1/organizations/{org}/default-memory-type"),
            )
            .body(json!({ "memory_type_id": other_type })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let usual = saved(&server, "owner", org, ws, &on_default, "On the default.").await;
    assert_eq!(usual["memory_type_id"], json!(other_type));
    assert_eq!(texts(&server, org, ws, &on_default, "").await, ["On the default."]);

    // A type that isn't the organization's is no type: to save in, or list.
    let theirs = server.organization("outsider", "larkspur").await;
    let their_type = {
        let (status, body) = server
            .send(
                call(
                    "outsider",
                    Method::POST,
                    format!("/v1/organizations/{theirs}/memory-types"),
                )
                .body(json!({ "name": "Theirs", "kind": "notes" })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        id(&body["data"]).to_owned()
    };
    for named in MISSING_TYPES.iter().copied().chain([their_type.as_str(), ""]) {
        let (status, body) = save(
            &server,
            "owner",
            org,
            ws,
            &t.agent,
            json!({ "text": "Misnamed.", "memory_type_id": named }),
        )
        .await;
        assert_eq!(
            refusal(status, &body),
            refused(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unknown_memory_type",
                &["memory_type_id"]
            ),
            "{named}"
        );
        let (status, body) = server
            .send(
                call(
                    "owner",
                    Method::GET,
                    format!("{}?memory_type={named}", memories_path(ws, &t.agent)),
                )
                .naming(org),
            )
            .await;
        assert_eq!(
            refusal(status, &body),
            refused(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unknown_memory_type",
                &["memory_type"]
            ),
            "{named}"
        );
    }
    assert!(texts(&server, org, ws, &t.agent, "").await.is_empty(), "nothing saved");
}

#[tokio::test]
async fn keeps_each_agent_to_its_types_most_notes() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let (org, ws) = (t.org.as_str(), t.sandbox.as_str());
    let small = notes_type(&server, org, "Small", json!({ "max_notes": 2 })).await;
    let other_type = notes_type(&server, org, "Roomy", json!({})).await;
    let agent = agent(&server, org, ws, "Packed", &small).await;
    let neighbor = self::agent(&server, org, ws, "Neighbor", &small).await;
    let first = saved(&server, "owner", org, ws, &agent, "First.").await;
    saved(&server, "owner", org, ws, &agent, "Second.").await;
    let (status, body) = save(&server, "owner", org, ws, &agent, json!({ "text": "Third." })).await;
    assert_eq!(
        refusal(status, &body),
        refused(StatusCode::UNPROCESSABLE_ENTITY, "memory_full", &[])
    );
    assert_eq!(
        body["error"]["message"],
        "Packed keeps 2 memories at most in Small. Forget one, or change one instead."
    );
    // Another agent and another type each have room of their own.
    saved(&server, "owner", org, ws, &neighbor, "The neighbor's.").await;
    let (status, body) = save(
        &server,
        "owner",
        org,
        ws,
        &agent,
        json!({ "text": "Elsewhere.", "memory_type_id": other_type }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // A change needs no room; forgetting one makes some.
    let (status, body) = update(
        &server,
        "owner",
        org,
        ws,
        &agent,
        id(&first),
        json!({ "text": "First, changed." }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = forget(&server, "owner", org, ws, &agent, id(&first)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    saved(&server, "owner", org, ws, &agent, "Third.").await;

    // Fewer allowed than it keeps: it keeps them all, and saves no more.
    let (status, body) = server
        .send(
            call(
                "owner",
                Method::PATCH,
                format!("/v1/organizations/{org}/memory-types/{small}"),
            )
            .body(json!({ "settings": { "max_notes": 1 } })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(texts(&server, org, ws, &agent, "").await, ["Third.", "Second."]);
    let (status, body) = save(&server, "owner", org, ws, &agent, json!({ "text": "Fourth." })).await;
    assert_eq!(
        refusal(status, &body),
        refused(StatusCode::UNPROCESSABLE_ENTITY, "memory_full", &[])
    );
}

#[tokio::test]
async fn pages_and_searches_newest_first() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let (org, ws, agent) = (t.org.as_str(), t.sandbox.as_str(), t.agent.as_str());
    let notes = [
        "Prefers answers in Portuguese.",
        "Invoices go to the finance team.",
        "The finance team meets on Mondays.",
        "Café com leite, não chá.",
        "Call or email, never text.",
    ];
    for note in notes {
        saved(&server, "owner", org, ws, agent, note).await;
    }
    let all = listed(&server, "owner", org, ws, agent, "?limit=100").await;
    let newest_first: Vec<&str> = notes.iter().rev().copied().collect();
    assert_eq!(texts(&server, org, ws, agent, "").await, newest_first);
    // A page at a time, the same items in the same order.
    let mut paged = Vec::new();
    let mut query = "?limit=2".to_owned();
    loop {
        let page = listed(&server, "owner", org, ws, agent, &query).await;
        assert_eq!(page["meta"]["pagination"]["total"], 5, "every match, on every page");
        paged.extend(page["data"].as_array().unwrap().iter().cloned());
        match page["meta"]["pagination"]["next_cursor"].as_str() {
            Some(cursor) => query = format!("?limit=2&cursor={cursor}"),
            None => break,
        }
    }
    assert_eq!(Value::Array(paged), all["data"]);
    assert_eq!(
        listed(&server, "owner", org, ws, agent, "?limit=0").await["data"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        listed(&server, "owner", org, ws, agent, "?limit=1000").await["data"]
            .as_array()
            .unwrap()
            .len(),
        5
    );

    // Every word, in any case and accent.
    let found = |q: &'static str| {
        let server = &server;
        async move {
            let query = format!("?{}", serde_urlencoded::to_string([("q", q)]).unwrap());
            texts(server, org, ws, agent, &query).await
        }
    };
    assert_eq!(found("FINANCE mondays").await, ["The finance team meets on Mondays."]);
    assert_eq!(
        found("finance").await,
        ["The finance team meets on Mondays.", "Invoices go to the finance team."]
    );
    assert!(found("finance portuguese").await.is_empty());
    assert_eq!(found("cafe NAO").await, ["Café com leite, não chá."]);
    assert_eq!(found("OR").await, ["Call or email, never text."]);
    assert!(found("NOT finance").await.is_empty());
    assert_eq!(
        found("\"finance\" mondays*").await,
        ["The finance team meets on Mondays."]
    );
    assert!(found("fin").await.is_empty());
    assert_eq!(found(" \t ").await, newest_first, "blank is no filter");
    let none = listed(&server, "owner", org, ws, agent, "?q=%3F%21+--+...").await;
    assert_eq!(
        (&none["data"], &none["meta"]["pagination"]),
        (&json!([]), &json!({ "next_cursor": null, "total": 0 })),
        "no word to look for"
    );
    let first = listed(&server, "owner", org, ws, agent, "?q=finance&limit=1").await;
    assert_eq!(first["meta"]["pagination"]["total"], 2);
    let cursor = first["meta"]["pagination"]["next_cursor"].as_str().unwrap();
    let second = listed(
        &server,
        "owner",
        org,
        ws,
        agent,
        &format!("?q=finance&limit=1&cursor={cursor}"),
    )
    .await;
    assert_eq!(second["data"][0]["text"], "Invoices go to the finance team.");
    assert_eq!(second["meta"]["pagination"]["next_cursor"], Value::Null);

    // Only this list's cursors, never another list's.
    notes_type(&server, org, "A second type", json!({})).await;
    let (_, types) = server
        .send(call(
            "owner",
            Method::GET,
            format!("/v1/organizations/{org}/memory-types?limit=1"),
        ))
        .await;
    let types_cursor = types["meta"]["pagination"]["next_cursor"].as_str().unwrap().to_owned();
    for cursor in ["%25%25%25", "not-a-cursor", types_cursor.as_str()] {
        let (status, body) = server
            .send(
                call(
                    "owner",
                    Method::GET,
                    format!("{}?cursor={cursor}", memories_path(ws, agent)),
                )
                .naming(org),
            )
            .await;
        assert_eq!(
            refusal(status, &body),
            refused(StatusCode::BAD_REQUEST, "validation_failed", &["cursor"]),
            "{cursor}"
        );
    }
}

#[tokio::test]
async fn lets_developers_write_and_the_console_only_read_in_production() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let org = t.org.as_str();
    let (sandbox, agent) = (t.sandbox.as_str(), t.agent.as_str());
    let kept = saved(&server, "owner", org, sandbox, agent, "Kept.").await;
    // Every write: save, change, forget one and forget all.
    let writes = |ws: &str, agent: &str, memory: &str| -> Vec<Request> {
        vec![
            (
                Method::POST,
                memories_path(ws, agent),
                Some(json!({ "text": "Written." })),
            ),
            (
                Method::PATCH,
                memory_path(ws, agent, memory),
                Some(json!({ "text": "Rewritten." })),
            ),
            (Method::DELETE, memory_path(ws, agent, memory), None),
            (Method::DELETE, memories_path(ws, agent), None),
        ]
    };
    let attempt = |who: &'static str, method: Method, path: String, body: Option<Value>| {
        let mut request = call(who, method, path).naming(org);
        if let Some(body) = body {
            request = request.body(body);
        }
        server.send(request)
    };

    // A viewer reads, and changes nothing.
    let before = listed(&server, "owner", org, sandbox, agent, "").await;
    for (method, path, body) in writes(sandbox, agent, id(&kept)) {
        let (status, answer) = attempt("viewer", method.clone(), path.clone(), body).await;
        assert_eq!(
            refusal(status, &answer),
            refused(StatusCode::FORBIDDEN, "forbidden", &[]),
            "{method} {path}"
        );
    }
    assert_eq!(listed(&server, "viewer", org, sandbox, agent, "").await, before);
    assert_eq!(
        get(&server, "viewer", org, sandbox, agent, id(&kept)).await.1["data"],
        kept
    );

    // A developer saves, changes, forgets one and forgets all in Sandbox.
    let written = saved(&server, "developer", org, sandbox, agent, "Written.").await;
    let (status, body) = update(
        &server,
        "developer",
        org,
        sandbox,
        agent,
        id(&written),
        json!({ "text": "Rewritten." }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        forget(&server, "developer", org, sandbox, agent, id(&written)).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        forget_all(&server, "developer", org, sandbox, agent).await.0,
        StatusCode::NO_CONTENT
    );
    assert!(texts(&server, org, sandbox, agent, "").await.is_empty());

    // In Production the Console reads only; the product writes with a key.
    let (production, reporter) = (t.production.as_str(), t.production_agent.as_str());
    let (key, _) = api_key(&server, org, "production").await;
    let theirs = saved(&server, &key, org, production, reporter, "Saved by the product.").await;
    for who in ["owner", "developer", "viewer"] {
        let page = listed(&server, who, org, production, reporter, "").await;
        assert_eq!(page["data"], json!([theirs]), "{who}");
        assert_eq!(
            get(&server, who, org, production, reporter, id(&theirs)).await.1["data"],
            theirs
        );
    }
    // The role first: a viewer is refused for it, there too.
    for (who, expected) in [
        ("owner", "console_read_only_in_production"),
        ("developer", "console_read_only_in_production"),
        ("viewer", "forbidden"),
    ] {
        for (method, path, body) in writes(production, reporter, id(&theirs)) {
            let (status, answer) = attempt(who, method.clone(), path.clone(), body).await;
            assert_eq!(
                refusal(status, &answer),
                refused(StatusCode::FORBIDDEN, expected, &[]),
                "{who}: {method} {path}"
            );
        }
    }
    let (status, body) = update(
        &server,
        &key,
        org,
        production,
        reporter,
        id(&theirs),
        json!({ "text": "Changed by the product." }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        forget(&server, &key, org, production, reporter, id(&theirs)).await.0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        forget_all(&server, &key, org, production, reporter).await.0,
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn keeps_memories_to_their_organization_workspace_agent_and_environment() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let (org, sandbox, agent) = (t.org.as_str(), t.sandbox.as_str(), t.agent.as_str());
    let qa = id(&server.workspace("owner", org, "qa-playground", "sandbox").await).to_owned();
    let neighbor = self::agent(&server, org, sandbox, "Neighbor", &t.memory_type).await;
    let player = self::agent(&server, org, &qa, "Player", &t.memory_type).await;
    let mine = saved(&server, "owner", org, sandbox, agent, "Mine.").await;
    let next_door = saved(&server, "owner", org, sandbox, &neighbor, "The neighbor's.").await;
    let played = saved(&server, "owner", org, &qa, &player, "The playground's.").await;

    let every_route = |ws: &str, agent: &str, memory: &str| -> Vec<Request> {
        vec![
            (Method::GET, memories_path(ws, agent), None),
            (
                Method::POST,
                memories_path(ws, agent),
                Some(json!({ "text": "Planted." })),
            ),
            (Method::GET, memory_path(ws, agent, memory), None),
            (
                Method::PATCH,
                memory_path(ws, agent, memory),
                Some(json!({ "text": "Owned." })),
            ),
            (
                Method::PATCH,
                memory_path(ws, agent, memory),
                Some(json!({ "text": "" })),
            ),
            (Method::DELETE, memory_path(ws, agent, memory), None),
            (Method::DELETE, memories_path(ws, agent), None),
        ]
    };
    let attempt = |who: String, naming: String, (method, path, body): Request| {
        let server = &server;
        async move {
            let mut request = call(&who, method, path).naming(&naming);
            if let Some(body) = body {
                request = request.body(body);
            }
            server.send(request).await
        }
    };
    // Who, the organization they name, and what they ask.
    let mut attempts: Vec<(String, String, Request)> = Vec::new();
    // Another agent's memory, through this one.
    for route in every_route(sandbox, agent, id(&next_door)).into_iter().skip(2).take(4) {
        attempts.push(("owner".to_owned(), org.to_owned(), route));
    }
    // The agent through another workspace, and that workspace's agent with this one's memory.
    for route in every_route(&qa, agent, id(&mine)) {
        attempts.push(("owner".to_owned(), org.to_owned(), route));
    }
    for route in every_route(&qa, &player, id(&mine)).into_iter().skip(2).take(4) {
        attempts.push(("owner".to_owned(), org.to_owned(), route));
    }
    // Another organization's owner, through Northwind's workspace or their own, and with their key.
    let theirs = server.organization("outsider", "larkspur").await;
    let harbor = id(&server.workspace("outsider", &theirs, "harbor-sandbox", "sandbox").await).to_owned();
    let (status, body) = server
        .send(
            call("outsider", Method::POST, format!("/v1/workspaces/{harbor}/agents"))
                .naming(&theirs)
                .body(json!({ "name": "Spy", "description": "", "instructions": "", "memory": "off" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let their_agent = id(&body["data"]).to_owned();
    let (status, body) = server
        .send(
            call("outsider", Method::POST, format!("/v1/organizations/{theirs}/api-keys"))
                .body(json!({ "name": "Spy", "environment": "sandbox" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let their_key = body["data"]["secret"].as_str().unwrap().to_owned();
    for route in every_route(sandbox, agent, id(&mine)) {
        attempts.push(("outsider".to_owned(), org.to_owned(), route.clone()));
        attempts.push(("outsider".to_owned(), theirs.clone(), route.clone()));
        attempts.push((their_key.clone(), theirs.clone(), route));
    }
    for route in every_route(&harbor, agent, id(&mine)) {
        attempts.push(("outsider".to_owned(), theirs.clone(), route));
    }
    // A key of the other environment.
    let (sandbox_key, _) = api_key(&server, org, "sandbox").await;
    let (production_key, _) = api_key(&server, org, "production").await;
    let reporter_memory = saved(
        &server,
        &production_key,
        org,
        &t.production,
        &t.production_agent,
        "Production's.",
    )
    .await;
    for route in every_route(&t.production, &t.production_agent, id(&reporter_memory)) {
        attempts.push((sandbox_key.clone(), org.to_owned(), route));
    }
    for route in every_route(sandbox, agent, id(&mine)) {
        attempts.push((production_key.clone(), org.to_owned(), route));
    }
    for (who, naming, route) in attempts {
        let described = format!("{} {} as {who}", route.0, route.1);
        let (status, body) = attempt(who, naming, route).await;
        assert_eq!(refusal(status, &body), not_found(), "{described}: {body}");
    }

    // A type of this organization names none of theirs.
    let (status, body) = save(
        &server,
        "outsider",
        &theirs,
        &harbor,
        &their_agent,
        json!({ "text": "Planted.", "memory_type_id": t.memory_type }),
    )
    .await;
    assert_eq!(
        refusal(status, &body),
        refused(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_memory_type",
            &["memory_type_id"]
        )
    );
    let (status, body) = server
        .send(
            call(
                "outsider",
                Method::GET,
                format!("{}?memory_type={}", memories_path(&harbor, &their_agent), t.memory_type),
            )
            .naming(&theirs),
        )
        .await;
    assert_eq!(
        refusal(status, &body),
        refused(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_memory_type",
            &["memory_type"]
        )
    );

    // Everything as it was.
    assert_eq!(
        listed(&server, "owner", org, sandbox, agent, "").await["data"],
        json!([mine])
    );
    assert_eq!(
        listed(&server, "owner", org, sandbox, &neighbor, "").await["data"],
        json!([next_door])
    );
    assert_eq!(
        listed(&server, "owner", org, &qa, &player, "").await["data"],
        json!([played])
    );
    assert_eq!(
        listed(&server, "owner", org, &t.production, &t.production_agent, "").await["data"],
        json!([reporter_memory])
    );
}

#[tokio::test]
async fn replays_a_save_with_the_same_key_and_refuses_the_key_for_another() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let (org, ws, agent) = (t.org.as_str(), t.sandbox.as_str(), t.agent.as_str());
    let attempt = |text: &str| {
        server.send(
            call("owner", Method::POST, memories_path(ws, agent))
                .naming(org)
                .idempotent("memory-once")
                .body(json!({ "text": text })),
        )
    };
    let (status, first) = attempt("Once.").await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let (status, again) = attempt("Once.").await;
    assert_eq!(
        (status, &again),
        (StatusCode::CREATED, &first),
        "the first answer, again"
    );
    let (status, other) = attempt("Twice.").await;
    assert_eq!(
        refusal(status, &other),
        refused(StatusCode::UNPROCESSABLE_ENTITY, "idempotency_key_reused", &[])
    );
    assert_eq!(
        listed(&server, "owner", org, ws, agent, "").await["data"],
        json!([first["data"]])
    );
}

/// Delivers what's queued, until nothing is left.
async fn deliver(server: &Server) {
    while webhook_delivery::work_once(&server.state).await {}
}

/// Adds an endpoint of `environment` for `events`: its ID.
async fn hook(server: &Server, org: &str, environment: &str, url: &str, events: Value) -> String {
    let (status, body) = server
        .send(
            call("owner", Method::POST, format!("/v1/organizations/{org}/webhooks")).body(json!({
                "environment": environment, "url": url, "description": "Memories", "events": events,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    id(&body["data"]["endpoint"]).to_owned()
}

async fn deliveries(server: &Server, org: &str, endpoint: &str) -> Vec<Value> {
    let (status, body) = server
        .send(call(
            "owner",
            Method::GET,
            format!("/v1/organizations/{org}/webhooks/{endpoint}/deliveries?limit=100"),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"].as_array().unwrap().clone()
}

#[tokio::test]
async fn sends_each_change_to_its_environments_endpoints_naming_the_memory_never_its_text() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let (org, ws, agent) = (t.org.as_str(), t.sandbox.as_str(), t.agent.as_str());
    let second_type = notes_type(&server, org, "Second", json!({})).await;
    let events = json!(["memory.saved", "memory.updated", "memory.forgotten"]);
    let sandbox_hook = hook(&server, org, "sandbox", HOOK, events.clone()).await;
    let production_hook = hook(&server, org, "production", "https://hooks.example/production", events).await;
    let unsubscribed = hook(
        &server,
        org,
        "sandbox",
        "https://hooks.example/runs",
        json!(["run.completed"]),
    )
    .await;

    let saved_one = saved(&server, "owner", org, ws, agent, "Prefers email.").await;
    let (status, _) = update(
        &server,
        "owner",
        org,
        ws,
        agent,
        id(&saved_one),
        json!({ "text": "Prefers email." }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, changed) = update(
        &server,
        "owner",
        org,
        ws,
        agent,
        id(&saved_one),
        json!({ "text": "Prefers email, never calls." }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    let changed = changed["data"].clone();
    let other = save(
        &server,
        "owner",
        org,
        ws,
        agent,
        json!({ "text": "Ships on Thursdays.", "memory_type_id": second_type }),
    )
    .await
    .1["data"]
        .clone();
    assert_eq!(
        forget(&server, "owner", org, ws, agent, id(&saved_one)).await.0,
        StatusCode::NO_CONTENT
    );
    let last = saved(&server, "owner", org, ws, agent, "Invoices go out monthly.").await;
    assert_eq!(
        forget_all(&server, "owner", org, ws, agent).await.0,
        StatusCode::NO_CONTENT
    );
    // Nothing left: nothing to send.
    assert_eq!(
        forget_all(&server, "owner", org, ws, agent).await.0,
        StatusCode::NO_CONTENT
    );

    // Recorded before each answer, one event each.
    let recorded = deliveries(&server, org, &sandbox_hook).await;
    assert_eq!(recorded.len(), 7);
    let mut event_ids: Vec<&str> = recorded.iter().map(|row| row["event_id"].as_str().unwrap()).collect();
    event_ids.sort_unstable();
    event_ids.dedup();
    assert_eq!(event_ids.len(), 7, "seven events");
    assert!(
        deliveries(&server, org, &production_hook).await.is_empty(),
        "never the other environment's"
    );
    assert!(
        deliveries(&server, org, &unsubscribed).await.is_empty(),
        "nor one that doesn't subscribe"
    );

    deliver(&server).await;
    let sent: Vec<Value> = server
        .providers
        .requests_to(HOOK)
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert_eq!(sent.len(), 7, "each delivered");
    let named = |memory: &Value| {
        json!({
            "object": "memory",
            "id": memory["id"],
            "agent_id": memory["agent_id"],
            "memory_type_id": memory["memory_type_id"],
            "origin": memory["origin"],
        })
    };
    let data_of = |kind: &str| {
        let mut data: Vec<Value> = sent
            .iter()
            .filter(|event| event["type"] == kind)
            .map(|event| event["data"].clone())
            .collect();
        data.sort_by_key(|data| data["id"].as_str().unwrap().to_owned());
        data
    };
    let sorted = |memories: &[&Value]| {
        let mut data: Vec<Value> = memories.iter().map(|memory| named(memory)).collect();
        data.sort_by_key(|data| data["id"].as_str().unwrap().to_owned());
        data
    };
    assert_eq!(data_of("memory.saved"), sorted(&[&saved_one, &other, &last]));
    assert_eq!(data_of("memory.updated"), sorted(&[&changed]));
    assert_eq!(
        data_of("memory.forgotten"),
        sorted(&[&changed, &other, &last]),
        "each memory forgotten, as it was: one by one, then everything left"
    );
    let workspace = server
        .send(call("owner", Method::GET, format!("/v1/workspaces/{ws}")).naming(org))
        .await
        .1;
    for event in &sent {
        assert_eq!(
            (&event["organization_id"], &event["environment"], &event["workspace"]),
            (
                &json!(org),
                &json!("sandbox"),
                &json!({ "id": ws, "external_id": workspace["data"]["external_id"] })
            )
        );
        let body = event.to_string();
        for memory in [&saved_one, &changed, &other, &last] {
            assert!(
                !body.contains(memory["text"].as_str().unwrap()),
                "never the text: {body}"
            );
        }
    }

    // What goes with its agent or its type goes without an event each.
    let going = self::agent(&server, org, ws, "Going", &second_type).await;
    saved(&server, "owner", org, ws, &going, "Goes with its agent.").await;
    let (status, _) = server
        .send(call("owner", Method::DELETE, format!("/v1/workspaces/{ws}/agents/{going}")).naming(org))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    save(
        &server,
        "owner",
        org,
        ws,
        agent,
        json!({ "text": "Goes with its type.", "memory_type_id": second_type }),
    )
    .await;
    let (status, body) = server
        .send(call(
            "owner",
            Method::DELETE,
            format!("/v1/organizations/{org}/memory-types/{second_type}"),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let mut kinds: Vec<String> = deliveries(&server, org, &sandbox_hook)
        .await
        .iter()
        .map(|row| row["event_type"].as_str().unwrap().to_owned())
        .collect();
    kinds.sort_unstable();
    assert_eq!(
        kinds,
        [
            "memory.forgotten",
            "memory.forgotten",
            "memory.forgotten",
            "memory.saved",
            "memory.saved",
            "memory.saved",
            "memory.saved",
            "memory.saved",
            "memory.updated"
        ],
        "only the two saved since"
    );
}

#[tokio::test]
async fn audits_each_change_naming_the_memory_by_its_agent_and_short_id() {
    let server = Server::new().await;
    let t = tenant(&server).await;
    let (org, ws, agent) = (t.org.as_str(), t.sandbox.as_str(), t.agent.as_str());
    let second_type = notes_type(&server, org, "Second", json!({})).await;
    let owner_id = user_id(&server, "owner").await;
    let first = saved(&server, "owner", org, ws, agent, "Audited one.").await;
    update(
        &server,
        "owner",
        org,
        ws,
        agent,
        id(&first),
        json!({ "text": "Audited one." }),
    )
    .await;
    update(
        &server,
        "owner",
        org,
        ws,
        agent,
        id(&first),
        json!({ "text": "Audited one, changed." }),
    )
    .await;
    forget(&server, "owner", org, ws, agent, id(&first)).await;
    let two = saved(&server, "owner", org, ws, agent, "Audited two.").await;
    let three = save(
        &server,
        "owner",
        org,
        ws,
        agent,
        json!({ "text": "Audited three.", "memory_type_id": second_type }),
    )
    .await
    .1["data"]
        .clone();
    forget_all(&server, "owner", org, ws, agent).await;
    forget_all(&server, "owner", org, ws, agent).await;
    // A read records nothing.
    get(&server, "owner", org, ws, agent, id(&two)).await;
    listed(&server, "owner", org, ws, agent, "").await;

    let (status, body) = server
        .send(call(
            "owner",
            Method::GET,
            format!("/v1/organizations/{org}/audit-events?limit=7"),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let events = body["data"].as_array().unwrap();
    let mut recorded: Vec<Value> = events
        .iter()
        .take(6)
        .map(|event| {
            json!([
                event["action"],
                event["target"],
                event["workspace_id"],
                event["environment"],
                event["actor"],
            ])
        })
        .collect();
    let actor = json!({ "type": "user", "id": owner_id, "name": "owner" });
    let named = |memory: &Value| {
        json!({
            "type": "memory",
            "id": memory["id"],
            "name": format!("Keeper · {}", memory["short_id"].as_str().unwrap()),
        })
    };
    let entry = |action: &str, target: Value| json!([action, target, ws, "sandbox", actor]);
    let mut expected = vec![
        entry("memory.saved", named(&first)),
        entry("memory.updated", named(&first)),
        entry("memory.forgotten", named(&first)),
        entry("memory.saved", named(&two)),
        entry("memory.saved", named(&three)),
        entry(
            "agent.memory_forgotten",
            json!({ "type": "agent", "id": agent, "name": "Keeper · 2 memories" }),
        ),
    ];
    let order = |values: &mut Vec<Value>| values.sort_by_key(Value::to_string);
    order(&mut recorded);
    order(&mut expected);
    assert_eq!(recorded, expected, "these six, and nothing for what changed nothing");
    assert_eq!(events[6]["action"], "memory_type.created", "and before them, the type");
    assert!(!body.to_string().contains("Audited"), "never the text");

    // One memory forgotten at once is counted as one.
    saved(&server, "owner", org, ws, agent, "Alone.").await;
    forget_all(&server, "owner", org, ws, agent).await;
    let (_, body) = server
        .send(call(
            "owner",
            Method::GET,
            format!("/v1/organizations/{org}/audit-events?limit=1"),
        ))
        .await;
    assert_eq!(body["data"][0]["target"]["name"], "Keeper · 1 memory");
}

/// The `notes` kind over the server's database, letting people do only what
/// [`Limited::manage`] says, and keeping what it's asked to forget all of.
struct Limited {
    notes: Notes,
    manage: Mutex<Manage>,
    forgotten: Mutex<Vec<Forget>>,
}

impl Limited {
    fn over(server: &Server) -> Arc<Self> {
        Arc::new(Self {
            notes: Notes::new(Arc::new(SqliteNoteStore::new(server.database.clone()))),
            manage: Mutex::new(Manage {
                list: true,
                add: true,
                change: true,
                forget: true,
            }),
            forgotten: Mutex::new(Vec::new()),
        })
    }

    fn allow(&self, change: impl FnOnce(&mut Manage)) {
        change(&mut self.manage.lock().unwrap());
    }
}

#[async_trait]
impl MemoryProvider for Limited {
    fn kind(&self) -> &MemoryKind {
        self.notes.kind()
    }

    fn describe(&self) -> KindDescription {
        KindDescription {
            manage: *self.manage.lock().unwrap(),
            ..self.notes.describe()
        }
    }

    fn check_settings(&self, settings: &Map<String, Value>) -> Result<Settings, SettingsRefusal> {
        self.notes.check_settings(settings)
    }

    fn tools(&self, settings: &Settings, writes: MemoryWrites) -> Vec<MemoryTool> {
        self.notes.tools(settings, writes)
    }

    fn text_rule(&self) -> String {
        self.notes.text_rule()
    }

    fn limit(&self, settings: &Settings) -> u32 {
        self.notes.limit(settings)
    }

    async fn recall(
        &self,
        scope: &MemoryScope,
        audience: &Audience,
        settings: &Settings,
    ) -> Result<Recall, MemoryError> {
        self.notes.recall(scope, audience, settings).await
    }

    async fn call(
        &self,
        scope: &MemoryScope,
        context: &CallContext<'_>,
        tool: &str,
        arguments: &Map<String, Value>,
    ) -> Result<CallResult, MemoryError> {
        self.notes.call(scope, context, tool, arguments).await
    }

    async fn list(&self, scope: &MemoryScope, filter: &MemoryFilter) -> Result<Page<Memory>, MemoryError> {
        self.notes.list(scope, filter).await
    }

    async fn get(&self, scope: &MemoryScope, id: MemoryId) -> Result<Memory, MemoryError> {
        self.notes.get(scope, id).await
    }

    async fn add(
        &self,
        scope: &MemoryScope,
        origin: &Origin,
        settings: &Settings,
        text: &str,
        now: Timestamp,
    ) -> Result<Memory, MemoryError> {
        self.notes.add(scope, origin, settings, text, now).await
    }

    async fn change(
        &self,
        scope: &MemoryScope,
        origin: &Origin,
        id: MemoryId,
        text: &str,
        now: Timestamp,
    ) -> Result<Rewritten, MemoryError> {
        self.notes.change(scope, origin, id, text, now).await
    }

    async fn forget(&self, scope: &MemoryScope, id: MemoryId) -> Result<Memory, MemoryError> {
        self.notes.forget(scope, id).await
    }

    async fn forget_all(&self, what: &Forget) -> Result<(), MemoryError> {
        self.forgotten.lock().unwrap().push(*what);
        self.notes.forget_all(what).await
    }
}

#[tokio::test]
async fn keeps_a_kind_it_does_not_serve_out_of_reach() {
    let mut server = Server::new().await;
    let t = tenant(&server).await;
    let (org, ws, agent) = (t.org.as_str(), t.sandbox.as_str(), t.agent.as_str());
    let kept = saved(&server, "owner", org, ws, agent, "Kept out of reach.").await;
    serve(&mut server, MemoryProviders::none());

    // A list or a save names the type, whose kind it doesn't serve.
    let not_served = refused(StatusCode::UNPROCESSABLE_ENTITY, "memory_kind_not_served", &[]);
    for query in [String::new(), format!("?memory_type={}&q=reach", t.memory_type)] {
        let (status, body) = server
            .send(call("owner", Method::GET, format!("{}{query}", memories_path(ws, agent))).naming(org))
            .await;
        assert_eq!(refusal(status, &body), not_served, "{query}");
    }
    let (status, body) = save(&server, "owner", org, ws, agent, json!({ "text": "Not kept." })).await;
    assert_eq!(refusal(status, &body), not_served);
    assert_eq!(
        body["error"]["message"],
        "This server doesn't serve notes, the memory kind of Notes."
    );
    // By its ID, it's no memory of any kind it serves.
    let (status, body) = get(&server, "owner", org, ws, agent, id(&kept)).await;
    assert_eq!(refusal(status, &body), not_found());
    for text in ["Not kept.", ""] {
        let (status, body) = update(&server, "owner", org, ws, agent, id(&kept), json!({ "text": text })).await;
        assert_eq!(refusal(status, &body), not_found(), "{text:?}");
    }
    let (status, body) = forget(&server, "owner", org, ws, agent, id(&kept)).await;
    assert_eq!(refusal(status, &body), not_found());
    // Forgetting everything reaches the kinds it serves only.
    assert_eq!(
        forget_all(&server, "owner", org, ws, agent).await.0,
        StatusCode::NO_CONTENT
    );

    let notes = zupflow_server::bootstrap::memory_providers(Arc::new(SqliteNoteStore::new(server.database.clone())));
    serve(&mut server, notes);
    assert_eq!(
        listed(&server, "owner", org, ws, agent, "").await["data"],
        json!([kept])
    );
}

#[tokio::test]
async fn refuses_what_a_kind_does_not_let_people_do() {
    let mut server = Server::new().await;
    let t = tenant(&server).await;
    let (org, ws, agent) = (t.org.as_str(), t.sandbox.as_str(), t.agent.as_str());
    let limited = Limited::over(&server);
    serve(&mut server, MemoryProviders::none().with(limited.clone()));
    let kept = saved(&server, "owner", org, ws, agent, "Kept.").await;
    let unsupported = refused(StatusCode::UNPROCESSABLE_ENTITY, "memory_kind_unsupported", &[]);
    let missing = MISSING_MEMORIES[0];

    limited.allow(|manage| manage.list = false);
    let (status, body) = server
        .send(call("owner", Method::GET, memories_path(ws, agent)).naming(org))
        .await;
    assert_eq!(refusal(status, &body), unsupported);
    assert_eq!(
        body["error"]["message"],
        "Notes's memory kind, notes, can't list memories from the API or the Console."
    );
    let (status, body) = get(&server, "owner", org, ws, agent, id(&kept)).await;
    assert_eq!(refusal(status, &body), unsupported);
    let (status, body) = get(&server, "owner", org, ws, agent, missing).await;
    assert_eq!(refusal(status, &body), not_found());
    limited.allow(|manage| manage.list = true);

    limited.allow(|manage| manage.add = false);
    let (status, body) = save(&server, "owner", org, ws, agent, json!({ "text": "Not kept." })).await;
    assert_eq!(refusal(status, &body), unsupported);
    limited.allow(|manage| manage.add = true);

    // The memory first, then what the kind allows, and only then the text.
    limited.allow(|manage| manage.change = false);
    for body in [json!({ "text": "" }), json!({ "text": 7 })] {
        let (status, answer) = update(&server, "owner", org, ws, agent, id(&kept), body.clone()).await;
        assert_eq!(refusal(status, &answer), unsupported, "{body}");
        let (status, answer) = update(&server, "owner", org, ws, agent, missing, body.clone()).await;
        assert_eq!(refusal(status, &answer), not_found(), "{body}");
    }
    limited.allow(|manage| manage.change = true);

    limited.allow(|manage| manage.forget = false);
    let (status, body) = forget(&server, "owner", org, ws, agent, id(&kept)).await;
    assert_eq!(refusal(status, &body), unsupported);
    let (status, body) = forget(&server, "owner", org, ws, agent, missing).await;
    assert_eq!(refusal(status, &body), not_found());
    // Forgetting everything leaves what people can't forget.
    assert_eq!(
        forget_all(&server, "owner", org, ws, agent).await.0,
        StatusCode::NO_CONTENT
    );
    limited.allow(|manage| manage.forget = true);
    limited.allow(|manage| manage.list = false);
    assert_eq!(
        forget_all(&server, "owner", org, ws, agent).await.0,
        StatusCode::NO_CONTENT
    );
    limited.allow(|manage| manage.list = true);

    assert_eq!(
        listed(&server, "owner", org, ws, agent, "").await["data"],
        json!([kept])
    );
    assert!(limited.forgotten.lock().unwrap().is_empty());
}

#[tokio::test]
async fn forgets_a_deleted_types_memories_through_its_kind() {
    let mut server = Server::new().await;
    let t = tenant(&server).await;
    let (org, ws, agent) = (t.org.as_str(), t.sandbox.as_str(), t.agent.as_str());
    let limited = Limited::over(&server);
    serve(&mut server, MemoryProviders::none().with(limited.clone()));
    let going = notes_type(&server, org, "Going", json!({})).await;
    let kept = saved(&server, "owner", org, ws, agent, "Stays with its type.").await;
    let gone = save(
        &server,
        "owner",
        org,
        ws,
        agent,
        json!({ "text": "Goes with its type.", "memory_type_id": going }),
    )
    .await
    .1["data"]
        .clone();
    let (status, body) = server
        .send(call(
            "owner",
            Method::DELETE,
            format!("/v1/organizations/{org}/memory-types/{going}"),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert_eq!(
        *limited.forgotten.lock().unwrap(),
        [Forget::Type {
            organization_id: OrganizationId::parse(org).unwrap(),
            memory_type_id: MemoryTypeId::parse(&going).unwrap(),
        }],
        "asked once, for that type in every workspace"
    );
    let (status, body) = get(&server, "owner", org, ws, agent, id(&gone)).await;
    assert_eq!(refusal(status, &body), not_found());
    assert_eq!(
        listed(&server, "owner", org, ws, agent, "").await["data"],
        json!([kept])
    );
    // A type that isn't deleted asks nothing.
    let (status, _) = server
        .send(call(
            "owner",
            Method::DELETE,
            format!("/v1/organizations/{org}/memory-types/{}", t.memory_type),
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "in use");
    assert_eq!(limited.forgotten.lock().unwrap().len(), 1);
}
