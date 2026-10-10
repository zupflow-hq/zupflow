//! API keys end to end: created and shown once, managed by role, refused once
//! revoked, and, as a caller, bound to one organization and one environment.

#![allow(clippy::unwrap_used)]

mod common;

use axum::http::{Method, StatusCode};
use common::{Server, call, code};
use serde_json::{Value, json};

/// Creates a key as `user`; its secret and its ID.
async fn key(server: &Server, user: &str, org: &str, environment: &str) -> (String, String) {
    let (status, body) = server
        .send(
            call(user, Method::POST, format!("/v1/organizations/{org}/api-keys"))
                .body(json!({ "name": "Server", "environment": environment })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    (
        body["data"]["secret"].as_str().unwrap().to_owned(),
        body["data"]["key"]["id"].as_str().unwrap().to_owned(),
    )
}

#[tokio::test]
async fn shows_a_secret_once_and_never_again() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let path = format!("/v1/organizations/{org}/api-keys");
    let (status, created) = server
        .send(call("alice", Method::POST, &path).body(json!({ "name": "  Billing  ", "environment": "production" })))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let secret = created["data"]["secret"].as_str().unwrap();
    let key = &created["data"]["key"];
    assert!(secret.starts_with("zf_live_"));
    assert!(key["id"].as_str().unwrap().starts_with("key_"));
    assert_eq!(
        (&key["name"], &key["environment"], &key["organization_id"]),
        (&json!("Billing"), &json!("production"), &json!(org))
    );
    assert!(secret.starts_with(key["prefix"].as_str().unwrap()));
    assert!(secret.ends_with(key["last4"].as_str().unwrap()));
    assert_eq!(key["created_by"]["email"], "alice@people.example");
    assert_eq!((&key["last_used_at"], &key["revoked_at"]), (&Value::Null, &Value::Null));

    let (_, list) = server.send(call("alice", Method::GET, &path)).await;
    assert_eq!(list["data"].as_array().unwrap(), &vec![key.clone()]);
    assert!(!list.to_string().contains(secret), "the secret isn't listed");

    let (status, invalid) = server
        .send(call("alice", Method::POST, &path).body(json!({ "name": "", "environment": "staging" })))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        invalid["error"]["fields"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["environment", "name"]
    );
}

#[tokio::test]
async fn lets_developers_manage_sandbox_keys_and_admins_production_ones() {
    let server = Server::new().await;
    let org = server.organization("owner", "northwind").await;
    for (user, role) in [("admin", "admin"), ("developer", "developer"), ("viewer", "viewer")] {
        server.member("owner", &org, user, role).await;
    }
    let path = format!("/v1/organizations/{org}/api-keys");
    let create = |user: &'static str, environment: &str| {
        call(user, Method::POST, &path).body(json!({ "name": "k", "environment": environment }))
    };
    let (status, body) = server.send(create("developer", "sandbox")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(body["data"]["secret"].as_str().unwrap().starts_with("zf_test_"));
    for (user, environment) in [
        ("developer", "production"),
        ("viewer", "sandbox"),
        ("viewer", "production"),
    ] {
        let (status, body) = server.send(create(user, environment)).await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::FORBIDDEN, "forbidden"),
            "{user} {environment}"
        );
    }
    assert_eq!(server.send(create("admin", "production")).await.0, StatusCode::CREATED);
    let (status, body) = server.send(call("viewer", Method::POST, &path).body(json!({}))).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "forbidden"),
        "the role is checked before the body"
    );

    let (_, production) = key(&server, "owner", &org, "production").await;
    let (_, sandbox) = key(&server, "owner", &org, "sandbox").await;
    let revoke = |user: &'static str, id: &str| call(user, Method::POST, format!("{path}/{id}/revoke"));
    assert_eq!(
        server.send(revoke("developer", &production)).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(server.send(revoke("viewer", &sandbox)).await.0, StatusCode::FORBIDDEN);
    let (status, revoked) = server.send(revoke("developer", &sandbox)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(revoked["data"]["revoked_at"].is_string());
    let (status, body) = server.send(revoke("developer", &sandbox)).await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "already_revoked"));
    assert_eq!(server.send(revoke("admin", &production)).await.0, StatusCode::OK);
}

