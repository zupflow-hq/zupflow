//! Memory types through the API: the kinds the server serves, an
//! organization's types and its one default type. Every member and the
//! organization's API keys read them; owners and admins change them, from
//! the Console only. Each organization's are its own, and a type its default
//! or an agent holds is never deleted.

#![allow(clippy::unwrap_used)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Call, Server, call, code};
use serde_json::{Value, json};
use zupflow_core::memory::registry::MemoryProviders;

/// IDs that name no type: one of a type's form that doesn't exist, and a
/// malformed one.
const MISSING_TYPES: [&str; 2] = ["mtype_0000000000000000000000", "not-a-memory-type"];

fn id(value: &Value) -> &str {
    value["id"].as_str().unwrap()
}

fn kinds_path(org: &str) -> String {
    format!("/v1/organizations/{org}/memory-kinds")
}

fn types_path(org: &str) -> String {
    format!("/v1/organizations/{org}/memory-types")
}

fn type_path(org: &str, memory_type: &str) -> String {
    format!("/v1/organizations/{org}/memory-types/{memory_type}")
}

fn default_path(org: &str) -> String {
    format!("/v1/organizations/{org}/default-memory-type")
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

async fn make(server: &Server, who: &str, org: &str, body: Value) -> Value {
    let (status, made) = server.send(call(who, Method::POST, types_path(org)).body(body)).await;
    assert_eq!(status, StatusCode::CREATED, "{made}");
    made["data"].clone()
}

/// A `notes` type of `org`, with the kind's defaults, made by its owner.
async fn notes(server: &Server, org: &str, name: &str) -> Value {
    make(server, "alice", org, json!({ "name": name, "kind": "notes" })).await
}

async fn read(server: &Server, who: &str, path: &str) -> Value {
    let (status, body) = server.send(call(who, Method::GET, path)).await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
    body["data"].clone()
}

async fn api_key(server: &Server, who: &str, org: &str, environment: &str) -> String {
    let (status, body) = server
        .send(
            call(who, Method::POST, format!("/v1/organizations/{org}/api-keys"))
                .body(json!({ "name": "Backend", "environment": environment })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["data"]["secret"].as_str().unwrap().to_owned()
}

/// A Sandbox or Production workspace named after its slug, in capitals.
async fn workspace(server: &Server, org: &str, slug: &str, environment: &str) -> String {
    id(&server.workspace("alice", org, slug, environment).await).to_owned()
}

/// Creates an agent in `ws` with `memory` (its `memory` and
/// `memory_type_id`, as the body sends them): the status and the answer.
async fn create_agent(
    server: &Server,
    who: &str,
    org: &str,
    ws: &str,
    name: &str,
    memory: Value,
) -> (StatusCode, Value) {
    let mut body = json!({ "name": name, "description": "", "instructions": "" });
    body.as_object_mut()
        .unwrap()
        .extend(memory.as_object().unwrap().clone());
    server
        .send(
            call(who, Method::POST, format!("/v1/workspaces/{ws}/agents"))
                .naming(org)
                .body(body),
        )
        .await
}

async fn agent(server: &Server, org: &str, ws: &str, name: &str, memory: Value) -> Value {
    let (status, body) = create_agent(server, "alice", org, ws, name, memory).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body["data"].clone()
}

async fn agent_count(server: &Server, org: &str, memory_type: &str) -> u64 {
    read(server, "alice", &type_path(org, memory_type)).await["agent_count"]
        .as_u64()
        .unwrap()
}

#[tokio::test]
async fn describes_the_kinds_it_serves_and_makes_types_of_them() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    server.member("alice", &org, "viewer", "viewer").await;
    let kinds = read(&server, "alice", &kinds_path(&org)).await;
    let listed: Vec<&str> = kinds
        .as_array()
        .unwrap()
        .iter()
        .map(|kind| kind["kind"].as_str().unwrap())
        .collect();
    assert_eq!(listed, ["notes"]);
    assert_eq!(kinds[0]["name"], "Notes");
    assert_eq!(
        kinds[0]["manage"],
        json!({ "list": true, "add": true, "change": true, "forget": true })
    );
    assert_eq!(
        kinds[0]["settings_schema"]["properties"]["recall_characters"]["default"],
        8000
    );
    // Every member and API key reads them, a key of either environment the same.
    assert_eq!(read(&server, "viewer", &kinds_path(&org)).await, kinds);
    for environment in ["sandbox", "production"] {
        let key = api_key(&server, "alice", &org, environment).await;
        assert_eq!(read(&server, &key, &kinds_path(&org)).await, kinds, "{environment}");
    }

    let made = notes(&server, &org, "  Team notes  ").await;
    assert!(id(&made).starts_with("mtype_"));
    let mut fields: Vec<&str> = made.as_object().unwrap().keys().map(String::as_str).collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        [
            "agent_count",
            "created_at",
            "id",
            "is_default",
            "kind",
            "name",
            "served",
            "settings",
            "updated_at",
            "writes"
        ],
        "never its organization's ID"
    );
    assert_eq!(
        (
            &made["name"],
            &made["kind"],
            &made["settings"],
            &made["writes"],
            &made["served"],
            &made["agent_count"],
            &made["is_default"],
        ),
        (
            &json!("Team notes"),
            &json!("notes"),
            &json!({ "recall_characters": 8000, "max_notes": 200 }),
            &json!("allow"),
            &json!(true),
            &json!(0),
            &json!(false),
        ),
        "trimmed, with the kind's defaults"
    );
    assert_eq!(read(&server, "alice", &type_path(&org, id(&made))).await, made);
    for (settings, writes) in [
        (json!({ "recall_characters": 0, "max_notes": 1 }), "deny"),
        (json!({ "recall_characters": 32000, "max_notes": 1000 }), "approve"),
    ] {
        let name = format!("Bounds {writes}");
        let made = make(
            &server,
            "alice",
            &org,
            json!({ "name": name, "kind": "notes", "settings": settings, "writes": writes }),
        )
        .await;
        assert_eq!((&made["settings"], &made["writes"]), (&settings, &json!(writes)));
    }
    // A whole number however JSON writes it, kept as an integer.
    let written = make(
        &server,
        "alice",
        &org,
        json!({ "name": "Written", "kind": "notes", "settings": { "recall_characters": 8000.0, "max_notes": 1e3 } }),
    )
    .await;
    assert_eq!(
        written["settings"],
        json!({ "recall_characters": 8000, "max_notes": 1000 })
    );
    assert!(written["settings"]["recall_characters"].is_u64());
    let longest = notes(&server, &org, &format!(" {} ", "x".repeat(60))).await;
    assert_eq!(longest["name"].as_str().unwrap().chars().count(), 60);

    // A kind it doesn't serve and settings the kind refuses, each named.
    let attempt = |body: Value| call("alice", Method::POST, types_path(&org)).body(body);
    for kind in ["no_such_kind", "Not a kind!"] {
        let (status, body) = server.send(attempt(json!({ "name": "X", "kind": kind }))).await;
        assert_eq!(
            refusal(status, &body),
            refused(StatusCode::UNPROCESSABLE_ENTITY, "unknown_memory_kind", &["kind"]),
            "{kind}"
        );
    }
    let (status, body) = server
        .send(attempt(json!({
            "name": "X", "kind": "notes", "settings": { "recall_characters": 32001, "max_notes": 0, "colour": "blue" },
        })))
        .await;
    assert_eq!(
        refusal(status, &body),
        refused(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_memory_settings",
            &["settings.colour", "settings.max_notes", "settings.recall_characters"]
        )
    );
    for value in [
        json!(-1),
        json!(1.5),
        json!("100"),
        Value::Null,
        json!(true),
        json!([100]),
        json!({ "characters": 100 }),
    ] {
        let (status, body) = server
            .send(attempt(
                json!({ "name": "X", "kind": "notes", "settings": { "recall_characters": value } }),
            ))
            .await;
        assert_eq!(
            refusal(status, &body),
            refused(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_memory_settings",
                &["settings.recall_characters"]
            ),
            "{value}"
        );
    }
    // What the shapes refuse is `400`, every field named at once.
    for (body, fields) in [
        (json!({}), vec!["kind", "name"]),
        (json!({ "name": "", "kind": "notes" }), vec!["name"]),
        (json!({ "name": "   ", "kind": "notes" }), vec!["name"]),
        (json!({ "name": "x".repeat(61), "kind": "notes" }), vec!["name"]),
        (json!({ "name": "X", "kind": 1 }), vec!["kind"]),
        (
            json!({ "name": "X", "kind": "notes", "writes": "sometimes" }),
            vec!["writes"],
        ),
        (
            json!({ "name": "X", "kind": "notes", "settings": [] }),
            vec!["settings"],
        ),
        (
            json!({ "name": 1, "kind": "notes", "settings": null, "writes": null }),
            vec!["name", "settings", "writes"],
        ),
    ] {
        let (status, answer) = server.send(attempt(body.clone())).await;
        assert_eq!(
            refusal(status, &answer),
            refused(StatusCode::BAD_REQUEST, "validation_failed", &fields),
            "{body}"
        );
    }
    // A name is the organization's once, exactly as written once trimmed.
    for name in ["Team notes", "  Team notes "] {
        let (status, body) = server.send(attempt(json!({ "name": name, "kind": "notes" }))).await;
        assert_eq!(
            refusal(status, &body),
            refused(StatusCode::CONFLICT, "memory_type_name_taken", &["name"]),
            "{name}"
        );
    }
    notes(&server, &org, "team notes").await;
    let listed = server
        .send(call("alice", Method::GET, format!("{}?limit=100", types_path(&org))))
        .await
        .1;
    assert_eq!(listed["meta"]["pagination"]["total"], 6, "no refusal made one");
}

#[tokio::test]
async fn lists_types_by_name_in_any_case_a_page_at_a_time() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    for name in ["B", "a", "c"] {
        notes(&server, &org, name).await;
    }
    let list = |query: String| server.send(call("alice", Method::GET, format!("{}{query}", types_path(&org))));
    let (status, all) = list("?limit=100".to_owned()).await;
    assert_eq!(status, StatusCode::OK, "{all}");
    let names: Vec<&str> = all["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|memory_type| memory_type["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a", "B", "c"]);
    assert_eq!(all["meta"]["pagination"], json!({ "next_cursor": null, "total": 3 }));
    let mut paged = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let query = match &cursor {
            None => "?limit=1".to_owned(),
            Some(cursor) => format!("?limit=1&cursor={cursor}"),
        };
        let (status, page) = list(query).await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["data"].as_array().unwrap().len(), 1);
        assert_eq!(page["meta"]["pagination"]["total"], 3);
        paged.push(id(&page["data"][0]).to_owned());
        match page["meta"]["pagination"]["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }
    let ids: Vec<&str> = all["data"].as_array().unwrap().iter().map(id).collect();
    assert_eq!(paged, ids);
    // Out-of-range limits are clamped, and a cursor the list didn't issue is refused.
    assert_eq!(list("?limit=0".to_owned()).await.1["data"].as_array().unwrap().len(), 1);
    assert_eq!(
        list("?limit=1000".to_owned()).await.1["data"].as_array().unwrap().len(),
        3
    );
    let (status, body) = list("?cursor=%25%25%25".to_owned()).await;
    assert_eq!(
        refusal(status, &body),
        refused(StatusCode::BAD_REQUEST, "validation_failed", &["cursor"])
    );
}

#[tokio::test]
async fn changes_a_types_name_settings_and_writes_but_never_its_kind() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let made = make(
        &server,
        "alice",
        &org,
        json!({ "name": "Before", "kind": "notes", "settings": { "recall_characters": 4000 } }),
    )
    .await;
    assert_eq!(made["settings"], json!({ "recall_characters": 4000, "max_notes": 200 }));
    let path = type_path(&org, id(&made));
    let change = |body: Value| call("alice", Method::PATCH, path.clone()).body(body);
    let (status, changed) = server
        .send(change(
            json!({ "name": "After", "settings": { "max_notes": 50 }, "writes": "approve" }),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    let changed = changed["data"].clone();
    assert_eq!(
        (
            id(&changed),
            &changed["name"],
            &changed["kind"],
            &changed["writes"],
            &changed["created_at"]
        ),
        (
            id(&made),
            &json!("After"),
            &json!("notes"),
            &json!("approve"),
            &made["created_at"]
        )
    );
    assert_eq!(
        changed["settings"],
        json!({ "recall_characters": 8000, "max_notes": 50 }),
        "replaced whole: one left out takes the kind's default again"
    );
    assert!(changed["updated_at"].as_str().unwrap() >= made["updated_at"].as_str().unwrap());
    assert_eq!(read(&server, "alice", &path).await, changed);
    // What isn't sent is kept, and the kind isn't something it can send.
    let (status, kept) = server.send(change(json!({ "kind": "other" }))).await;
    assert_eq!(status, StatusCode::OK, "{kept}");
    assert_eq!(
        (
            &kept["data"]["name"],
            &kept["data"]["kind"],
            &kept["data"]["settings"],
            &kept["data"]["writes"]
        ),
        (
            &changed["name"],
            &json!("notes"),
            &changed["settings"],
            &changed["writes"]
        )
    );
    // Refused as making one is, changing nothing.
    for (body, expected) in [
        (
            json!({ "settings": { "max_notes": 1001, "colour": "blue" } }),
            refused(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_memory_settings",
                &["settings.colour", "settings.max_notes"],
            ),
        ),
        (
            json!({ "name": " " }),
            refused(StatusCode::BAD_REQUEST, "validation_failed", &["name"]),
        ),
        (
            json!({ "name": null }),
            refused(StatusCode::BAD_REQUEST, "validation_failed", &["name"]),
        ),
        (
            json!({ "writes": "sometimes" }),
            refused(StatusCode::BAD_REQUEST, "validation_failed", &["writes"]),
        ),
        (
            json!({ "settings": "all" }),
            refused(StatusCode::BAD_REQUEST, "validation_failed", &["settings"]),
        ),
    ] {
        let (status, body) = server.send(change(body.clone())).await;
        assert_eq!(refusal(status, &body), expected, "{body}");
    }
    let after = read(&server, "alice", &path).await;
    assert_eq!(
        (&after["name"], &after["settings"], &after["writes"]),
        (&changed["name"], &changed["settings"], &changed["writes"])
    );
    // A type keeps its own name, padded or not, and takes no other type's.
    let (status, renamed) = server.send(change(json!({ "name": "  After " }))).await;
    assert_eq!((status, &renamed["data"]["name"]), (StatusCode::OK, &json!("After")));
    let other = notes(&server, &org, "Other").await;
    let (status, body) = server
        .send(call("alice", Method::PATCH, type_path(&org, id(&other))).body(json!({ "name": "After" })))
        .await;
    assert_eq!(
        refusal(status, &body),
        refused(StatusCode::CONFLICT, "memory_type_name_taken", &["name"])
    );
    // A type that isn't there isn't found, whether its ID is unknown or malformed.
    for missing in MISSING_TYPES {
        for request in [
            call("alice", Method::GET, type_path(&org, missing)),
            call("alice", Method::PATCH, type_path(&org, missing)).body(json!({ "name": "X" })),
            call("alice", Method::DELETE, type_path(&org, missing)),
        ] {
            let (status, body) = server.send(request).await;
            assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"), "{missing}");
        }
    }
}

/// The changes, each sent with a body that would be refused, so a refusal
/// of the caller comes before anything about what it sent.
fn changes<'a>(who: &'a str, org: &str, memory_type: &str) -> Vec<Call<'a>> {
    vec![
        call(who, Method::POST, types_path(org)).body(json!({ "nonsense": true })),
        call(who, Method::PATCH, type_path(org, memory_type)).body(json!({ "name": 5 })),
        call(who, Method::DELETE, type_path(org, memory_type)),
        call(who, Method::PUT, default_path(org)).body(json!({})),
    ]
}

