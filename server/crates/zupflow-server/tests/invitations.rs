//! `/v1/me/invitations`: only the invited person sees and answers an
//! invitation, once, while it hasn't expired, and only through a sign-in that
//! vouches for their address.

#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::{Method, StatusCode};
use common::{Server, call, code};
use serde_json::{Value, json};
use zupflow_core::identity::{Identity, IdentityError, Profile, VerifiedIdentity};
use zupflow_core::organizations::INVITATION_TTL_MS;

/// Signs in `test:{name}` as `{name}@people.example`, verified. The same
/// person signs in as `test:unverified:{name}` when the provider no longer
/// vouches for the address, and as `test:moved:{name}` when it vouches for
/// another one.
struct ChangingIdentity;

#[async_trait]
impl Identity for ChangingIdentity {
    async fn verify(&self, bearer: Option<&str>) -> Result<VerifiedIdentity, IdentityError> {
        let token = bearer
            .and_then(|token| token.strip_prefix("test:"))
            .ok_or(IdentityError::Unauthenticated)?;
        let (name, email, email_verified) = match token.split_once(':') {
            Some(("unverified", name)) => (name, format!("{name}@people.example"), false),
            Some(("moved", name)) => (name, format!("{name}@elsewhere.example"), true),
            _ => (token, format!("{token}@people.example"), true),
        };
        Ok(VerifiedIdentity {
            provider: "test".to_owned(),
            subject: name.to_owned(),
            profile: Profile {
                name: name.to_owned(),
                email,
                email_verified,
                avatar_url: None,
            },
        })
    }
}