#[tokio::test]
async fn hides_another_organizations_keys_as_404() {
    let server = Server::new().await;
    let northwind = server.organization("alice", "northwind").await;
    let larkspur = server.organization("bob", "larkspur").await;
    let (_, theirs) = key(&server, "alice", &northwind, "sandbox").await;
    for (method, uri) in [
        (Method::GET, format!("/v1/organizations/{northwind}/api-keys")),
        (
            Method::POST,
            format!("/v1/organizations/{northwind}/api-keys/{theirs}/revoke"),
        ),
        (
            Method::POST,
            format!("/v1/organizations/{larkspur}/api-keys/{theirs}/revoke"),
        ),
        (
            Method::POST,
            format!("/v1/organizations/{larkspur}/api-keys/not-a-key/revoke"),
        ),
    ] {
        let (status, body) = server.send(call("bob", method.clone(), &uri)).await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::NOT_FOUND, "not_found"),
            "{method} {uri}"
        );
    }
    let (_, list) = server
        .send(call(
            "alice",
            Method::GET,
            format!("/v1/organizations/{northwind}/api-keys"),
        ))
        .await;
    assert_eq!(list["data"][0]["revoked_at"], Value::Null, "untouched");
}

#[tokio::test]
async fn acts_as_its_organization_in_its_environment_only() {
    let server = Server::new().await;
    let northwind = server.organization("alice", "northwind").await;
    let larkspur = server.organization("bob", "larkspur").await;
    let sandbox_ws = server.workspace("alice", &northwind, "acme-sandbox", "sandbox").await;
    let production_ws = server.workspace("alice", &northwind, "acme", "production").await;
    let harbor = server.workspace("bob", &larkspur, "harbor", "sandbox").await;
    let (secret, key_id) = key(&server, "alice", &northwind, "sandbox").await;
    let secret = secret.as_str();
    let id = |workspace: &Value| workspace["id"].as_str().unwrap().to_owned();

    // Its own environment, with no header needed.
    let (status, list) = server
        .send(call(
            secret,
            Method::GET,
            format!("/v1/organizations/{northwind}/workspaces"),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["data"].as_array().unwrap().len(), 1, "only Sandbox workspaces");
    assert_eq!(list["data"][0]["id"], json!(id(&sandbox_ws)));
    let (status, _) = server
        .send(call(secret, Method::GET, format!("/v1/workspaces/{}", id(&sandbox_ws))))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, created) = server
        .send(
            call(
                secret,
                Method::POST,
                format!("/v1/organizations/{northwind}/workspaces"),
            )
            .body(json!({ "name": "By key", "slug": "by-key", "environment": "sandbox", "external_id": null })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");

    // Never the other environment.
    let (status, body) = server
        .send(call(
            secret,
            Method::GET,
            format!("/v1/organizations/{northwind}/workspaces?environment=production"),
        ))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "environment_mismatch")
    );
    let (status, body) = server
        .send(
            call(
                secret,
                Method::POST,
                format!("/v1/organizations/{northwind}/workspaces"),
            )
            .body(json!({ "name": "Nope", "slug": "nope", "environment": "production", "external_id": null })),
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "environment_mismatch")
    );
    for method in [Method::GET, Method::DELETE] {
        let (status, body) = server
            .send(call(
                secret,
                method.clone(),
                format!("/v1/workspaces/{}", id(&production_ws)),
            ))
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::NOT_FOUND, "not_found"),
            "{method} Production"
        );
    }

    // Never another organization, whatever it names.
    for request in [
        call(secret, Method::GET, format!("/v1/organizations/{larkspur}/workspaces")),
        call(secret, Method::GET, format!("/v1/workspaces/{}", id(&harbor))),
        call(secret, Method::GET, format!("/v1/workspaces/{}", id(&harbor))).naming(&larkspur),
        call(secret, Method::GET, format!("/v1/workspaces/{}", id(&sandbox_ws))).naming(&larkspur),
    ] {
        let uri = request.uri.clone();
        let (status, body) = server.send(request).await;
        assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"), "{uri}");
    }

    // Nor the Console's own routes.
    for (method, uri) in [
        (Method::GET, "/v1/me".to_owned()),
        (Method::GET, format!("/v1/organizations/{northwind}")),
        (Method::GET, format!("/v1/organizations/{northwind}/members")),
        (Method::GET, format!("/v1/organizations/{northwind}/api-keys")),
    ] {
        let (status, body) = server.send(call(secret, method, &uri)).await;
        assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "console_only"), "{uri}");
    }

    // Its use is recorded, and revoking it ends it.
    let path = format!("/v1/organizations/{northwind}/api-keys");
    let (_, keys) = server.send(call("alice", Method::GET, &path)).await;
    assert!(keys["data"][0]["last_used_at"].is_string());
    server
        .send(call("alice", Method::POST, format!("{path}/{key_id}/revoke")))
        .await;
    let (status, body) = server
        .send(call(
            secret,
            Method::GET,
            format!("/v1/organizations/{northwind}/workspaces"),
        ))
        .await;
    assert_eq!((status, code(&body)), (StatusCode::UNAUTHORIZED, "unauthenticated"));
}