/// Everything a caller reads of the organization's memory.
async fn reads(server: &Server, who: &str, org: &str, memory_type: &str) -> Value {
    json!([
        read(server, who, &kinds_path(org)).await,
        read(server, who, &format!("{}?limit=100", types_path(org))).await,
        read(server, who, &type_path(org, memory_type)).await,
        read(server, who, &default_path(org)).await,
    ])
}

#[tokio::test]
async fn lets_only_owners_and_admins_change_them_and_only_from_the_console() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    for (who, role) in [("developer", "developer"), ("viewer", "viewer"), ("admin", "admin")] {
        server.member("alice", &org, who, role).await;
    }
    let made = notes(&server, &org, "Roles").await;
    let before = reads(&server, "alice", &org, id(&made)).await;
    for who in ["developer", "viewer"] {
        for change in changes(who, &org, id(&made)) {
            let what = format!("{who}: {} {}", change.method, change.uri);
            let (status, body) = server.send(change).await;
            assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "forbidden"), "{what}");
        }
        assert_eq!(reads(&server, who, &org, id(&made)).await, before, "{who} reads them");
    }
    // API keys of either environment read them and change none, idempotently or not.
    for environment in ["sandbox", "production"] {
        let key = api_key(&server, "alice", &org, environment).await;
        let mut attempts = changes(&key, &org, id(&made));
        attempts.push(
            call(&key, Method::POST, types_path(&org))
                .idempotent("by-key")
                .body(json!({ "name": "Keyed", "kind": "notes" })),
        );
        for change in attempts {
            let what = format!("{environment} key: {} {}", change.method, change.uri);
            let (status, body) = server.send(change).await;
            assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "console_only"), "{what}");
        }
        assert_eq!(reads(&server, &key, &org, id(&made)).await, before, "{environment}");
    }
    assert_eq!(
        reads(&server, "alice", &org, id(&made)).await,
        before,
        "nothing changed"
    );

    // An admin may.
    let theirs = make(&server, "admin", &org, json!({ "name": "Admin's", "kind": "notes" })).await;
    let (status, changed) = server
        .send(call("admin", Method::PATCH, type_path(&org, id(&theirs))).body(json!({ "writes": "deny" })))
        .await;
    assert_eq!((status, &changed["data"]["writes"]), (StatusCode::OK, &json!("deny")));
    let (status, chosen) = server
        .send(call("admin", Method::PUT, default_path(&org)).body(json!({ "memory_type_id": id(&theirs) })))
        .await;
    assert_eq!(status, StatusCode::OK, "{chosen}");
    assert_eq!(
        (
            id(&chosen["data"]["memory_type"]),
            &chosen["data"]["updated_by"]["email"]
        ),
        (id(&theirs), &json!("admin@people.example"))
    );
    let (status, _) = server
        .send(call("admin", Method::PUT, default_path(&org)).body(json!({ "memory_type_id": null })))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = server
        .send(call("admin", Method::DELETE, type_path(&org, id(&theirs))))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn keeps_each_organizations_types_and_default_to_itself() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let theirs = server.organization("mallory", "larkspur").await;
    let made = notes(&server, &org, "Private").await;
    let private = id(&made).to_owned();
    // Another organization's member finds none of it, by any path or ID.
    for (method, path, body) in [
        (Method::GET, kinds_path(&org), None),
        (Method::GET, types_path(&org), None),
        (
            Method::POST,
            types_path(&org),
            Some(json!({ "name": "Owned", "kind": "notes" })),
        ),
        (Method::GET, type_path(&org, &private), None),
        (
            Method::PATCH,
            type_path(&org, &private),
            Some(json!({ "name": "Owned" })),
        ),
        (Method::DELETE, type_path(&org, &private), None),
        (Method::GET, default_path(&org), None),
        (Method::PUT, default_path(&org), Some(json!({ "memory_type_id": null }))),
        (Method::GET, type_path(&theirs, &private), None),
        (
            Method::PATCH,
            type_path(&theirs, &private),
            Some(json!({ "name": "Owned" })),
        ),
        (Method::DELETE, type_path(&theirs, &private), None),
    ] {
        let what = format!("{method} {path}");
        let mut request = call("mallory", method, path);
        if let Some(body) = body {
            request = request.body(body);
        }
        let (status, answer) = server.send(request).await;
        assert_eq!((status, code(&answer)), (StatusCode::NOT_FOUND, "not_found"), "{what}");
    }
    // Nor do its API keys.
    let key = api_key(&server, "mallory", &theirs, "sandbox").await;
    for path in [
        kinds_path(&org),
        types_path(&org),
        type_path(&org, &private),
        default_path(&org),
        type_path(&theirs, &private),
    ] {
        let (status, answer) = server.send(call(&key, Method::GET, &path)).await;
        assert_eq!((status, code(&answer)), (StatusCode::NOT_FOUND, "not_found"), "{path}");
    }
    // It's not the other organization's to choose as its default, or to
    // give its agents.
    let (status, answer) = server
        .send(call("mallory", Method::PUT, default_path(&theirs)).body(json!({ "memory_type_id": private })))
        .await;
    assert_eq!(
        refusal(status, &answer),
        refused(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_memory_type",
            &["memory_type_id"]
        )
    );
    let harbor = id(&server.workspace("mallory", &theirs, "harbor-sandbox", "sandbox").await).to_owned();
    let naming = json!({ "memory": "type", "memory_type_id": private });
    let (status, answer) = create_agent(&server, "mallory", &theirs, &harbor, "Theirs", naming.clone()).await;
    assert_eq!(
        refusal(status, &answer),
        refused(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_memory_type",
            &["memory_type_id"]
        )
    );
    let (status, plain) = create_agent(&server, "mallory", &theirs, &harbor, "Theirs", json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{plain}");
    let (status, answer) = server
        .send(
            call(
                "mallory",
                Method::PATCH,
                format!("/v1/workspaces/{harbor}/agents/{}", id(&plain["data"])),
            )
            .naming(&theirs)
            .body(naming),
        )
        .await;
    assert_eq!(
        refusal(status, &answer),
        refused(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_memory_type",
            &["memory_type_id"]
        )
    );
    // Each organization's default and names are its own.
    let (status, _) = server
        .send(call("alice", Method::PUT, default_path(&org)).body(json!({ "memory_type_id": private })))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        read(&server, "mallory", &default_path(&theirs)).await,
        json!({ "memory_type": null, "updated_at": null, "updated_by": null })
    );
    let listed = server.send(call("mallory", Method::GET, types_path(&theirs))).await.1;
    assert_eq!(
        (&listed["data"], &listed["meta"]["pagination"]["total"]),
        (&json!([]), &json!(0))
    );
    let same_name = make(
        &server,
        "mallory",
        &theirs,
        json!({ "name": "Private", "kind": "notes" }),
    )
    .await;
    assert_ne!(id(&same_name), private);
}

