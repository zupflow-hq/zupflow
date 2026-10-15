//! The crate against [`FakeSlack`]: every request shape Slack documents,
//! every answer read into Zupflow's terms, and the fixtures in `fixtures/`
//! (hand-built in Slack's documented shapes) read as the inbound job will.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use zeroize::Zeroizing;
use zupflow_core::channels::ChannelProviderKind;
use zupflow_core::channels::ConversationKind;
use zupflow_core::channels::provider::{
    ChannelProvider, DeletedMessage, EditedMessage, InboundDelivery, InboundEvent, InboundRefusal, InboundRequest,
    MessageRef, NormalizedMessage, OutgoingPost, ProviderCallError, ProviderPerson,
};
use zupflow_core::channels::provision::{
    AppManifest, AppProvisioner, ConsentExchange, ConsentRequest, InstallStep, ManagerTokens, ProvisionError,
    SLACK_EVENTS, SLACK_SCOPES,
};
use zupflow_core::time::Timestamp;

use crate::api::{SlackCode, SlackError};
use crate::events::{SlackChannelProvider, sign, ts_time};
use crate::fake::{Call, FAKE_API_URL, FakeSlack, Fault, ISSUED_AT, TOKEN_SECONDS};
use crate::manifest::{digest, slack_manifest};
use crate::provision::SlackProvisioner;
use crate::send::{MARKDOWN_MAX, Post, SEND_EVENT_TYPE, SlackClient};

const TEAM: &str = "T0FAKE001";
const CONFIGURATION: &str = "xoxe.xoxp-1-pasted";
const REFRESH: &str = "xoxe-1-pasted-refresh";
const REDIRECT: &str = "https://zupflow.example/connect/oauth/callback";
const EVENTS_URL: &str = "https://zupflow.example/channels/inbound/chin_0123456789abcdef";

fn tokens(configuration_token: &str, refresh_token: &str) -> ManagerTokens {
    ManagerTokens {
        configuration_token: Zeroizing::new(configuration_token.to_owned()),
        refresh_token: Zeroizing::new(refresh_token.to_owned()),
    }
}

fn agent_manifest(name: &str) -> AppManifest {
    AppManifest {
        name: name.to_owned(),
        description: "Answers questions about the roadmap.".to_owned(),
        handle: "atlas".to_owned(),
        scopes: SLACK_SCOPES,
        events: SLACK_EVENTS,
        events_url: EVENTS_URL.to_owned(),
        redirect_url: REDIRECT.to_owned(),
    }
}

/// A fake with a linked team, and the provisioner pointed at it.
fn linked() -> (FakeSlack, SlackProvisioner) {
    let slack = FakeSlack::new();
    slack.issue_configuration_token(CONFIGURATION, REFRESH, TEAM);
    (slack, SlackProvisioner::new(FakeSlack::api()))
}

fn manifest_sent(call: &Call) -> Value {
    serde_json::from_str(&call.form["manifest"]).expect("the manifest argument is JSON")
}

// --- Manifests and the configuration token ---------------------------------

#[tokio::test]
async fn a_pasted_token_is_checked_with_validate_and_refused_when_slack_refuses_it() {
    let (slack, provisioner) = linked();
    let checked = provisioner
        .check_tokens(&slack, ChannelProviderKind::Slack, &tokens(CONFIGURATION, REFRESH))
        .await;
    // Its age isn't known, so it has no expiry: the rotation job rotates it first.
    assert_eq!(checked.map(|checked| checked.expires_at), Ok(None));
    let calls = slack.calls_to("apps.manifest.validate");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].authorization.as_deref(), Some("Bearer xoxe.xoxp-1-pasted"));
    assert_eq!(
        calls[0].content_type.as_deref(),
        Some("application/x-www-form-urlencoded")
    );
    assert!(slack.apps().is_empty(), "validating makes nothing");

    for token in ["xoxe.xoxp-1-someone-elses", "xoxb-1-a-bot-token"] {
        let refused = provisioner
            .check_tokens(&slack, ChannelProviderKind::Slack, &tokens(token, REFRESH))
            .await;
        assert_eq!(refused.map(|_| ()), Err(ProvisionError::TokensRefused), "{token}");
    }
    slack.fail_next("apps.manifest.validate", Fault::Status(503));
    let unavailable = provisioner
        .check_tokens(&slack, ChannelProviderKind::Slack, &tokens(CONFIGURATION, REFRESH))
        .await;
    assert_eq!(unavailable.map(|_| ()), Err(ProvisionError::Unavailable));
}

#[tokio::test]
async fn an_app_is_made_from_the_agents_manifest() {
    let (slack, provisioner) = linked();
    let created = provisioner
        .create(&slack, &tokens(CONFIGURATION, REFRESH), &agent_manifest("Atlas"))
        .await
        .expect("made");
    let app = &slack.apps()[0];
    assert_eq!(created.external_app_id, app.id);
    assert_eq!(created.client_id, app.client_id);
    assert_eq!(*created.secrets.client_secret, app.client_secret);
    assert_eq!(*created.secrets.signing_secret, app.signing_secret);
    assert!(matches!(created.install, InstallStep::NeedsConsent));
    assert!(!format!("{created:?}").contains(&app.client_secret), "never printed");

    let call = &slack.calls_to("apps.manifest.create")[0];
    assert_eq!(call.authorization.as_deref(), Some("Bearer xoxe.xoxp-1-pasted"));
    assert_eq!(call.form.keys().collect::<Vec<_>>(), ["manifest"]);
    assert_eq!(
        manifest_sent(call),
        json!({
            "display_information": { "name": "Atlas", "description": "Answers questions about the roadmap." },
            "features": {
                "app_home": {
                    "home_tab_enabled": false,
                    "messages_tab_enabled": true,
                    "messages_tab_read_only_enabled": false,
                },
                "bot_user": { "display_name": "atlas", "always_online": true },
            },
            "oauth_config": { "redirect_urls": [REDIRECT], "scopes": { "bot": SLACK_SCOPES } },
            "settings": {
                "org_deploy_enabled": false,
                "socket_mode_enabled": false,
                "token_rotation_enabled": false,
            },
        }),
        "made without its events: Slack would check their URL before its secret could be stored"
    );
    assert!(slack.deliveries().is_empty());

    // Then the whole manifest: Slack checks the events URL with a challenge
    // signed by the app's own secret, which the app now holds.
    provisioner
        .update(
            &slack,
            &tokens(CONFIGURATION, REFRESH),
            &created.external_app_id,
            &agent_manifest("Atlas"),
        )
        .await
        .expect("subscribed");
    let update = &slack.calls_to("apps.manifest.update")[0];
    assert_eq!(
        manifest_sent(update)["settings"]["event_subscriptions"],
        json!({ "request_url": EVENTS_URL, "bot_events": SLACK_EVENTS })
    );
    let deliveries = slack.deliveries();
    assert_eq!(deliveries.len(), 1);
    let challenge = &deliveries[0];
    assert_eq!(challenge.url, EVENTS_URL);
    let request = InboundRequest {
        timestamp: Some(&challenge.timestamp),
        signature: Some(&challenge.signature),
        body: challenge.body.as_bytes(),
    };
    let provider = SlackChannelProvider::default();
    assert_eq!(
        provider.verify(&request, created.secrets.signing_secret.as_bytes(), Timestamp::now()),
        Ok(())
    );
    assert_eq!(
        provider.verify(&request, b"another-apps-signing-secret", Timestamp::now()),
        Err(InboundRefusal::InvalidSignature)
    );
    assert_eq!(
        provider.read(challenge.body.as_bytes(), Timestamp::now()),
        Ok(InboundDelivery::UrlVerification {
            challenge: format!("challenge-{}", created.external_app_id)
        })
    );
    // The same URL again isn't checked again.
    provisioner
        .update(
            &slack,
            &tokens(CONFIGURATION, REFRESH),
            &created.external_app_id,
            &agent_manifest("Atlas 2"),
        )
        .await
        .expect("changed");
    assert_eq!(slack.deliveries().len(), 1);
    // Sorted scopes, so the same agent is the same bytes.
    let mut sorted = SLACK_SCOPES.to_vec();
    sorted.sort_unstable();
    assert_eq!(sorted, SLACK_SCOPES);
}