/// `alice`'s organization, and an invitation in it for `bob`, who has signed in once.
async fn invited(server: &Server, role: &str) -> (String, String) {
    let org = server.organization("alice", "northwind").await;
    server.send(call("bob", Method::GET, "/v1/me")).await;
    let (status, body) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/members"))
                .body(json!({ "email": "Bob@People.Example", "role": role })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    (org, body["data"]["id"].as_str().unwrap().to_owned())
}

async fn invitations(server: &Server, user: &str) -> Vec<Value> {
    let (status, body) = server.send(call(user, Method::GET, "/v1/me/invitations")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["data"].as_array().unwrap().clone()
}

async fn answer(server: &Server, user: &str, invitation: &str, verb: &str) -> (StatusCode, Value) {
    server
        .send(call(
            user,
            Method::POST,
            format!("/v1/me/invitations/{invitation}/{verb}"),
        ))
        .await
}

fn organizations_of(body: &Value) -> Vec<String> {
    body["data"]["organizations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|organization| organization["id"].as_str().unwrap().to_owned())
        .collect()
}

async fn database(server: &Server) -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect(&format!("sqlite:{}", server.directory.join("zupflow.sqlite").display()))
        .await
        .unwrap()
}

#[tokio::test]
async fn joins_only_when_the_invited_person_accepts() {
    let server = Server::new().await;
    let (org, invitation) = invited(&server, "developer").await;

    let listed = invitations(&server, "bob").await;
    assert_eq!(listed.len(), 1);
    let shown = &listed[0];
    assert_eq!(shown["id"], invitation.as_str());
    assert_eq!(
        shown["organization"],
        json!({ "id": org, "name": "Test", "slug": "northwind" })
    );
    assert_eq!(
        (&shown["role"], &shown["status"], &shown["invited_by"]),
        (
            &json!("developer"),
            &json!("pending"),
            &json!({ "name": "alice", "email": "alice@people.example" })
        )
    );
    assert!(invitations(&server, "alice").await.is_empty(), "nobody else sees it");
    assert!(invitations(&server, "carol").await.is_empty());

    // Signed in with the invited address, bob is still no member.
    let (_, me) = server.send(call("bob", Method::GET, "/v1/me")).await;
    assert!(organizations_of(&me).is_empty());
    let organization = format!("/v1/organizations/{org}");
    assert_eq!(
        server.send(call("bob", Method::GET, &organization)).await.0,
        StatusCode::NOT_FOUND
    );

    let (status, joined) = answer(&server, "bob", &invitation, "accept").await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    assert_eq!(
        (&joined["data"]["id"], &joined["data"]["role"]),
        (&json!(org), &json!("developer"))
    );
    let (_, me) = server.send(call("bob", Method::GET, "/v1/me")).await;
    assert_eq!(organizations_of(&me), vec![org.clone()]);
    assert_eq!(
        server.send(call("bob", Method::GET, &organization)).await.0,
        StatusCode::OK
    );
    let (_, members) = server
        .send(call("alice", Method::GET, format!("{organization}/members")))
        .await;
    let member = members["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|member| member["id"] == invitation.as_str())
        .unwrap()
        .clone();
    assert_eq!(
        (&member["status"], &member["expires_at"]),
        (&json!("active"), &Value::Null)
    );
    assert!(invitations(&server, "bob").await.is_empty());

    // Answered once.
    for verb in ["accept", "decline"] {
        let (status, body) = answer(&server, "bob", &invitation, verb).await;
        assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "invitation_accepted"));
    }
}

#[tokio::test]
async fn answers_anyone_else_as_if_it_did_not_exist() {
    let server = Server::new().await;
    let (org, invitation) = invited(&server, "owner").await;
    let (_, members) = server
        .send(call("alice", Method::GET, format!("/v1/organizations/{org}/members")))
        .await;
    let alice_member = members["data"][0]["id"].as_str().unwrap().to_owned();
    for (user, id) in [
        ("alice", invitation.as_str()),
        ("carol", invitation.as_str()),
        ("bob", alice_member.as_str()),
        ("bob", "mem_short"),
        ("bob", "not-an-id"),
    ] {
        for verb in ["accept", "decline"] {
            let (status, body) = answer(&server, user, id, verb).await;
            assert_eq!(
                (status, code(&body)),
                (StatusCode::NOT_FOUND, "not_found"),
                "{user} {verb} {id}"
            );
        }
    }
    // Signed out: 401 before anything is read.
    let request = axum::http::Request::builder()
        .method(Method::POST)
        .uri(format!("/v1/me/invitations/{invitation}/accept"))
        .body(axum::body::Body::empty())
        .unwrap();
    let response = tower::ServiceExt::oneshot(server.app.clone(), request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // Still bob's to answer.
    assert_eq!(invitations(&server, "bob").await.len(), 1);
}

#[tokio::test]
async fn refuses_an_api_key() {
    let server = Server::new().await;
    let (org, invitation) = invited(&server, "developer").await;
    let (status, created) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/api-keys"))
                .body(json!({ "name": "Backend", "environment": "sandbox" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let secret = created["data"]["secret"].as_str().unwrap().to_owned();
    let (status, body) = server.send(call(&secret, Method::GET, "/v1/me/invitations")).await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "console_only"));
    for key in [None, Some("key-1")] {
        let mut request = call(&secret, Method::POST, format!("/v1/me/invitations/{invitation}/accept"));
        if let Some(key) = key {
            request = request.idempotent(key);
        }
        let (status, body) = server.send(request).await;
        assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "console_only"));
    }
    let (status, body) = server
        .send(call("zf_test_unknown", Method::GET, "/v1/me/invitations"))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNAUTHORIZED, "unauthenticated"),
        "{body}"
    );
    assert_eq!(invitations(&server, "bob").await.len(), 1);
}

