//! The idempotency middleware end to end (decision 0016), the same cases the
//! shared contract suite holds the mock to.

#![allow(clippy::unwrap_used)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Server, call, code};
use serde_json::json;

fn workspace(slug: &str) -> serde_json::Value {
    json!({ "name": "W", "slug": slug, "environment": "sandbox", "external_id": null })
}

async fn count_workspaces(server: &Server, user: &str, org: &str) -> usize {
    server
        .send(call(user, Method::GET, format!("/v1/organizations/{org}/workspaces")))
        .await
        .1["data"]
        .as_array()
        .unwrap()
        .len()
}

#[tokio::test]
async fn replays_a_repeated_post_instead_of_running_it_twice() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let path = format!("/v1/organizations/{org}/workspaces");
    let first = server
        .send(
            call("alice", Method::POST, &path)
                .idempotent("k-1")
                .body(workspace("acme")),
        )
        .await;
    assert_eq!(first.0, StatusCode::CREATED);
    let again = server
        .send(
            call("alice", Method::POST, &path)
                .idempotent("k-1")
                .body(workspace("acme")),
        )
        .await;
    assert_eq!(again, first, "the same status and body, exactly");
    assert_eq!(count_workspaces(&server, "alice", &org).await, 1, "created once");

    // The same key for a different request.
    let (status, body) = server
        .send(
            call("alice", Method::POST, &path)
                .idempotent("k-1")
                .body(workspace("other")),
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "idempotency_key_reused")
    );
    // Without a key, nothing is replayed: the second create meets the taken slug.
    let (status, body) = server
        .send(call("alice", Method::POST, &path).body(workspace("acme")))
        .await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "slug_taken"));
}

#[tokio::test]
async fn replays_a_saved_error_too() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let invite = format!("/v1/organizations/{org}/members");
    let bad = json!({ "email": "not an email", "role": "viewer" });
    let first = server
        .send(call("alice", Method::POST, &invite).idempotent("bad").body(bad.clone()))
        .await;
    assert_eq!(first.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        server
            .send(call("alice", Method::POST, &invite).idempotent("bad").body(bad))
            .await,
        first
    );
}

#[tokio::test]
async fn checks_the_key_after_the_credential_and_before_the_route() {
    let server = Server::new().await;
    let org = server.organization("owner", "northwind").await;
    server.member("owner", &org, "viewer", "viewer").await;
    let path = format!("/v1/organizations/{org}/workspaces");
    let long = "k".repeat(256);
    for bad in ["", "tab\there", long.as_str()] {
        let (status, body) = server
            .send(call("viewer", Method::POST, &path).idempotent(bad).body(workspace("x")))
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::BAD_REQUEST, "invalid_idempotency_key"),
            "{bad:?}: before the viewer's 403"
        );
    }
    // A caller outside the organization gets its 404, never a replay or a 400.
    let (status, body) = server
        .send(
            call("stranger", Method::POST, &path)
                .idempotent("")
                .body(workspace("x")),
        )
        .await;
    assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"));
}

#[tokio::test]
async fn never_shows_a_secret_twice() {
    let server = Server::new().await;
    let org = server.organization("owner", "northwind").await;
    server.member("owner", &org, "developer", "developer").await;
    let path = format!("/v1/organizations/{org}/api-keys");
    let input = json!({ "name": "Server", "environment": "sandbox" });
    let (status, created) = server
        .send(
            call("owner", Method::POST, &path)
                .idempotent("secret")
                .body(input.clone()),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(created["data"]["secret"].is_string());
    let (status, body) = server
        .send(call("owner", Method::POST, &path).idempotent("secret").body(input))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "idempotency_key_completed")
    );
    assert!(!body.to_string().contains(created["data"]["secret"].as_str().unwrap()));
    let (_, keys) = server.send(call("owner", Method::GET, &path)).await;
    assert_eq!(keys["data"].as_array().unwrap().len(), 1, "created once");

    // An error from a secret-once route carries no secret, so it replays.
    let production = json!({ "name": "Nope", "environment": "production" });
    let refused = server
        .send(
            call("developer", Method::POST, &path)
                .idempotent("err")
                .body(production.clone()),
        )
        .await;
    assert_eq!(refused.0, StatusCode::FORBIDDEN);
    assert_eq!(
        server
            .send(
                call("developer", Method::POST, &path)
                    .idempotent("err")
                    .body(production)
            )
            .await,
        refused
    );
}

#[tokio::test]
async fn keeps_a_key_to_the_caller_that_sent_it() {
    let server = Server::new().await;
    let org = server.organization("owner", "northwind").await;
    server.member("owner", &org, "admin", "admin").await;
    let path = format!("/v1/organizations/{org}/workspaces");
    // The same key from two members of one organization: two requests.
    for (user, slug) in [("owner", "by-owner"), ("admin", "by-admin")] {
        let (status, _) = server
            .send(
                call(user, Method::POST, &path)
                    .idempotent("shared")
                    .body(workspace(slug)),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{user}");
    }
    // And from two API keys of that organization, each its own caller.
    for slug in ["by-first-key", "by-second-key"] {
        let (_, key) = server
            .send(
                call("owner", Method::POST, format!("/v1/organizations/{org}/api-keys"))
                    .body(json!({ "name": "k", "environment": "sandbox" })),
            )
            .await;
        let secret = key["data"]["secret"].as_str().unwrap().to_owned();
        let (status, _) = server
            .send(
                call(&secret, Method::POST, &path)
                    .idempotent("shared")
                    .body(workspace(slug)),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{slug}");
    }
    assert_eq!(count_workspaces(&server, "owner", &org).await, 4);

    // Creating an organization: each user's own key space.
    for (user, slug) in [("owner", "first-org"), ("admin", "second-org")] {
        let request = || {
            call(user, Method::POST, "/v1/organizations")
                .idempotent("new-org")
                .body(json!({ "name": "New", "slug": slug }))
        };
        let first = server.send(request()).await;
        assert_eq!(first.0, StatusCode::CREATED, "{user}");
        assert_eq!(server.send(request()).await, first, "{user} replays");
    }
}

/// A key never changes what a route answers an API key it doesn't serve:
/// `403 console_only`, whatever key or organization the request names.
#[tokio::test]
async fn leaves_console_only_routes_to_refuse_api_keys() {
    let server = Server::new().await;
    let northwind = server.organization("alice", "northwind").await;
    let larkspur = server.organization("bob", "larkspur").await;
    let (_, created) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{northwind}/api-keys"))
                .body(json!({ "name": "k", "environment": "sandbox" })),
        )
        .await;
    let secret = created["data"]["secret"].as_str().unwrap().to_owned();
    let invite = json!({ "email": "new@people.example", "role": "viewer" });
    for (org, key) in [
        (&larkspur, None),
        (&larkspur, Some("k")),
        (&northwind, Some("")),
        (&northwind, Some("k")),
    ] {
        let mut request = call(&secret, Method::POST, format!("/v1/organizations/{org}/members")).body(invite.clone());
        if let Some(key) = key {
            request = request.idempotent(key);
        }
        let (status, body) = server.send(request).await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::FORBIDDEN, "console_only"),
            "{org} {key:?}"
        );
    }
    let (status, body) = server
        .send(
            call(&secret, Method::POST, "/v1/organizations")
                .idempotent("k")
                .body(json!({ "name": "By key", "slug": "by-key" })),
        )
        .await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "console_only"));
}