#[test]
fn a_manifest_is_the_same_bytes_for_the_same_agent() {
    let manifest = agent_manifest("Atlas");
    assert_eq!(digest(&manifest), digest(&agent_manifest("Atlas")));
    assert_eq!(digest(&manifest).len(), 64);
    assert_ne!(digest(&manifest), digest(&agent_manifest("Atlas 2")));
    let mut quiet = agent_manifest("Atlas");
    quiet.description = "  ".to_owned();
    let sent = slack_manifest(&quiet);
    assert!(sent["display_information"].get("description").is_none());
    assert!(sent["features"].get("assistant_view").is_none(), "no assistant threads");
    let mut long = agent_manifest("Atlas");
    long.description = "d".repeat(500);
    long.handle = "h".repeat(200);
    let sent = slack_manifest(&long);
    assert_eq!(
        sent["display_information"]["description"].as_str().map(str::len),
        Some(140)
    );
    assert_eq!(
        sent["features"]["bot_user"]["display_name"].as_str().map(str::len),
        Some(80)
    );
}

#[tokio::test]
async fn an_app_is_changed_and_deleted_by_its_id() {
    let (slack, provisioner) = linked();
    let pair = tokens(CONFIGURATION, REFRESH);
    let created = provisioner
        .create(&slack, &pair, &agent_manifest("Atlas"))
        .await
        .expect("made");
    provisioner
        .update(&slack, &pair, &created.external_app_id, &agent_manifest("Atlas Prime"))
        .await
        .expect("changed");
    let call = &slack.calls_to("apps.manifest.update")[0];
    assert_eq!(call.form["app_id"], created.external_app_id);
    assert_eq!(manifest_sent(call)["display_information"]["name"], "Atlas Prime");
    assert_eq!(slack.apps()[0].manifest["display_information"]["name"], "Atlas Prime");

    provisioner
        .delete(&slack, &pair, &created.external_app_id)
        .await
        .expect("deleted");
    let call = &slack.calls_to("apps.manifest.delete")[0];
    assert_eq!(call.form.keys().collect::<Vec<_>>(), ["app_id"]);
    assert!(slack.apps()[0].deleted);
    // Deleted already: the job's at-least-once delete treats this as done.
    assert_eq!(
        provisioner.delete(&slack, &pair, &created.external_app_id).await,
        Err(ProvisionError::AppNotFound)
    );
    assert_eq!(
        provisioner
            .update(&slack, &pair, "A0NOTTHERE", &agent_manifest("Atlas"))
            .await,
        Err(ProvisionError::AppNotFound)
    );
    // Slack can't look an app up: a lost create can't be found.
    assert_eq!(provisioner.find(&slack, &pair, EVENTS_URL).await, Ok(None));
}

#[tokio::test]
async fn tier_one_limits_are_waited_out_with_slacks_retry_after() {
    let (slack, provisioner) = linked();
    let pair = tokens(CONFIGURATION, REFRESH);
    slack.fail_next("apps.manifest.create", Fault::RateLimited { secs: 42 });
    assert_eq!(
        provisioner
            .create(&slack, &pair, &agent_manifest("Atlas"))
            .await
            .map(|_| ()),
        Err(ProvisionError::RateLimited { retry_after_secs: 42 })
    );
    assert!(slack.apps().is_empty(), "nothing made");
    // Within reason: an absurd wait is capped at an hour.
    slack.fail_next("apps.manifest.create", Fault::RateLimited { secs: 86_400 });
    assert_eq!(
        provisioner
            .create(&slack, &pair, &agent_manifest("Atlas"))
            .await
            .map(|_| ()),
        Err(ProvisionError::RateLimited { retry_after_secs: 3600 })
    );
    // `ok: false, error: ratelimited` without a header is Slack's minute.
    slack.fail_next("apps.manifest.update", Fault::Error("ratelimited".to_owned()));
    assert_eq!(
        provisioner.update(&slack, &pair, "A0X", &agent_manifest("Atlas")).await,
        Err(ProvisionError::RateLimited { retry_after_secs: 60 })
    );
}

#[tokio::test]
async fn slacks_failures_become_stable_codes() {
    let (slack, provisioner) = linked();
    let pair = tokens(CONFIGURATION, REFRESH);
    let cases = [
        (Fault::Status(500), ProvisionError::Unavailable),
        (Fault::Status(502), ProvisionError::Unavailable),
        (Fault::TimedOut, ProvisionError::Unavailable),
        (
            Fault::Error("invalid_manifest".to_owned()),
            ProvisionError::Refused("invalid_manifest"),
        ),
        (Fault::Error("token_expired".to_owned()), ProvisionError::TokensRefused),
        (
            Fault::Error("some_new_error_with_<b>markup</b>".to_owned()),
            ProvisionError::Refused("provider_refused"),
        ),
    ];
    for (fault, expected) in cases {
        slack.fail_next("apps.manifest.create", fault.clone());
        let result = provisioner.create(&slack, &pair, &agent_manifest("Atlas")).await;
        assert_eq!(result.map(|_| ()), Err(expected), "{fault:?}");
    }
    assert!(slack.apps().is_empty());
}

// --- Rotation ---------------------------------------------------------------

#[tokio::test]
async fn a_refresh_token_is_spent_once_for_a_new_pair() {
    let (slack, provisioner) = linked();
    let rotated = provisioner
        .rotate_tokens(&slack, &tokens(CONFIGURATION, REFRESH))
        .await
        .expect("rotated");
    let call = &slack.calls_to("tooling.tokens.rotate")[0];
    assert_eq!(call.authorization, None, "the refresh token is the credential");
    assert_eq!(call.form.get("refresh_token").map(String::as_str), Some(REFRESH));
    assert_eq!(*rotated.tokens.configuration_token, "xoxe.xoxp-1-rotated-1");
    assert_eq!(*rotated.tokens.refresh_token, "xoxe-1-refresh-1");
    // Slack's `exp`, twelve hours on.
    let expected = Timestamp::now().millis() + TOKEN_SECONDS * 1000;
    let expires_at = rotated.expires_at.expect("an expiry").millis();
    assert!((expected - 5_000..=expected).contains(&expires_at), "{expires_at}");
    assert!(!format!("{rotated:?}").contains("rotated-1"), "never printed");
    // The old refresh token is spent; the new pair works.
    assert_eq!(
        provisioner
            .rotate_tokens(&slack, &tokens(CONFIGURATION, REFRESH))
            .await
            .map(|_| ()),
        Err(ProvisionError::TokensRefused)
    );
    provisioner
        .check_tokens(&slack, ChannelProviderKind::Slack, &rotated.tokens)
        .await
        .expect("the new configuration token works");
    let again = provisioner
        .rotate_tokens(&slack, &rotated.tokens)
        .await
        .expect("rotated again");
    assert_eq!(*again.tokens.refresh_token, "xoxe-1-refresh-2");
    assert_eq!(slack.rotations(), 2);
}