#[tokio::test]
async fn needs_a_sign_in_that_vouches_for_the_address() {
    let server = Server::with_identity(Arc::new(ChangingIdentity)).await;
    let (org, invitation) = invited(&server, "developer").await;
    // Bob's provider no longer vouches for his address, or vouches for another one.
    for bob in ["unverified:bob", "moved:bob"] {
        for (method, uri) in [
            (Method::GET, "/v1/me/invitations".to_owned()),
            (Method::POST, format!("/v1/me/invitations/{invitation}/accept")),
            (Method::POST, format!("/v1/me/invitations/{invitation}/decline")),
            // Refused before the ID is read.
            (Method::POST, "/v1/me/invitations/mem_short/accept".to_owned()),
        ] {
            let (status, body) = server.send(call(bob, method, &uri)).await;
            assert_eq!(
                (status, code(&body)),
                (StatusCode::FORBIDDEN, "email_unverified"),
                "{bob} {uri}"
            );
        }
    }
    // Moved, the message doesn't send bob to verify an address his provider no longer has.
    let (_, body) = server.send(call("moved:bob", Method::GET, "/v1/me/invitations")).await;
    let message = body["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("bob@people.example") && !message.contains("Verify"),
        "{message}"
    );
    let (_, body) = server
        .send(call("unverified:bob", Method::GET, "/v1/me/invitations"))
        .await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("Invitations go to bob@people.example. Verify")
    );
    let (_, me) = server.send(call("bob", Method::GET, "/v1/me")).await;
    assert!(organizations_of(&me).is_empty());
    // Vouched for again, bob answers it.
    let (status, joined) = answer(&server, "bob", &invitation, "accept").await;
    assert_eq!((status, &joined["data"]["id"]), (StatusCode::OK, &json!(org)));
}

#[tokio::test]
async fn refuses_an_expired_invitation_but_lets_it_be_declined() {
    let server = Server::new().await;
    let (_, invitation) = invited(&server, "developer").await;
    let pool = database(&server).await;
    sqlx::query("UPDATE members SET invited_at = invited_at - ? WHERE status = 'invited'")
        .bind(INVITATION_TTL_MS)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let listed = invitations(&server, "bob").await;
    assert_eq!(listed[0]["status"], "expired");
    let (status, body) = answer(&server, "bob", &invitation, "accept").await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invitation_expired")
    );
    let (_, me) = server.send(call("bob", Method::GET, "/v1/me")).await;
    assert!(organizations_of(&me).is_empty());
    let (status, _) = answer(&server, "bob", &invitation, "decline").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(invitations(&server, "bob").await.is_empty());
}