#[tokio::test]
async fn refuses_deleting_a_type_its_default_or_an_agent_holds_naming_them() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let qa = workspace(&server, &org, "qa", "sandbox").await;
    let lab = workspace(&server, &org, "lab", "sandbox").await;
    let acme = workspace(&server, &org, "acme", "production").await;
    let made = notes(&server, &org, "Held").await;
    let held = id(&made).to_owned();
    let naming = json!({ "memory": "type", "memory_type_id": held });
    let delete = || call("alice", Method::DELETE, type_path(&org, &held));

    let holder = agent(&server, &org, &qa, "Holder", naming.clone()).await;
    assert_eq!(agent_count(&server, &org, &held).await, 1);
    let (status, answer) = server.send(delete()).await;
    assert_eq!(
        (status, code(&answer), &answer["error"]["fields"]),
        (
            StatusCode::CONFLICT,
            "memory_type_in_use",
            &json!({ "agents": "Holder (QA)" })
        )
    );
    assert_eq!(
        answer["error"]["message"],
        "Can't delete Held: agents use it: Holder (QA); choose another memory for them first."
    );
    // Its agent lets go; then the default holds it.
    let (status, _) = server
        .send(
            call(
                "alice",
                Method::PATCH,
                format!("/v1/workspaces/{qa}/agents/{}", id(&holder)),
            )
            .naming(&org)
            .body(json!({ "memory": "default" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(agent_count(&server, &org, &held).await, 0);
    let set_default = |memory_type_id: Value| {
        call("alice", Method::PUT, default_path(&org)).body(json!({ "memory_type_id": memory_type_id }))
    };
    assert_eq!(server.send(set_default(json!(held))).await.0, StatusCode::OK);
    let (status, answer) = server.send(delete()).await;
    assert_eq!(
        (status, code(&answer), &answer["error"]["fields"]),
        (
            StatusCode::CONFLICT,
            "memory_type_in_use",
            &json!({ "default_memory_type": "Held" })
        )
    );
    assert_eq!(
        answer["error"]["message"],
        "Can't delete Held: Held is the organization's default memory type; choose another default first."
    );

    // Six agents in every workspace and environment, and the default: five
    // are named, and an agent on the default isn't one of them.
    let mut holders = Vec::new();
    for (i, (ws, place)) in [(&qa, "QA"), (&lab, "LAB"), (&acme, "ACME")]
        .into_iter()
        .cycle()
        .take(6)
        .enumerate()
    {
        let name = format!("Agent {i}");
        let made = agent(&server, &org, ws, &name, naming.clone()).await;
        holders.push((ws.clone(), id(&made).to_owned(), format!("{name} ({place})")));
    }
    agent(&server, &org, &qa, "On the default", json!({})).await;
    assert_eq!(
        agent_count(&server, &org, &held).await,
        6,
        "the default's agents aren't counted"
    );
    let (status, answer) = server.send(delete()).await;
    assert_eq!((status, code(&answer)), (StatusCode::CONFLICT, "memory_type_in_use"));
    let fields = &answer["error"]["fields"];
    assert_eq!(fields["default_memory_type"], "Held");
    let named = fields["agents"].as_str().unwrap();
    assert!(named.ends_with(", 1 more"), "{named}");
    assert_eq!(
        holders
            .iter()
            .filter(|(_, _, holder)| named.contains(holder.as_str()))
            .count(),
        5,
        "{named}"
    );
    assert!(!named.contains("On the default"), "{named}");
    assert!(answer["error"]["message"].as_str().unwrap().contains(named));

    // Agents that go let go of it, with their workspace too.
    assert_eq!(server.send(set_default(Value::Null)).await.0, StatusCode::OK);
    for (ws, agent, _) in holders.iter().filter(|(ws, _, _)| *ws == acme) {
        let (status, _) = server
            .send(call("alice", Method::DELETE, format!("/v1/workspaces/{ws}/agents/{agent}")).naming(&org))
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    assert_eq!(agent_count(&server, &org, &held).await, 4);
    for ws in [&qa, &lab] {
        let (status, _) = server
            .send(call("alice", Method::DELETE, format!("/v1/workspaces/{ws}")).naming(&org))
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    assert_eq!(agent_count(&server, &org, &held).await, 0);
    assert_eq!(server.send(delete()).await.0, StatusCode::NO_CONTENT);
    for request in [call("alice", Method::GET, type_path(&org, &held)), delete()] {
        let (status, answer) = server.send(request).await;
        assert_eq!((status, code(&answer)), (StatusCode::NOT_FOUND, "not_found"));
    }
    // Gone, it's no agent's to name.
    let (status, answer) = create_agent(&server, "alice", &org, &acme, "Late", naming).await;
    assert_eq!(
        refusal(status, &answer),
        refused(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_memory_type",
            &["memory_type_id"]
        )
    );
}

#[tokio::test]
async fn chooses_one_of_its_types_as_the_default_or_none_leaving_the_types_as_they_were() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    assert_eq!(
        read(&server, "alice", &default_path(&org)).await,
        json!({ "memory_type": null, "updated_at": null, "updated_by": null }),
        "memory is off until someone chooses"
    );
    let first = notes(&server, &org, "First").await;
    let second = notes(&server, &org, "Second").await;
    let set_default = |body: Value| call("alice", Method::PUT, default_path(&org)).body(body);
    let (status, chosen) = server.send(set_default(json!({ "memory_type_id": id(&first) }))).await;
    assert_eq!(status, StatusCode::OK, "{chosen}");
    let chosen = chosen["data"].clone();
    let mut expected = first.clone();
    expected["is_default"] = json!(true);
    assert_eq!(chosen["memory_type"], expected, "the type as it was, now the default");
    assert_eq!(chosen["updated_by"]["email"], "alice@people.example");
    assert!(chosen["updated_at"].is_string());
    assert_eq!(read(&server, "alice", &default_path(&org)).await, chosen);
    assert_eq!(read(&server, "alice", &type_path(&org, id(&first))).await, expected);
    // One default: choosing another takes it from the first.
    server.send(set_default(json!({ "memory_type_id": id(&second) }))).await;
    let listed = read(&server, "alice", &types_path(&org)).await;
    let defaults: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .filter(|memory_type| memory_type["is_default"] == true)
        .map(id)
        .collect();
    assert_eq!(defaults, [id(&second)]);
    // Only one of the organization's types, or none.
    for missing in MISSING_TYPES {
        let (status, answer) = server.send(set_default(json!({ "memory_type_id": missing }))).await;
        assert_eq!(
            refusal(status, &answer),
            refused(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unknown_memory_type",
                &["memory_type_id"]
            ),
            "{missing}"
        );
    }
    for body in [json!({}), json!({ "memory_type_id": 5 })] {
        let (status, answer) = server.send(set_default(body.clone())).await;
        assert_eq!(
            refusal(status, &answer),
            refused(StatusCode::BAD_REQUEST, "validation_failed", &["memory_type_id"]),
            "{body}"
        );
    }
    assert_eq!(
        id(&read(&server, "alice", &default_path(&org)).await["memory_type"]),
        id(&second)
    );
    let (status, cleared) = server.send(set_default(json!({ "memory_type_id": null }))).await;
    assert_eq!(status, StatusCode::OK, "{cleared}");
    assert_eq!(cleared["data"]["memory_type"], Value::Null);
    assert_eq!(cleared["data"]["updated_by"]["email"], "alice@people.example");
    assert!(cleared["data"]["updated_at"].is_string());
    assert_eq!(read(&server, "alice", &default_path(&org)).await, cleared["data"]);
    for made in [&first, &second] {
        assert_eq!(
            &read(&server, "alice", &type_path(&org, id(made))).await,
            made,
            "never the default, and unchanged"
        );
    }
}

#[tokio::test]
async fn replays_making_a_type_for_the_same_idempotency_key() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let request = |name: &str| {
        call("alice", Method::POST, types_path(&org))
            .idempotent("make-once")
            .body(json!({ "name": name, "kind": "notes" }))
    };
    let (status, first) = server.send(request("Once")).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let (status, replay) = server.send(request("Once")).await;
    assert_eq!((status, &replay), (StatusCode::CREATED, &first));
    let (status, reused) = server.send(request("Twice")).await;
    assert_eq!(
        (status, code(&reused)),
        (StatusCode::UNPROCESSABLE_ENTITY, "idempotency_key_reused")
    );
    let listed = server.send(call("alice", Method::GET, types_path(&org))).await.1;
    let ids: Vec<&str> = listed["data"].as_array().unwrap().iter().map(id).collect();
    assert_eq!(ids, [id(&first["data"])], "made once");
}

#[tokio::test]
async fn records_each_change_naming_the_type() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let made = notes(&server, &org, "Audited").await;
    let path = type_path(&org, id(&made));
    for request in [
        call("alice", Method::PATCH, &path).body(json!({ "name": "Renamed" })),
        call("alice", Method::PUT, default_path(&org)).body(json!({ "memory_type_id": id(&made) })),
        call("alice", Method::PUT, default_path(&org)).body(json!({ "memory_type_id": null })),
        call("alice", Method::DELETE, &path),
    ] {
        let (status, body) = server.send(request).await;
        assert!(status.is_success(), "{body}");
    }
    let events = read(
        &server,
        "alice",
        &format!("/v1/organizations/{org}/audit-events?limit=5"),
    )
    .await;
    let recorded: Vec<Value> = events
        .as_array()
        .unwrap()
        .iter()
        .map(|event| {
            json!([
                event["action"],
                event["target"],
                event["workspace_id"],
                event["environment"],
                event["actor"]["type"],
            ])
        })
        .collect();
    let target = |kind: &str, id: Value, name: &str| json!({ "type": kind, "id": id, "name": name });
    // Newest first, each organization-wide: in no workspace and no environment.
    assert_eq!(
        recorded,
        [
            json!([
                "memory_type.deleted",
                target("memory_type", json!(id(&made)), "Renamed"),
                null,
                null,
                "user"
            ]),
            json!([
                "default_memory_type.updated",
                target("default_memory_type", Value::Null, "None"),
                null,
                null,
                "user"
            ]),
            json!([
                "default_memory_type.updated",
                target("default_memory_type", json!(id(&made)), "Renamed"),
                null,
                null,
                "user"
            ]),
            json!([
                "memory_type.updated",
                target("memory_type", json!(id(&made)), "Renamed"),
                null,
                null,
                "user"
            ]),
            json!([
                "memory_type.created",
                target("memory_type", json!(id(&made)), "Audited"),
                null,
                null,
                "user"
            ]),
        ]
    );
}

/// A type outlives its kind: when the server no longer serves it, the type
/// still reads, as not served, and its settings can't be checked.
#[tokio::test]
async fn reads_a_type_of_a_kind_it_no_longer_serves() {
    let mut server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let qa = workspace(&server, &org, "qa", "sandbox").await;
    let made = notes(&server, &org, "Kept").await;
    server.state.memory_providers = MemoryProviders::none();
    server.app = zupflow_server::app(server.state.clone(), zupflow_server::RequestGuard::default());

    assert_eq!(read(&server, "alice", &kinds_path(&org)).await, json!([]));
    let path = type_path(&org, id(&made));
    let mut expected = made.clone();
    expected["served"] = json!(false);
    assert_eq!(read(&server, "alice", &path).await, expected);
    let (status, answer) = server
        .send(call("alice", Method::POST, types_path(&org)).body(json!({ "name": "New", "kind": "notes" })))
        .await;
    assert_eq!(
        refusal(status, &answer),
        refused(StatusCode::UNPROCESSABLE_ENTITY, "unknown_memory_kind", &["kind"])
    );
    // Nothing here can check its settings: they're kept as sent.
    let (status, changed) = server
        .send(call("alice", Method::PATCH, &path).body(json!({ "settings": { "anything": [1, 2] } })))
        .await;
    assert_eq!(status, StatusCode::OK, "{changed}");
    assert_eq!(changed["data"]["settings"], json!({ "anything": [1, 2] }));
    // Its agents run without memory, but may still name it.
    let (status, answer) = create_agent(
        &server,
        "alice",
        &org,
        &qa,
        "Named",
        json!({ "memory": "type", "memory_type_id": id(&made) }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{answer}");
    assert_eq!(agent_count(&server, &org, id(&made)).await, 1);
}