#[tokio::test]
async fn a_rotation_whose_answer_is_lost_spends_the_refresh_token_all_the_same() {
    // Why the job keeps one rotation at a time and stores the pair before
    // finishing: Slack has already spent the old refresh token.
    let (slack, provisioner) = linked();
    slack.fail_next("tooling.tokens.rotate", Fault::LoseAnswer);
    assert_eq!(
        provisioner
            .rotate_tokens(&slack, &tokens(CONFIGURATION, REFRESH))
            .await
            .map(|_| ()),
        Err(ProvisionError::Unavailable)
    );
    assert_eq!(slack.rotations(), 1);
    assert_eq!(
        provisioner
            .rotate_tokens(&slack, &tokens(CONFIGURATION, REFRESH))
            .await
            .map(|_| ()),
        Err(ProvisionError::TokensRefused)
    );
}

#[tokio::test]
async fn two_rotations_racing_with_one_refresh_token_get_one_pair() {
    // Slack spends a refresh token once, so a second rotation racing the
    // first is refused. The server's job keeps this from happening (one
    // leased job per manager and version); this is what it guards against.
    let slack = Arc::new(FakeSlack::new());
    slack.issue_configuration_token(CONFIGURATION, REFRESH, TEAM);
    slack.slow("tooling.tokens.rotate", Duration::from_millis(20));
    let provisioner = Arc::new(SlackProvisioner::new(FakeSlack::api()));
    let racers: Vec<_> = (0..2)
        .map(|_| {
            let slack = Arc::clone(&slack);
            let provisioner = Arc::clone(&provisioner);
            tokio::spawn(async move {
                provisioner
                    .rotate_tokens(slack.as_ref(), &tokens(CONFIGURATION, REFRESH))
                    .await
                    .map(|_| ())
            })
        })
        .collect();
    let mut results = Vec::new();
    for racer in racers {
        results.push(racer.await.expect("joined"));
    }
    results.sort_by_key(Result::is_err);
    assert_eq!(results, [Ok(()), Err(ProvisionError::TokensRefused)]);
    assert_eq!(slack.rotations(), 1);
}

// --- Consent and install ----------------------------------------------------

#[test]
fn the_consent_page_is_slacks_with_the_apps_own_client() {
    let provisioner = SlackProvisioner::default();
    let url = provisioner
        .consent_url(&ConsentRequest {
            client_id: "1000.42",
            scopes: SLACK_SCOPES,
            redirect_url: REDIRECT,
            state: "cs_state&evil=1",
        })
        .expect("a URL");
    let parsed = url::Url::parse(&url).expect("absolute");
    assert_eq!(parsed.origin().ascii_serialization(), "https://slack.com");
    assert_eq!(parsed.path(), "/oauth/v2/authorize");
    let query: BTreeMap<String, String> = parsed.query_pairs().into_owned().collect();
    assert_eq!(query["client_id"], "1000.42");
    assert_eq!(query["scope"], SLACK_SCOPES.join(","));
    assert_eq!(query["redirect_uri"], REDIRECT);
    assert_eq!(query["state"], "cs_state&evil=1");
    assert_eq!(query.len(), 4, "no user scopes");
}

/// An app made in the fake, and a code someone in `team` allowed it with:
/// its client ID, client secret and the code.
async fn made_and_allowed(slack: &FakeSlack, provisioner: &SlackProvisioner, team: &str) -> (String, String, String) {
    let created = provisioner
        .create(slack, &tokens(CONFIGURATION, REFRESH), &agent_manifest("Atlas"))
        .await
        .expect("made");
    let code = slack.allow(&created.external_app_id, team, "Lakeside");
    (created.client_id, created.secrets.client_secret.to_string(), code)
}

fn exchange<'a>(client_id: &'a str, client_secret: &'a str, code: &'a str) -> ConsentExchange<'a> {
    ConsentExchange {
        client_id,
        client_secret,
        code,
        redirect_url: REDIRECT,
    }
}

#[tokio::test]
async fn an_allowed_install_is_exchanged_for_the_bot_token_with_the_apps_own_credentials() {
    let (slack, provisioner) = linked();
    let (client_id, client_secret, code) = made_and_allowed(&slack, &provisioner, TEAM).await;
    let allowed = exchange(&client_id, &client_secret, &code);
    assert!(!format!("{allowed:?}").contains(&client_secret), "never printed");
    let consented = provisioner.complete_consent(&slack, &allowed).await.expect("installed");
    let install = &slack.installs()[0];
    assert_eq!(consented.external_app_id.as_deref(), Some(slack.apps()[0].id.as_str()));
    assert_eq!(*consented.install.token, install.token);
    assert_eq!(consented.install.team.id, TEAM);
    assert_eq!(consented.install.team.name.as_deref(), Some("Lakeside"));
    assert_eq!(consented.install.identity.external_id, install.bot_user_id);
    assert_eq!(consented.install.identity.handle, "atlas");
    assert_eq!(consented.install.granted_scopes, SLACK_SCOPES);
    assert!(consented.refresh_token.is_none() && consented.expires_at.is_none());
    assert!(!format!("{consented:?}").contains(&install.token), "never printed");

    let call = &slack.calls_to("oauth.v2.access")[0];
    let basic = format!(
        "Basic {}",
        base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("{client_id}:{client_secret}")
        )
    );
    assert_eq!(call.authorization.as_deref(), Some(basic.as_str()));
    assert!(
        !call.raw.contains(&client_secret),
        "the secret is in the header, not the body"
    );
    assert_eq!(call.form["code"], code);
    assert_eq!(call.form["redirect_uri"], REDIRECT);

    // A code works once.
    assert_eq!(
        provisioner.complete_consent(&slack, &allowed).await.map(|_| ()),
        Err(ProvisionError::Refused("token_exchange_failed"))
    );
    assert_eq!(slack.installs().len(), 1);
}

#[tokio::test]
async fn an_exchange_with_the_wrong_secret_or_another_apps_code_is_refused() {
    let (slack, provisioner) = linked();
    let (client_id, client_secret, code) = made_and_allowed(&slack, &provisioner, TEAM).await;
    assert_eq!(
        provisioner
            .complete_consent(&slack, &exchange(&client_id, "not-the-secret", &code))
            .await
            .map(|_| ()),
        Err(ProvisionError::Refused("token_exchange_failed"))
    );
    let (_, _, other_code) = made_and_allowed(&slack, &provisioner, TEAM).await;
    assert_eq!(
        provisioner
            .complete_consent(&slack, &exchange(&client_id, &client_secret, &other_code))
            .await
            .map(|_| ()),
        Err(ProvisionError::Refused("token_exchange_failed"))
    );
    assert!(slack.installs().is_empty());
    slack.fail_next("oauth.v2.access", Fault::Status(503));
    assert_eq!(
        provisioner
            .complete_consent(&slack, &exchange(&client_id, &client_secret, &code))
            .await
            .map(|_| ()),
        Err(ProvisionError::Unavailable)
    );
}