#[tokio::test]
async fn withdraws_an_invitation_on_decline() {
    let server = Server::new().await;
    let (org, invitation) = invited(&server, "admin").await;
    let (status, _) = answer(&server, "bob", &invitation, "decline").await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, members) = server
        .send(call("alice", Method::GET, format!("/v1/organizations/{org}/members")))
        .await;
    assert_eq!(members["data"].as_array().unwrap().len(), 1, "only alice");
    for verb in ["accept", "decline"] {
        assert_eq!(answer(&server, "bob", &invitation, verb).await.0, StatusCode::NOT_FOUND);
    }
    // A withdrawn invitation is just as gone.
    let (status, again) = server
        .send(
            call("alice", Method::POST, format!("/v1/organizations/{org}/members"))
                .body(json!({ "email": "bob@people.example", "role": "viewer" })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let again = again["data"]["id"].as_str().unwrap().to_owned();
    let (status, _) = server
        .send(call(
            "alice",
            Method::DELETE,
            format!("/v1/organizations/{org}/members/{again}"),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(answer(&server, "bob", &again, "accept").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn replays_an_answer_sent_again_with_its_key() {
    let server = Server::new().await;
    let (_, invitation) = invited(&server, "developer").await;
    let accept = format!("/v1/me/invitations/{invitation}/accept");
    let first = server
        .send(call("bob", Method::POST, &accept).idempotent("join-1"))
        .await;
    let again = server
        .send(call("bob", Method::POST, &accept).idempotent("join-1"))
        .await;
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(again, first);
    // The key is bob's own: carol's same key runs as her request, and finds nothing.
    let carol = server
        .send(call("carol", Method::POST, &accept).idempotent("join-1"))
        .await;
    assert_eq!(carol.0, StatusCode::NOT_FOUND);
    // A new key is a new answer.
    let (status, body) = server
        .send(call("bob", Method::POST, &accept).idempotent("join-2"))
        .await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "invitation_accepted"));
}

#[tokio::test(flavor = "multi_thread")]
async fn joins_once_when_two_accepts_race() {
    let server = Server::new().await;
    let (org, invitation) = invited(&server, "developer").await;
    let accept = format!("/v1/me/invitations/{invitation}/accept");
    let (first, second) = tokio::join!(
        server.send(call("bob", Method::POST, &accept).idempotent("race-1")),
        server.send(call("bob", Method::POST, &accept).idempotent("race-2")),
    );
    let mut answers = [
        (first.0, code(&first.1).to_owned()),
        (second.0, code(&second.1).to_owned()),
    ];
    answers.sort();
    assert_eq!(
        answers,
        [
            (StatusCode::OK, String::new()),
            (StatusCode::CONFLICT, "invitation_accepted".to_owned())
        ]
    );
    let (_, members) = server
        .send(call("alice", Method::GET, format!("/v1/organizations/{org}/members")))
        .await;
    let bobs: Vec<&Value> = members["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|member| member["user"]["email"] == "bob@people.example")
        .collect();
    assert_eq!(bobs.len(), 1);
    assert_eq!(bobs[0]["status"], "active");
}

/// `user` invites `email` to `org` as `role`: the status and body.
async fn invite(server: &Server, user: &str, org: &str, email: &str, role: &str) -> (StatusCode, Value) {
    server
        .send(
            call(user, Method::POST, format!("/v1/organizations/{org}/members"))
                .body(json!({ "email": email, "role": role })),
        )
        .await
}

async fn set_role(server: &Server, org: &str, member: &str, role: &str) -> StatusCode {
    let (status, body) = server
        .send(
            call(
                "alice",
                Method::PATCH,
                format!("/v1/organizations/{org}/members/{member}"),
            )
            .body(json!({ "role": role })),
        )
        .await;
    assert!(status.is_success() || status.is_client_error(), "{body}");
    status
}

/// Bob, invited to alice's organization as `role`, accepts: his member ID.
async fn bob_joins(server: &Server, role: &str) -> (String, String) {
    let (org, invitation) = invited(server, role).await;
    assert_eq!(answer(server, "bob", &invitation, "accept").await.0, StatusCode::OK);
    (org, invitation)
}

#[tokio::test]
async fn withdraws_what_a_demoted_or_removed_grantor_granted() {
    let server = Server::new().await;
    let (org, bob) = bob_joins(&server, "owner").await;
    server.send(call("carol", Method::GET, "/v1/me")).await;
    let (status, sent) = invite(&server, "bob", &org, "carol@people.example", "owner").await;
    assert_eq!(status, StatusCode::CREATED, "{sent}");
    let for_carol = sent["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        invitations(&server, "carol").await[0]["invited_by"]["email"],
        "bob@people.example"
    );

    // Demoted to admin, bob can't grant owner: carol's invitation is withdrawn with it.
    assert_eq!(set_role(&server, &org, &bob, "admin").await, StatusCode::OK);
    assert!(invitations(&server, "carol").await.is_empty());
    let (status, body) = answer(&server, "carol", &for_carol, "accept").await;
    assert_eq!((status, code(&body)), (StatusCode::NOT_FOUND, "not_found"));
    let (status, body) = invite(&server, "bob", &org, "carol@people.example", "owner").await;
    assert_eq!((status, code(&body)), (StatusCode::FORBIDDEN, "forbidden"));

    // Removed, nothing he granted stands.
    let (status, sent) = invite(&server, "bob", &org, "carol@people.example", "developer").await;
    assert_eq!(status, StatusCode::CREATED, "{sent}");
    let for_carol = sent["data"]["id"].as_str().unwrap().to_owned();
    let (status, _) = server
        .send(call(
            "alice",
            Method::DELETE,
            format!("/v1/organizations/{org}/members/{bob}"),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(invitations(&server, "carol").await.is_empty());
    assert_eq!(
        answer(&server, "carol", &for_carol, "accept").await.0,
        StatusCode::NOT_FOUND
    );
    let (_, me) = server.send(call("carol", Method::GET, "/v1/me")).await;
    assert!(organizations_of(&me).is_empty());
}

#[tokio::test]
async fn makes_whoever_changes_an_invitations_role_its_grantor() {
    let server = Server::new().await;
    let (org, bob) = bob_joins(&server, "admin").await;
    server.send(call("carol", Method::GET, "/v1/me")).await;
    let (_, sent) = invite(&server, "bob", &org, "carol@people.example", "viewer").await;
    let for_carol = sent["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(set_role(&server, &org, &for_carol, "developer").await, StatusCode::OK);
    // Bob's demotion no longer touches it: alice grants it now.
    assert_eq!(set_role(&server, &org, &bob, "viewer").await, StatusCode::OK);
    let listed = invitations(&server, "carol").await;
    assert_eq!(listed[0]["invited_by"]["email"], "alice@people.example");
    let (status, joined) = answer(&server, "carol", &for_carol, "accept").await;
    assert_eq!((status, &joined["data"]["role"]), (StatusCode::OK, &json!("developer")));
}

#[tokio::test]
async fn cancels_only_an_invitation_still_pending() {
    let server = Server::new().await;
    let (org, bob) = bob_joins(&server, "developer").await;
    let cancel = |query: &str| {
        call(
            "alice",
            Method::DELETE,
            format!("/v1/organizations/{org}/members/{bob}?{query}"),
        )
    };
    let (status, body) = server.send(cancel("status=invited")).await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "invitation_accepted"));
    for query in ["status=active", "status="] {
        let (status, body) = server.send(cancel(query)).await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::BAD_REQUEST, "validation_failed"),
            "{query}"
        );
    }
    let (_, me) = server.send(call("bob", Method::GET, "/v1/me")).await;
    assert_eq!(organizations_of(&me), vec![org.clone()], "bob is still a member");
    // A pending one is cancelled.
    let (_, sent) = invite(&server, "alice", &org, "carol@people.example", "viewer").await;
    let for_carol = sent["data"]["id"].as_str().unwrap();
    let (status, _) = server
        .send(call(
            "alice",
            Method::DELETE,
            format!("/v1/organizations/{org}/members/{for_carol}?status=invited"),
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn renews_an_expired_invitation_sent_again() {
    let server = Server::new().await;
    let (org, invitation) = invited(&server, "developer").await;
    let (status, body) = invite(&server, "alice", &org, "bob@people.example", "viewer").await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "already_member"));
    let pool = database(&server).await;
    sqlx::query("UPDATE members SET invited_at = invited_at - ? WHERE status = 'invited'")
        .bind(INVITATION_TTL_MS)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let (status, renewed) = invite(&server, "alice", &org, "bob@people.example", "viewer").await;
    assert_eq!(status, StatusCode::CREATED, "{renewed}");
    assert_eq!(
        (&renewed["data"]["id"], &renewed["data"]["role"]),
        (&json!(invitation), &json!("viewer"))
    );
    assert_eq!(invitations(&server, "bob").await[0]["status"], "pending");
    assert_eq!(answer(&server, "bob", &invitation, "accept").await.0, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn never_lets_an_accept_outrun_its_grantors_demotion() {
    let server = Server::new().await;
    let (org, bob) = bob_joins(&server, "owner").await;
    server.send(call("carol", Method::GET, "/v1/me")).await;
    for round in 0..5 {
        assert_eq!(set_role(&server, &org, &bob, "owner").await, StatusCode::OK);
        let (status, sent) = invite(&server, "bob", &org, "carol@people.example", "owner").await;
        assert_eq!(status, StatusCode::CREATED, "{sent}");
        let for_carol = sent["data"]["id"].as_str().unwrap().to_owned();
        let accept = format!("/v1/me/invitations/{for_carol}/accept");
        let key = format!("race-{round}");
        let ((accepted, _), demoted) = tokio::join!(
            server.send(call("carol", Method::POST, &accept).idempotent(&key)),
            set_role(&server, &org, &bob, "admin"),
        );
        assert_eq!(demoted, StatusCode::OK);
        let (_, me) = server.send(call("carol", Method::GET, "/v1/me")).await;
        match accepted {
            StatusCode::OK => {
                assert_eq!(organizations_of(&me), vec![org.clone()]);
                let (status, _) = server
                    .send(call(
                        "alice",
                        Method::DELETE,
                        format!("/v1/organizations/{org}/members/{for_carol}"),
                    ))
                    .await;
                assert_eq!(status, StatusCode::NO_CONTENT);
            }
            StatusCode::NOT_FOUND => assert!(organizations_of(&me).is_empty()),
            other => panic!("{other}"),
        }
    }
}