/// A `zf_…` bearer is only ever an API key. Under `local` sign-in, whose
/// identity adapter vouches for any bearer, a revoked or forged key must
/// still be refused, never handed to the adapter and signed in as the owner.
#[tokio::test]
async fn never_lets_a_dead_key_through_an_adapter_that_accepts_anything() {
    let server = Server::with_identity(std::sync::Arc::new(zupflow_basic::local_identity::LocalIdentity)).await;
    let org = server.organization("anyone", "northwind").await;
    let (secret, id) = key(&server, "anyone", &org, "sandbox").await;
    let path = format!("/v1/organizations/{org}/workspaces");
    assert_eq!(server.send(call(&secret, Method::GET, &path)).await.0, StatusCode::OK);
    server
        .send(call(
            "anyone",
            Method::POST,
            format!("/v1/organizations/{org}/api-keys/{id}/revoke"),
        ))
        .await;
    let forged = format!("{}x", &secret[..secret.len() - 1]);
    for dead in [secret.as_str(), forged.as_str(), "zf_live_"] {
        let (status, body) = server.send(call(dead, Method::GET, &path)).await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::UNAUTHORIZED, "unauthenticated"),
            "{dead}"
        );
        let (status, _) = server.send(call(dead, Method::GET, "/v1/me")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{dead} is never the local owner");
    }
}