#[tokio::test]
async fn an_install_whose_identity_cant_be_read_is_revoked_not_kept() {
    let (slack, provisioner) = linked();
    let (client_id, client_secret, code) = made_and_allowed(&slack, &provisioner, TEAM).await;
    slack.fail_next("auth.test", Fault::Status(500));
    assert_eq!(
        provisioner
            .complete_consent(&slack, &exchange(&client_id, &client_secret, &code))
            .await
            .map(|_| ()),
        Err(ProvisionError::Unavailable)
    );
    assert!(slack.installs()[0].revoked);
}

#[tokio::test]
async fn revoking_an_install_is_done_once_and_after() {
    let (slack, provisioner) = linked();
    let (client_id, client_secret, code) = made_and_allowed(&slack, &provisioner, TEAM).await;
    let consented = provisioner
        .complete_consent(&slack, &exchange(&client_id, &client_secret, &code))
        .await
        .expect("installed");
    provisioner
        .revoke_install(&slack, &consented.install.token)
        .await
        .expect("revoked");
    assert!(slack.installs()[0].revoked);
    assert_eq!(
        slack.calls_to("auth.revoke")[0].authorization,
        Some(format!("Bearer {}", *consented.install.token))
    );
    // Revoked already: done.
    assert_eq!(
        provisioner.revoke_install(&slack, &consented.install.token).await,
        Ok(())
    );
    slack.fail_next("auth.revoke", Fault::Status(503));
    assert_eq!(
        provisioner.revoke_install(&slack, &consented.install.token).await,
        Err(ProvisionError::Unavailable)
    );
}

// --- Signatures -------------------------------------------------------------

const SIGNING_SECRET: &[u8] = b"8f742231b10e8888abcd99yyyzzz85a5";
const MENTION: &[u8] = include_bytes!("../fixtures/events/app_mention.json");

fn request<'a>(timestamp: Option<&'a str>, signature: Option<&'a str>, body: &'a [u8]) -> InboundRequest<'a> {
    InboundRequest {
        timestamp,
        signature,
        body,
    }
}

fn mention_time() -> Timestamp {
    Timestamp::from_millis(1_760_000_100_000)
}

#[test]
fn slacks_documented_example_signature_is_reproduced() {
    // Slack's worked example ("Verifying requests from Slack").
    let body = b"token=xyzz0WbapA4vBCDEFasx0q6G&team_id=T1DC2JH3J&team_domain=testteamnow&channel_id=G8PSS9T3V&channel_name=foobar&user_id=U2CERLKJA&user_name=roadrunner&command=%2Fwebhook-collect&text=&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT1DC2JH3J%2F397700885554%2F96rGlfmibIGlgcZRskXaIFfN&trigger_id=398738663015.47445629121.803a0bc887a14d10d2c447fce8b6703c";
    assert_eq!(
        sign(SIGNING_SECRET, "1531420618", body),
        "v0=a2114d57b48eac39b9ad189dd8316235a7b4a8d21a10bd27519666489c69b503"
    );
}

#[test]
fn a_delivery_signed_with_the_apps_secret_is_accepted() {
    let provider = SlackChannelProvider::default();
    let signature = sign(SIGNING_SECRET, "1760000100", MENTION);
    assert!(signature.starts_with("v0=") && signature.len() == 67);
    assert_eq!(
        provider.verify(
            &request(Some("1760000100"), Some(&signature), MENTION),
            SIGNING_SECRET,
            mention_time()
        ),
        Ok(())
    );
    // Within five minutes either way.
    for at in ["1759999801", "1760000399"] {
        let signature = sign(SIGNING_SECRET, at, MENTION);
        assert_eq!(
            provider.verify(
                &request(Some(at), Some(&signature), MENTION),
                SIGNING_SECRET,
                mention_time()
            ),
            Ok(()),
            "{at}"
        );
    }
    // Hex in capitals is the same bytes.
    let upper = format!("v0={}", signature[3..].to_uppercase());
    assert_eq!(
        provider.verify(
            &request(Some("1760000100"), Some(&upper), MENTION),
            SIGNING_SECRET,
            mention_time()
        ),
        Ok(())
    );
}

#[test]
fn a_delivery_not_signed_by_the_app_or_too_old_is_refused() {
    let provider = SlackChannelProvider::default();
    let good = sign(SIGNING_SECRET, "1760000100", MENTION);
    let other_app = sign(b"another-apps-signing-secret", "1760000100", MENTION);
    let mut tampered = MENTION.to_vec();
    tampered.push(b' ');
    let cases = [
        (
            request(Some("1760000100"), Some(&other_app), MENTION),
            InboundRefusal::InvalidSignature,
        ),
        (
            request(Some("1760000100"), Some(&good), &tampered),
            InboundRefusal::InvalidSignature,
        ),
        (
            request(Some("1760000101"), Some(&good), MENTION),
            InboundRefusal::InvalidSignature,
        ),
        (
            request(Some("1760000100"), None, MENTION),
            InboundRefusal::InvalidSignature,
        ),
        (
            request(Some("1760000100"), Some("v1=abc"), MENTION),
            InboundRefusal::InvalidSignature,
        ),
        (
            request(Some("1760000100"), Some(&good[..60]), MENTION),
            InboundRefusal::InvalidSignature,
        ),
        // Stale before the signature is looked at.
        (request(Some("1759999799"), Some(&good), MENTION), InboundRefusal::Stale),
        (request(Some("1760000401"), Some(&good), MENTION), InboundRefusal::Stale),
        (request(None, Some(&good), MENTION), InboundRefusal::Stale),
        (request(Some("soon"), Some(&good), MENTION), InboundRefusal::Stale),
    ];
    for (request, expected) in cases {
        assert_eq!(
            provider.verify(&request, SIGNING_SECRET, mention_time()),
            Err(expected),
            "{:?} {:?}",
            request.timestamp,
            request.signature
        );
    }
}

// --- Reading events ---------------------------------------------------------

fn read(fixture: &str) -> InboundDelivery {
    let body = std::fs::read(format!("{}/fixtures/events/{fixture}.json", env!("CARGO_MANIFEST_DIR")))
        .expect("the fixture exists");
    SlackChannelProvider::default()
        .read(&body, mention_time())
        .expect("read")
}

fn at(ts: &str) -> Timestamp {
    ts_time(ts).expect("a ts")
}

fn delivered(event_id: &str, kind: &str, event: InboundEvent) -> InboundDelivery {
    InboundDelivery::Event {
        event_id: event_id.to_owned(),
        kind: kind.to_owned(),
        team_id: Some(TEAM.to_owned()),
        event,
    }
}

fn message(fixture: &str) -> NormalizedMessage {
    match read(fixture) {
        InboundDelivery::Event {
            event: InboundEvent::Message(message),
            team_id,
            ..
        } if team_id.as_deref() == Some(TEAM) => *message,
        other => panic!("{fixture} is a message in the team, not {other:?}"),
    }
}

#[test]
fn slacks_check_of_the_endpoint_is_answered_with_its_challenge() {
    assert_eq!(
        read("url_verification"),
        InboundDelivery::UrlVerification {
            challenge: "3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P".to_owned()
        }
    );
}

/// A mention arrives as `app_mention` and as the `message` it is: only the
/// message is read, so one mention wakes the agent once, and a message that
/// doesn't name its conversation's kind is recorded and nothing more,
/// rather than guessed.
#[test]
fn a_mention_is_read_once_from_its_message() {
    assert_eq!(
        read("app_mention"),
        delivered("Ev0MENTION01", "app_mention", InboundEvent::Other)
    );
    let mut unnamed: Value = serde_json::from_str(
        &std::fs::read_to_string(format!(
            "{}/fixtures/events/message_im.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("the fixture"),
    )
    .expect("JSON");
    unnamed["event"]
        .as_object_mut()
        .expect("an event")
        .remove("channel_type");
    assert_eq!(
        SlackChannelProvider::default().read(unnamed.to_string().as_bytes(), mention_time()),
        Ok(delivered("Ev0DIRECT001", "message", InboundEvent::Other))
    );
    assert_eq!(
        read("message_mention"),
        delivered(
            "Ev0MENTIONMSG",
            "message",
            InboundEvent::Message(Box::new(NormalizedMessage {
                conversation_external_id: "C0GENERAL".to_owned(),
                conversation_kind: ConversationKind::Channel,
                conversation_is_external: false,
                author_external_id: Some("U0ALICE01".to_owned()),
                author_bot_id: None,
                author_team_id: Some(TEAM.to_owned()),
                author_is_automated: false,
                external_id: "1760000100.000200".to_owned(),
                thread_key: "1760000100.000200".to_owned(),
                text: "<@UBOT00001> can you summarize <#C0GENERAL|general> for <@U0BOB0002>?".to_owned(),
                mentions: vec!["UBOT00001".to_owned(), "U0BOB0002".to_owned()],
                sent_at: mention_time(),
            }))
        )
    );
}

/// Slack Connect: a person of another organization's team, in a channel
/// shared with it, is read with their own team; the delivery's team is
/// still the one the app is installed in.
#[test]
fn a_shared_channels_guest_is_read_with_their_own_team() {
    let guest = message("message_shared");
    assert!(guest.conversation_is_external);
    assert_eq!(guest.author_external_id.as_deref(), Some("U0GUEST01"));
    assert_eq!(guest.author_team_id.as_deref(), Some("T0OTHERORG"));
    assert!(!guest.author_is_automated);
    assert_eq!(guest.mentions, ["UBOT00001"]);
    // Without `user_team`, the event's `team` is the author's.
    let mut body: Value = serde_json::from_str(
        &std::fs::read_to_string(format!(
            "{}/fixtures/events/message_shared.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("the fixture"),
    )
    .expect("JSON");
    let event = body["event"].as_object_mut().expect("an event");
    event.remove("user_team");
    event.insert("team".to_owned(), json!("T0THIRDORG"));
    match SlackChannelProvider::default().read(body.to_string().as_bytes(), mention_time()) {
        Ok(InboundDelivery::Event {
            team_id,
            event: InboundEvent::Message(message),
            ..
        }) => {
            assert_eq!(team_id.as_deref(), Some(TEAM));
            assert_eq!(message.author_team_id.as_deref(), Some("T0THIRDORG"));
        }
        other => panic!("a message, not {other:?}"),
    }
}

#[test]
fn messages_are_read_with_their_conversation_kind_and_thread() {
    let reply = message("message_channel_thread_reply");
    assert_eq!(reply.thread_key, "1760000100.000200", "the thread it replies in");
    assert_eq!(reply.external_id, "1760000160.000300");
    assert_eq!(reply.mentions, ["UBOT00001", "U0ALICE01"]);
    assert_eq!(reply.sent_at, at("1760000160.000300"));

    let kinds = [
        ("message_im", "D0ALICEDM", ConversationKind::Dm, ""),
        (
            "message_group",
            "C0PRIVATE",
            ConversationKind::PrivateChannel,
            "1760000300.000100",
        ),
        (
            "message_mpim",
            "G0GROUPDM",
            ConversationKind::GroupDm,
            "1760000400.000100",
        ),
    ];
    for (fixture, conversation, kind, thread) in kinds {
        let read = message(fixture);
        assert_eq!(read.conversation_external_id, conversation, "{fixture}");
        assert_eq!(read.conversation_kind, kind, "{fixture}");
        // A DM's top level is one conversation.
        assert_eq!(read.thread_key, thread, "{fixture}");
        assert!(!read.author_is_automated, "{fixture}");
    }
}

#[test]
fn the_apps_own_messages_and_other_apps_are_marked_automated() {
    let own = message("message_own");
    // Told apart from people by the bot user's ID or its bot ID, which the
    // store knows.
    assert_eq!(own.author_external_id.as_deref(), Some("UBOT00001"));
    assert_eq!(own.author_bot_id.as_deref(), Some("B0FAKE001"));
    assert!(own.author_is_automated);
    let webhook = message("message_other_app");
    assert_eq!(webhook.author_external_id.as_deref(), Some("B0WEBHOOK"));
    assert_eq!(webhook.author_bot_id.as_deref(), Some("B0WEBHOOK"));
    assert!(webhook.author_is_automated);
    // Any post with a bot ID is an app's, whether or not it names a user
    // or a subtype.
    let mut body: Value = serde_json::from_str(
        &std::fs::read_to_string(format!(
            "{}/fixtures/events/message_mention.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("the fixture"),
    )
    .expect("JSON");
    body["event"]["bot_id"] = json!("B0INTEGRATION");
    match SlackChannelProvider::default().read(body.to_string().as_bytes(), mention_time()) {
        Ok(InboundDelivery::Event {
            event: InboundEvent::Message(message),
            ..
        }) => {
            assert_eq!(message.author_external_id.as_deref(), Some("U0ALICE01"));
            assert!(message.author_is_automated);
        }
        other => panic!("a message, not {other:?}"),
    }
}

/// An edit by its author and a deletion are read as such, by the message
/// they change; a change nobody made to the text (an unfurl, a reply
/// counted) is nothing.
#[test]
fn edits_and_deletions_are_read_and_other_changes_ignored() {
    assert_eq!(
        read("message_changed"),
        delivered(
            "Ev0EDITED001",
            "message",
            InboundEvent::Edited(Box::new(EditedMessage {
                conversation_external_id: "C0GENERAL".to_owned(),
                external_id: "1760000160.000300".to_owned(),
                text: "Thanks <@UBOT00001>, and <@U0ALICE01> too".to_owned(),
                edited_at: at("1760000220.000000"),
            }))
        )
    );
    assert_eq!(
        read("message_deleted"),
        delivered(
            "Ev0DELETED01",
            "message",
            InboundEvent::Deleted(Box::new(DeletedMessage {
                conversation_external_id: "C0GENERAL".to_owned(),
                external_id: "1760000160.000300".to_owned(),
                deleted_at: at("1760000700.000100"),
            }))
        )
    );
    assert_eq!(
        read("message_unfurled"),
        delivered("Ev0UNFURL001", "message", InboundEvent::Other)
    );
}

#[test]
fn removal_is_read_and_everything_else_ignored() {
    assert_eq!(
        read("app_uninstalled"),
        delivered("Ev0UNINSTALL", "app_uninstalled", InboundEvent::Uninstalled)
    );
    assert_eq!(
        read("tokens_revoked"),
        delivered("Ev0REVOKED01", "tokens_revoked", InboundEvent::Uninstalled)
    );
    assert_eq!(
        read("channel_join"),
        delivered("Ev0JOINED001", "message", InboundEvent::Other)
    );
    // Not an event callback, and no event ID: nothing to record.
    assert_eq!(read("app_rate_limited"), InboundDelivery::Ignored);
}

#[test]
fn what_slack_doesnt_send_is_unreadable() {
    let provider = SlackChannelProvider::default();
    assert_eq!(
        provider.read(b"not json", mention_time()),
        Err(InboundRefusal::InvalidJson)
    );
    for body in [&b"[]"[..], br#"{"no":"type"}"#] {
        assert_eq!(
            provider.read(body, mention_time()),
            Err(InboundRefusal::Unreadable),
            "{}",
            String::from_utf8_lossy(body)
        );
    }
    // No event ID: nothing to record it by.
    assert_eq!(
        provider.read(
            br#"{"type":"event_callback","team_id":"T1","event":{"type":"message","channel":"C1","ts":"1.1"}}"#,
            mention_time()
        ),
        Ok(InboundDelivery::Ignored)
    );
    // No team: recorded, and never read as anyone's.
    assert!(matches!(
        provider.read(
            br#"{"type":"event_callback","event_id":"Ev1","event":{"type":"message","channel":"C1","ts":"1.1","user":"U1","channel_type":"channel"}}"#,
            mention_time()
        ),
        Ok(InboundDelivery::Event { team_id: None, .. })
    ));
    // A `ts` that isn't a time is sent now; a field longer than Slack
    // sends leaves the message unread.
    match provider.read(
        br#"{"type":"event_callback","team_id":"T1","event_id":"Ev1","event":{"type":"message","channel":"C1","ts":"soon","user":"U1","channel_type":"channel"}}"#,
        mention_time(),
    ) {
        Ok(InboundDelivery::Event {
            event: InboundEvent::Message(message),
            ..
        }) => assert_eq!(message.sent_at, mention_time()),
        other => panic!("a message, not {other:?}"),
    }
    let long = format!(
        r#"{{"type":"event_callback","team_id":"T1","event_id":"Ev1","event":{{"type":"message","channel":"C1","ts":"1.1","user":"{}","channel_type":"channel"}}}}"#,
        "U".repeat(300)
    );
    assert!(matches!(
        provider.read(long.as_bytes(), mention_time()),
        Ok(InboundDelivery::Event {
            event: InboundEvent::Other,
            ..
        })
    ));
}

#[test]
fn a_ts_is_its_seconds_and_microseconds() {
    assert_eq!(ts_time("1760000100.000200"), Some(mention_time()));
    assert_eq!(
        ts_time("1760000100.123456"),
        Some(Timestamp::from_millis(1_760_000_100_123))
    );
    assert_eq!(ts_time("1760000100.5"), Some(Timestamp::from_millis(1_760_000_100_500)));
    assert_eq!(ts_time("1760000100"), Some(mention_time()));
    assert_eq!(ts_time("1760000100.1234567"), None);
    assert_eq!(ts_time("x.1"), None);
}

// --- Sending ------------------------------------------------------------------

/// An installed app in the fake: its bot token.
async fn installed(slack: &FakeSlack) -> String {
    let provisioner = SlackProvisioner::new(FakeSlack::api());
    slack.issue_configuration_token(CONFIGURATION, REFRESH, TEAM);
    let (client_id, client_secret, code) = made_and_allowed(slack, &provisioner, TEAM).await;
    let consented = provisioner
        .complete_consent(slack, &exchange(&client_id, &client_secret, &code))
        .await
        .expect("installed");
    consented.install.token.to_string()
}

fn post<'a>(text: &'a str, invocation_id: Option<&'a str>) -> Post<'a> {
    Post {
        channel: "C0GENERAL",
        thread_ts: Some("1760000100.000200"),
        text,
        invocation_id,
        username: None,
        icon_url: None,
    }
}

#[tokio::test]
async fn a_send_is_one_markdown_block_carrying_its_invocation() {
    let slack = FakeSlack::new();
    let token = installed(&slack).await;
    let client = SlackClient::new(FakeSlack::api());
    let text = "**Done.** See the [plan](https://docs.example/plan).";
    let posted = client
        .post_message(&slack, &token, &post(text, Some("inv_0001")))
        .await
        .expect("posted");
    assert_eq!(posted.channel, "C0GENERAL");
    let call = &slack.calls_to("chat.postMessage")[0];
    assert_eq!(call.authorization, Some(format!("Bearer {token}")));
    assert_eq!(call.form["channel"], "C0GENERAL");
    assert_eq!(call.form["thread_ts"], "1760000100.000200");
    assert_eq!(call.form["text"], text, "a notification's fallback");
    let blocks: Value = serde_json::from_str(&call.form["blocks"]).expect("JSON");
    assert_eq!(blocks, json!([{ "type": "markdown", "text": text }]));
    let metadata: Value = serde_json::from_str(&call.form["metadata"]).expect("JSON");
    assert_eq!(
        metadata,
        json!({ "event_type": SEND_EVENT_TYPE, "event_payload": { "invocation_id": "inv_0001" } })
    );
    assert_eq!(slack.messages()[0].ts, posted.ts);

    client
        .update_message(&slack, &token, &posted.channel, &posted.ts, "**Done**, revised.")
        .await
        .expect("edited");
    let call = &slack.calls_to("chat.update")[0];
    assert_eq!(call.form["ts"], posted.ts);
    assert!(call.form.contains_key("blocks"));
    client
        .react(&slack, &token, &posted.channel, &posted.ts, ":white_check_mark:", false)
        .await
        .expect("reacted");
    assert_eq!(slack.calls_to("reactions.add")[0].form["name"], "white_check_mark");
    // Twice is done; so is removing what isn't there.
    client
        .react(&slack, &token, &posted.channel, &posted.ts, "white_check_mark", false)
        .await
        .expect("already there");
    client
        .react(&slack, &token, &posted.channel, &posted.ts, "eyes", true)
        .await
        .expect("not there");
    client
        .delete_message(&slack, &token, &posted.channel, &posted.ts)
        .await
        .expect("deleted");
    client
        .delete_message(&slack, &token, &posted.channel, &posted.ts)
        .await
        .expect("deleted already");
}

#[tokio::test]
async fn what_cant_be_sent_is_refused_before_slack_is_called() {
    let slack = FakeSlack::new();
    let token = installed(&slack).await;
    let client = SlackClient::new(FakeSlack::api());
    let long = "a".repeat(MARKDOWN_MAX + 1);
    assert_eq!(
        client.post_message(&slack, &token, &post(&long, None)).await,
        Err(SlackError::Refused(SlackCode::TooLong))
    );
    assert_eq!(
        client.post_message(&slack, &token, &post("  ", None)).await,
        Err(SlackError::Refused(SlackCode::NoText))
    );
    assert!(slack.calls_to("chat.postMessage").is_empty());
}

#[tokio::test]
async fn sends_fail_with_stable_codes_and_slacks_wait() {
    let slack = FakeSlack::new();
    let token = installed(&slack).await;
    let client = SlackClient::new(FakeSlack::api());
    slack.fail_next("chat.postMessage", Fault::RateLimited { secs: 3 });
    let limited = client.post_message(&slack, &token, &post("hi", None)).await;
    assert_eq!(
        limited,
        Err(SlackError::RateLimited {
            retry_after: Duration::from_secs(3)
        })
    );
    assert!(limited.is_err_and(|error| !error.outcome_unknown()), "nothing was sent");

    let mut missing = post("hi", None);
    missing.channel = "X0GONE";
    let refused = client.post_message(&slack, &token, &missing).await;
    assert_eq!(refused, Err(SlackError::Refused(SlackCode::ChannelNotFound)));
    assert!(refused.is_err_and(|error| error.code() == "channel_not_found"));

    let revoked = client.post_message(&slack, "xoxb-revoked", &post("hi", None)).await;
    assert!(revoked.is_err_and(|error| error.token_refused()));

    slack.fail_next("chat.postMessage", Fault::Error("some_<script>_thing".to_owned()));
    let other = client.post_message(&slack, &token, &post("hi", None)).await;
    assert!(
        other.is_err_and(|error| error.code() == "provider_refused" && error.to_string() == "provider_refused"),
        "Slack's words aren't passed on"
    );
    slack.fail_next("chat.postMessage", Fault::Status(503));
    let unavailable = client.post_message(&slack, &token, &post("hi", None)).await;
    assert!(unavailable.is_err_and(|error| error.outcome_unknown()));
}

#[tokio::test]
async fn a_send_whose_answer_was_lost_is_found_by_its_invocation() {
    let slack = FakeSlack::new();
    let token = installed(&slack).await;
    let client = SlackClient::new(FakeSlack::api());
    let attempted_at = Timestamp::from_millis(ISSUED_AT * 1000);
    let thread = Some("1760000100.000200");
    // Something else in the thread first.
    client
        .post_message(&slack, &token, &post("unrelated", Some("inv_other")))
        .await
        .expect("posted");
    slack.fail_next("chat.postMessage", Fault::LoseAnswer);
    let lost = client
        .post_message(&slack, &token, &post("the answer", Some("inv_lost")))
        .await;
    assert!(lost.is_err_and(|error| error.outcome_unknown()));
    let found = client
        .find_sent(&slack, &token, "C0GENERAL", thread, "inv_lost", attempted_at)
        .await
        .expect("read");
    let sent = slack
        .messages()
        .into_iter()
        .find(|message| message.text == "the answer")
        .expect("Slack holds it");
    assert_eq!(found, Some(sent.ts));
    let call = slack.calls_to("conversations.replies").pop().expect("read the thread");
    assert_eq!(call.form["include_all_metadata"], "true");
    assert_eq!(call.form["ts"], "1760000100.000200");

    // One that never arrived isn't found: send it again.
    slack.fail_next("chat.postMessage", Fault::TimedOut);
    let never = client
        .post_message(&slack, &token, &post("never", Some("inv_never")))
        .await;
    assert!(never.is_err_and(|error| error.outcome_unknown()));
    assert_eq!(
        client
            .find_sent(&slack, &token, "C0GENERAL", thread, "inv_never", attempted_at)
            .await,
        Ok(None)
    );
    // At the top level, from history.
    let mut top = post("top", Some("inv_top"));
    top.thread_ts = None;
    slack.fail_next("chat.postMessage", Fault::LoseAnswer);
    assert!(client.post_message(&slack, &token, &top).await.is_err());
    let found = client
        .find_sent(&slack, &token, "C0GENERAL", None, "inv_top", attempted_at)
        .await
        .expect("read");
    assert!(found.is_some());
    assert_eq!(slack.calls_to("conversations.history").len(), 1);
    // A read that can't be made leaves it unknown.
    slack.fail_next("conversations.replies", Fault::Status(500));
    assert!(
        client
            .find_sent(&slack, &token, "C0GENERAL", thread, "inv_lost", attempted_at)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn people_conversations_and_status_are_read_with_the_bot_token() {
    let slack = FakeSlack::new();
    let token = installed(&slack).await;
    let client = SlackClient::new(FakeSlack::api());
    slack.add_user(json!({
        "id": "U0ALICE01", "name": "alice", "deleted": false, "is_bot": false, "tz": "Europe/Paris",
        "real_name": "Alice Moreau",
        "profile": { "display_name": "", "real_name": "Alice Moreau", "email": "alice@team.example", "title": "PM" },
    }));
    let person = client.user_info(&slack, &token, "U0ALICE01").await.expect("found");
    assert_eq!(person.id, "U0ALICE01");
    assert_eq!(person.handle.as_deref(), Some("alice"));
    assert_eq!(
        person.display_name.as_deref(),
        Some("Alice Moreau"),
        "the real name without a display name"
    );
    assert_eq!(person.email.as_deref(), Some("alice@team.example"));
    assert!(!person.is_automated);
    assert_eq!(
        client.user_info(&slack, &token, "U0NOBODY").await,
        Err(SlackError::Refused(SlackCode::UserNotFound))
    );
    assert_eq!(
        client.open_dm(&slack, &token, "U0ALICE01").await,
        Ok("DU0ALICE01".to_owned())
    );
    let conversation = client
        .conversation_info(&slack, &token, "C0GENERAL")
        .await
        .expect("found");
    assert_eq!(conversation.kind, ConversationKind::Channel);
    assert_eq!(conversation.name.as_deref(), Some("general"));
    assert!(
        client
            .set_status(&slack, &token, "D0ALICEDM", "1760000200.000100", "is thinking…")
            .await
    );
    let call = &slack.calls_to("assistant.threads.setStatus")[0];
    assert_eq!(call.form["channel_id"], "D0ALICEDM");
    assert_eq!(call.form["thread_ts"], "1760000200.000100");
    // Best effort: a refusal (no assistant view, a missing scope) or no answer is only not shown.
    for fault in [
        Fault::Error("missing_scope".to_owned()),
        Fault::Status(500),
        Fault::TimedOut,
    ] {
        slack.fail_next("assistant.threads.setStatus", fault);
        assert!(
            !client
                .set_status(&slack, &token, "D0ALICEDM", "1760000200.000100", "is thinking…")
                .await
        );
    }
}

/// What the inbound and reply jobs ask of the provider: people by their
/// user ID, and a run's answer posted in its thread, found again by the
/// run's ID when its answer was lost.
#[tokio::test]
async fn the_provider_reads_people_and_posts_answers_with_the_bot_token() {
    let slack = FakeSlack::new();
    let token = installed(&slack).await;
    let provider = SlackChannelProvider::new(FakeSlack::api());
    slack.add_user(json!({
        "id": "U0ALICE01", "name": "alice", "deleted": false, "is_bot": false,
        "profile": { "display_name": "Alice", "real_name": "Alice Moreau", "email": "alice@team.example" },
    }));
    assert_eq!(
        provider.person(&slack, &token, "U0ALICE01").await,
        Ok(Some(ProviderPerson {
            handle: Some("alice".to_owned()),
            display_name: Some("Alice".to_owned()),
            email: Some("alice@team.example".to_owned()),
            is_automated: false,
        }))
    );
    // No such user, or a bot ID: no one, and a bot ID isn't asked about.
    assert_eq!(provider.person(&slack, &token, "U0NOBODY").await, Ok(None));
    let asked = slack.calls_to("users.info").len();
    assert_eq!(provider.person(&slack, &token, "B0WEBHOOK").await, Ok(None));
    assert_eq!(slack.calls_to("users.info").len(), asked);
    slack.fail_next("users.info", Fault::RateLimited { secs: 7 });
    assert_eq!(
        provider.person(&slack, &token, "U0ALICE01").await,
        Err(ProviderCallError::RateLimited { retry_after_secs: 7 })
    );
    slack.fail_next("users.info", Fault::Status(503));
    assert_eq!(
        provider.person(&slack, &token, "U0ALICE01").await,
        Err(ProviderCallError::Unavailable)
    );

    let answer = OutgoingPost {
        conversation_external_id: "C0GENERAL",
        thread_key: "1760000100.000200",
        text: "Here's the summary.",
        send_id: "run_0123",
    };
    let ts = provider.post(&slack, &token, &answer).await.expect("posted");
    let posted = slack.messages().pop().expect("Slack holds it");
    assert_eq!(posted.ts, ts);
    assert_eq!(posted.thread_ts.as_deref(), Some("1760000100.000200"));
    assert_eq!(posted.text, "Here's the summary.");
    assert_eq!(
        posted.metadata.expect("metadata")["event_payload"]["invocation_id"],
        "run_0123"
    );
    // A DM's top level is posted at the top level.
    let top = OutgoingPost {
        conversation_external_id: "D0ALICEDM",
        thread_key: "",
        ..answer
    };
    provider.post(&slack, &token, &top).await.expect("posted");
    assert_eq!(slack.messages().pop().expect("held").thread_ts, None);
    // Too long for one block: cut, and marked.
    let long = "a".repeat(MARKDOWN_MAX + 10);
    let cut = OutgoingPost { text: &long, ..answer };
    provider.post(&slack, &token, &cut).await.expect("posted");
    let held = slack.messages().pop().expect("held").text;
    assert_eq!(held.chars().count(), MARKDOWN_MAX);
    assert!(held.ends_with('…'));

    // Lost on the way back: unknown, then found by the run's ID.
    let lost = OutgoingPost {
        send_id: "run_lost",
        ..answer
    };
    slack.fail_next("chat.postMessage", Fault::LoseAnswer);
    assert_eq!(
        provider.post(&slack, &token, &lost).await,
        Err(ProviderCallError::OutcomeUnknown)
    );
    let attempted_at = Timestamp::from_millis(ISSUED_AT * 1000);
    let found = provider
        .find_sent(&slack, &token, &lost, attempted_at)
        .await
        .expect("read");
    assert!(found.is_some());
    let never = OutgoingPost {
        send_id: "run_never",
        ..answer
    };
    assert_eq!(provider.find_sent(&slack, &token, &never, attempted_at).await, Ok(None));
    // A revoked token is said so; Slack's own words never are.
    slack.fail_next("chat.postMessage", Fault::Error("token_revoked".to_owned()));
    assert_eq!(
        provider.post(&slack, &token, &answer).await,
        Err(ProviderCallError::TokenRefused)
    );
    slack.fail_next("chat.postMessage", Fault::Error("not_in_channel".to_owned()));
    assert_eq!(
        provider.post(&slack, &token, &answer).await,
        Err(ProviderCallError::Refused("not_in_channel"))
    );
    // Slack failing on its side, in its answer: a post may have happened,
    // and a read is tried again; neither is a refusal, nor someone unknown.
    for transient in [
        "internal_error",
        "fatal_error",
        "service_unavailable",
        "request_timeout",
    ] {
        slack.fail_next("chat.postMessage", Fault::Error(transient.to_owned()));
        assert_eq!(
            provider.post(&slack, &token, &answer).await,
            Err(ProviderCallError::OutcomeUnknown),
            "{transient}"
        );
        slack.fail_next("conversations.replies", Fault::Error(transient.to_owned()));
        assert_eq!(
            provider.find_sent(&slack, &token, &lost, attempted_at).await,
            Err(ProviderCallError::Unavailable),
            "{transient}"
        );
        slack.fail_next("users.info", Fault::Error(transient.to_owned()));
        assert_eq!(
            provider.person(&slack, &token, "U0ALICE01").await,
            Err(ProviderCallError::Unavailable),
            "{transient}"
        );
    }
    slack.fail_next("users.info", Fault::Error("missing_scope".to_owned()));
    assert_eq!(
        provider.person(&slack, &token, "U0ALICE01").await,
        Err(ProviderCallError::Refused("missing_scope")),
        "a refusal says nothing of whether they exist"
    );
}

#[test]
fn the_fake_is_reached_at_its_own_address() {
    assert_eq!(
        FakeSlack::api().method_url("auth.test"),
        format!("{FAKE_API_URL}/auth.test")
    );
}

/// What a run's channel tools ask of the provider, with the connection's
/// bot token: an edit, a reaction and a deletion of a message by its
/// conversation and `ts`, a direct conversation opened with one user, and
/// the thread's status while the agent works.
#[tokio::test]
async fn the_provider_edits_reacts_deletes_opens_dms_and_shows_work() {
    let slack = FakeSlack::new();
    let token = installed(&slack).await;
    let provider = SlackChannelProvider::new(FakeSlack::api());
    let posted = provider
        .post(
            &slack,
            &token,
            &OutgoingPost {
                conversation_external_id: "C0GENERAL",
                thread_key: "1760000100.000200",
                text: "On it.",
                send_id: "call_0001",
            },
        )
        .await
        .expect("posted");
    let message = MessageRef {
        conversation_external_id: "C0GENERAL",
        external_id: &posted,
    };
    provider.edit(&slack, &token, &message, "Done.").await.expect("edited");
    assert_eq!(slack.calls_to("chat.update")[0].form["ts"], posted);
    provider
        .react(&slack, &token, &message, "eyes", false)
        .await
        .expect("reacted");
    assert_eq!(slack.calls_to("reactions.add")[0].form["timestamp"], posted);
    provider.delete(&slack, &token, &message).await.expect("deleted");
    provider
        .delete(&slack, &token, &message)
        .await
        .expect("deleted already");
    slack.add_user(json!({ "id": "U0ALICE01", "name": "alice", "deleted": false, "is_bot": false }));
    assert_eq!(
        provider.open_direct(&slack, &token, "U0ALICE01").await,
        Ok("DU0ALICE01".to_owned())
    );
    // A bot ID names no one to message.
    assert_eq!(
        provider.open_direct(&slack, &token, "B0WEBHOOK").await,
        Err(ProviderCallError::Refused("user_not_found"))
    );
    assert!(
        provider
            .show_working(&slack, &token, "C0GENERAL", "1760000100.000200", true)
            .await
    );
    assert!(
        provider
            .show_working(&slack, &token, "C0GENERAL", "1760000100.000200", false)
            .await
    );
    let cleared = slack.calls_to("assistant.threads.setStatus").pop().expect("cleared");
    assert_eq!(cleared.form["status"], "");
    // A DM's top level has no thread to show it in; a failure is only not shown.
    assert!(!provider.show_working(&slack, &token, "D0ALICEDM", "", true).await);
    slack.fail_next("assistant.threads.setStatus", Fault::Status(500));
    assert!(
        !provider
            .show_working(&slack, &token, "C0GENERAL", "1760000100.000200", true)
            .await
    );
    slack.fail_next("chat.update", Fault::Error("cant_update_message".to_owned()));
    assert_eq!(
        provider.edit(&slack, &token, &message, "Again.").await,
        Err(ProviderCallError::Refused("cant_update_message"))
    );
}