/// The reverse of the Sandbox case above: a Production key never sees
/// Sandbox, and a key changes nothing in another organization.
#[tokio::test]
async fn keeps_a_production_key_out_of_sandbox_and_other_organizations() {
    let server = Server::new().await;
    let northwind = server.organization("alice", "northwind").await;
    let larkspur = server.organization("bob", "larkspur").await;
    let sandbox_ws = server.workspace("alice", &northwind, "acme-sandbox", "sandbox").await;
    let production_ws = server.workspace("alice", &northwind, "acme", "production").await;
    let harbor = server.workspace("bob", &larkspur, "harbor", "production").await;
    let (secret, _) = key(&server, "alice", &northwind, "production").await;
    let secret = secret.as_str();
    let id = |workspace: &Value| workspace["id"].as_str().unwrap().to_owned();
    let list = format!("/v1/organizations/{northwind}/workspaces");

    let (_, listed) = server.send(call(secret, Method::GET, &list)).await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);
    assert_eq!(listed["data"][0]["id"], json!(id(&production_ws)), "only Production");
    let (status, body) = server
        .send(call(secret, Method::GET, format!("{list}?environment=sandbox")))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "environment_mismatch")
    );
    let (status, body) = server
        .send(
            call(secret, Method::POST, &list)
                .body(json!({ "name": "Nope", "slug": "nope", "environment": "sandbox", "external_id": null })),
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "environment_mismatch")
    );
    for (method, body) in [
        (Method::GET, None),
        (Method::PATCH, Some(json!({ "name": "Taken" }))),
        (Method::DELETE, None),
    ] {
        let mut request = call(secret, method.clone(), format!("/v1/workspaces/{}", id(&sandbox_ws)));
        if let Some(body) = body.clone() {
            request = request.body(body);
        }
        let (status, answer) = server.send(request).await;
        assert_eq!(
            (status, code(&answer)),
            (StatusCode::NOT_FOUND, "not_found"),
            "{method} Sandbox"
        );

        for naming in [None, Some(larkspur.as_str()), Some(" "), Some("org_nope")] {
            let mut request = call(secret, method.clone(), format!("/v1/workspaces/{}", id(&harbor)));
            if let Some(organization) = naming {
                request = request.naming(organization);
            }
            if let Some(body) = body.clone() {
                request = request.body(body);
            }
            let (status, answer) = server.send(request).await;
            assert_eq!(
                (status, code(&answer)),
                (StatusCode::NOT_FOUND, "not_found"),
                "{method} another organization's naming {naming:?}"
            );
        }
    }
    // Nothing changed.
    let (status, still) = server
        .send(call("alice", Method::GET, format!("/v1/workspaces/{}", id(&sandbox_ws))).naming(&northwind))
        .await;
    assert_eq!((status, &still["data"]["name"]), (StatusCode::OK, &sandbox_ws["name"]));
    let (status, theirs) = server
        .send(call("bob", Method::GET, format!("/v1/workspaces/{}", id(&harbor))).naming(&larkspur))
        .await;
    assert_eq!((status, &theirs["data"]["name"]), (StatusCode::OK, &harbor["name"]));
    // A blank or unreadable header names nothing, even with the key's own workspace.
    let blank = call(secret, Method::GET, format!("/v1/workspaces/{}", id(&production_ws))).naming(" ");
    assert_eq!(server.send(blank).await.0, StatusCode::NOT_FOUND);
    let malformed = call(secret, Method::GET, format!("/v1/workspaces/{}", id(&production_ws))).naming("org_nope");
    assert_eq!(server.send(malformed).await.0, StatusCode::NOT_FOUND);
    let unreadable = axum::http::Request::builder()
        .uri(format!("/v1/workspaces/{}", id(&production_ws)))
        .header("Authorization", format!("Bearer {secret}"))
        .header(
            "Zupflow-Organization",
            axum::http::HeaderValue::from_bytes(b"\xff\xfe").unwrap(),
        )
        .body(axum::body::Body::empty())
        .unwrap();
    let response = tower::ServiceExt::oneshot(server.app.clone(), unreadable)
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "an unreadable header names nothing"
    );
    // A header with the key's own organization is fine.
    let own = call(secret, Method::GET, format!("/v1/workspaces/{}", id(&production_ws))).naming(&northwind);
    assert_eq!(server.send(own).await.0, StatusCode::OK);
}

#[tokio::test]
async fn refuses_a_key_it_did_not_issue() {
    let server = Server::new().await;
    let org = server.organization("alice", "northwind").await;
    let (secret, _) = key(&server, "alice", &org, "sandbox").await;
    // The last character flipped to another, so it never equals the real key.
    let last = if secret.ends_with('x') { 'y' } else { 'x' };
    let altered = format!("{}{last}", &secret[..secret.len() - 1]);
    for forged in [
        altered.as_str(),
        "zf_test_",
        "zf_live_0000aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        let (status, body) = server
            .send(call(forged, Method::GET, format!("/v1/organizations/{org}/workspaces")))
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::UNAUTHORIZED, "unauthenticated"),
            "{forged}"
        );
    }
}
