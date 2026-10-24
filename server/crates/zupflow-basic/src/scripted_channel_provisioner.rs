//! [`ScriptedChannelProvisioner`]: the basic [`AppProvisioner`], for tests,
//! local runs and the contract's reference server. It calls no provider: it
//! answers by fixed rules, as the Console's mock API's fake provider does.
//!
//! - Tokens containing [`REFUSED_TOKEN_MARKER`], in any case, are refused,
//!   as a provider refuses an expired or revoked configuration token. Any
//!   other pair is accepted, for [`zupflow_core::channels::TOKEN_LIFETIME_MS`].
//! - An app is made at once, with an `A0…` ID of its own and fresh random
//!   secrets, and waits for consent. The provisioner keeps a ledger of every
//!   app it made and whether it was deleted ([`ScriptedChannelProvisioner::apps`]),
//!   finds a live one by its events URL, and deletes one once
//!   (`AppNotFound` after).
//! - A test can lose the next `create`'s answer: the app is made, the caller
//!   is told the provider is unavailable
//!   ([`ScriptedChannelProvisioner::lose_next_answer`]); fail the next
//!   `delete` ([`ScriptedChannelProvisioner::fail_next_delete`]); or run
//!   something while the next `create` is with the provider
//!   ([`ScriptedChannelProvisioner::during_next_create`]).
//! - The consent page is on an `.example` domain: nobody can allow anything there.
//!
//! It never sends a request: the [`OutboundHttp`] each call is given goes unused.
//!
//! [`NoChannelProvisioner`] is the provisioner of a deployment that reaches
//! no provider: every call is `NotConfigured`.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;
use zupflow_core::channels::provision::{
    AppManifest, AppProvisioner, AppSecrets, CheckedTokens, ConsentRequest, CreatedApp, InstallStep, ManagerTokens,
    ProvisionError, RotatedTokens,
};
use zupflow_core::channels::{ChannelProviderKind, ManagerMethod, TOKEN_LIFETIME_MS};
use zupflow_core::outbound::OutboundHttp;
use zupflow_core::time::Timestamp;

/// The text that makes the scripted provisioner refuse a token, in any case.
pub const REFUSED_TOKEN_MARKER: &str = "invalid";

/// Where the scripted provisioner's consent page would be.
pub const CONSENT_URL: &str = "https://consent.provider.example/oauth/authorize";

/// One app the scripted provider made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptedApp {
    pub external_app_id: String,
    pub events_url: String,
    pub deleted: bool,
}

/// Something a test runs while a `create` is with the provider.
pub type DuringCreate = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

#[derive(Default)]
struct Ledger {
    apps: Vec<ScriptedApp>,
    lose_answers: u32,
    unavailable_deletes: u32,
    during_create: Option<DuringCreate>,
}

/// Clones share one ledger: a test keeps one to read what the server's made.
#[derive(Clone, Default)]
pub struct ScriptedChannelProvisioner {
    ledger: Arc<Mutex<Ledger>>,
}

impl std::fmt::Debug for ScriptedChannelProvisioner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScriptedChannelProvisioner")
            .field("apps", &self.ledger().apps)
            .finish_non_exhaustive()
    }
}

impl ScriptedChannelProvisioner {
    fn ledger(&self) -> MutexGuard<'_, Ledger> {
        self.ledger.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every app made, in order, with whether it's been deleted since.
    pub fn apps(&self) -> Vec<ScriptedApp> {
        self.ledger().apps.clone()
    }

    /// The next `create` makes its app but answers `Unavailable`, as a
    /// provider whose answer never arrived.
    pub fn lose_next_answer(&self) {
        self.ledger().lose_answers += 1;
    }

    /// The next `delete` answers `Unavailable`, deleting nothing.
    pub fn fail_next_delete(&self) {
        self.ledger().unavailable_deletes += 1;
    }

    /// The next `create` runs `during` before it makes its app.
    pub fn during_next_create(&self, during: DuringCreate) {
        self.ledger().during_create = Some(during);
    }
}

fn refused(tokens: &ManagerTokens) -> bool {
    [&tokens.configuration_token, &tokens.refresh_token]
        .iter()
        .any(|token| token.to_ascii_lowercase().contains(REFUSED_TOKEN_MARKER))
}

fn checked(tokens: &ManagerTokens) -> Result<(), ProvisionError> {
    if refused(tokens) {
        Err(ProvisionError::TokensRefused)
    } else {
        Ok(())
    }
}

fn expiry() -> Option<Timestamp> {
    Some(Timestamp::from_millis(
        Timestamp::now().millis().saturating_add(TOKEN_LIFETIME_MS),
    ))
}

/// `length` characters of Slack's ID alphabet, derived from `seed`.
fn derived(seed: &str, length: usize) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    Sha256::digest(seed.as_bytes())
        .iter()
        .take(length)
        .map(|byte| char::from(ALPHABET[usize::from(byte % 32)]))
        .collect()
}

fn random_secret() -> Result<Zeroizing<String>, ProvisionError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| ProvisionError::Unavailable)?;
    Ok(Zeroizing::new(bytes.iter().map(|byte| format!("{byte:02x}")).collect()))
}

/// Percent-encodes everything but unreserved characters.
fn encoded(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => char::from(byte).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[async_trait]
impl AppProvisioner for ScriptedChannelProvisioner {
    fn method(&self) -> ManagerMethod {
        ManagerMethod::ConfigurationToken
    }

    async fn check_tokens(
        &self,
        _: &dyn OutboundHttp,
        _: ChannelProviderKind,
        tokens: &ManagerTokens,
    ) -> Result<CheckedTokens, ProvisionError> {
        checked(tokens)?;
        Ok(CheckedTokens { expires_at: expiry() })
    }

    async fn rotate_tokens(
        &self,
        _: &dyn OutboundHttp,
        tokens: &ManagerTokens,
    ) -> Result<RotatedTokens, ProvisionError> {
        checked(tokens)?;
        Ok(RotatedTokens {
            tokens: ManagerTokens {
                configuration_token: Zeroizing::new(format!("xoxe.scripted-{}", random_secret()?.as_str())),
                refresh_token: Zeroizing::new(format!("xoxe-scripted-{}", random_secret()?.as_str())),
            },
            expires_at: expiry(),
        })
    }

    async fn create(
        &self,
        _: &dyn OutboundHttp,
        tokens: &ManagerTokens,
        manifest: &AppManifest,
    ) -> Result<CreatedApp, ProvisionError> {
        checked(tokens)?;
        let during = self.ledger().during_create.take();
        if let Some(during) = during {
            during().await;
        }
        let secrets = AppSecrets {
            client_secret: random_secret()?,
            signing_secret: random_secret()?,
        };
        let mut ledger = self.ledger();
        let seed = format!("{}#{}", manifest.events_url, ledger.apps.len());
        let external_app_id = format!("A0{}", derived(&seed, 9));
        ledger.apps.push(ScriptedApp {
            external_app_id: external_app_id.clone(),
            events_url: manifest.events_url.clone(),
            deleted: false,
        });
        if ledger.lose_answers > 0 {
            ledger.lose_answers -= 1;
            return Err(ProvisionError::Unavailable);
        }
        Ok(CreatedApp {
            external_app_id,
            client_id: format!("{}.{}", 1000 + seed.len(), derived(&format!("{seed}:client"), 12)),
            secrets,
            install: InstallStep::NeedsConsent,
        })
    }

    async fn find(
        &self,
        _: &dyn OutboundHttp,
        tokens: &ManagerTokens,
        events_url: &str,
    ) -> Result<Option<String>, ProvisionError> {
        checked(tokens)?;
        Ok(self
            .ledger()
            .apps
            .iter()
            .rev()
            .find(|app| !app.deleted && app.events_url == events_url)
            .map(|app| app.external_app_id.clone()))
    }

    async fn update(
        &self,
        _: &dyn OutboundHttp,
        tokens: &ManagerTokens,
        external_app_id: &str,
        _: &AppManifest,
    ) -> Result<(), ProvisionError> {
        checked(tokens)?;
        let ledger = self.ledger();
        if ledger
            .apps
            .iter()
            .any(|app| !app.deleted && app.external_app_id == external_app_id)
        {
            Ok(())
        } else {
            Err(ProvisionError::AppNotFound)
        }
    }

    async fn delete(
        &self,
        _: &dyn OutboundHttp,
        tokens: &ManagerTokens,
        external_app_id: &str,
    ) -> Result<(), ProvisionError> {
        checked(tokens)?;
        let mut ledger = self.ledger();
        if ledger.unavailable_deletes > 0 {
            ledger.unavailable_deletes -= 1;
            return Err(ProvisionError::Unavailable);
        }
        let app = ledger
            .apps
            .iter_mut()
            .find(|app| !app.deleted && app.external_app_id == external_app_id)
            .ok_or(ProvisionError::AppNotFound)?;
        app.deleted = true;
        Ok(())
    }

    fn consent_url(&self, request: &ConsentRequest<'_>) -> Result<String, ProvisionError> {
        Ok(format!(
            "{CONSENT_URL}?client_id={}&scope={}&redirect_uri={}&state={}",
            encoded(request.client_id),
            encoded(&request.scopes.join(",")),
            encoded(request.redirect_url),
            encoded(request.state),
        ))
    }
}

/// The provisioner of a deployment that reaches no channel provider.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoChannelProvisioner;

#[async_trait]
impl AppProvisioner for NoChannelProvisioner {
    fn method(&self) -> ManagerMethod {
        ManagerMethod::ConfigurationToken
    }

    async fn check_tokens(
        &self,
        _: &dyn OutboundHttp,
        _: ChannelProviderKind,
        _: &ManagerTokens,
    ) -> Result<CheckedTokens, ProvisionError> {
        Err(ProvisionError::NotConfigured)
    }

    async fn rotate_tokens(&self, _: &dyn OutboundHttp, _: &ManagerTokens) -> Result<RotatedTokens, ProvisionError> {
        Err(ProvisionError::NotConfigured)
    }

    async fn create(
        &self,
        _: &dyn OutboundHttp,
        _: &ManagerTokens,
        _: &AppManifest,
    ) -> Result<CreatedApp, ProvisionError> {
        Err(ProvisionError::NotConfigured)
    }

    async fn find(&self, _: &dyn OutboundHttp, _: &ManagerTokens, _: &str) -> Result<Option<String>, ProvisionError> {
        Err(ProvisionError::NotConfigured)
    }

    async fn update(
        &self,
        _: &dyn OutboundHttp,
        _: &ManagerTokens,
        _: &str,
        _: &AppManifest,
    ) -> Result<(), ProvisionError> {
        Err(ProvisionError::NotConfigured)
    }

    async fn delete(&self, _: &dyn OutboundHttp, _: &ManagerTokens, _: &str) -> Result<(), ProvisionError> {
        Err(ProvisionError::NotConfigured)
    }

    fn consent_url(&self, _: &ConsentRequest<'_>) -> Result<String, ProvisionError> {
        Err(ProvisionError::NotConfigured)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use zupflow_core::channels::provision::{SLACK_EVENTS, SLACK_SCOPES};
    use zupflow_core::outbound::{Limits, OutboundError, OutboundRequest, OutboundResponse};

    use super::*;

    /// The scripted provisioner sends nothing: anything sent fails the test.
    struct NoRequests;

    #[async_trait]
    impl OutboundHttp for NoRequests {
        async fn send(&self, _: OutboundRequest, _: Limits) -> Result<OutboundResponse, OutboundError> {
            panic!("the scripted provisioner sent a request");
        }
    }

    fn tokens(configuration: &str, refresh: &str) -> ManagerTokens {
        ManagerTokens {
            configuration_token: Zeroizing::new(configuration.to_owned()),
            refresh_token: Zeroizing::new(refresh.to_owned()),
        }
    }

    fn manifest(events_url: &str) -> AppManifest {
        AppManifest {
            name: "Scribe".to_owned(),
            description: String::new(),
            handle: "scribe".to_owned(),
            scopes: SLACK_SCOPES,
            events: SLACK_EVENTS,
            events_url: events_url.to_owned(),
            redirect_url: "https://zupflow.example/connect/callback".to_owned(),
        }
    }

    #[tokio::test]
    async fn refuses_tokens_marked_invalid_in_any_case() {
        let p = ScriptedChannelProvisioner::default();
        let good = tokens("xoxe.xoxp-1-good", "xoxe-1-good");
        assert!(
            p.check_tokens(&NoRequests, ChannelProviderKind::Slack, &good)
                .await
                .unwrap()
                .expires_at
                .is_some()
        );
        for bad in [tokens("xoxe.INVALID", "xoxe-1"), tokens("xoxe.ok", "xoxe-Invalid")] {
            assert_eq!(
                p.check_tokens(&NoRequests, ChannelProviderKind::Slack, &bad).await,
                Err(ProvisionError::TokensRefused)
            );
            assert_eq!(
                p.create(&NoRequests, &bad, &manifest("https://a")).await.err(),
                Some(ProvisionError::TokensRefused)
            );
        }
        assert!(p.apps().is_empty());
    }

    #[tokio::test]
    async fn makes_an_app_waiting_for_consent_with_fresh_secrets() {
        let p = ScriptedChannelProvisioner::default();
        let good = tokens("xoxe.xoxp-1-good", "xoxe-1-good");
        let events_url = "https://z.example/channels/inbound/a";
        let first = p.create(&NoRequests, &good, &manifest(events_url)).await.unwrap();
        let again = p.create(&NoRequests, &good, &manifest(events_url)).await.unwrap();
        assert!(first.external_app_id.starts_with("A0") && first.external_app_id.len() == 11);
        assert_ne!(first.external_app_id, again.external_app_id, "every app its own ID");
        assert_ne!(*first.secrets.signing_secret, *again.secrets.signing_secret);
        assert!(matches!(first.install, InstallStep::NeedsConsent));
        let url = p
            .consent_url(&ConsentRequest {
                client_id: &first.client_id,
                scopes: SLACK_SCOPES,
                redirect_url: "https://z.example/cb?x=1",
                state: "s&t",
            })
            .unwrap();
        assert!(url.starts_with(CONSENT_URL) && url.contains("state=s%26t") && url.contains("cb%3Fx%3D1"));
    }

    #[tokio::test]
    async fn finds_and_deletes_an_app_whose_answer_was_lost() {
        let p = ScriptedChannelProvisioner::default();
        let good = tokens("xoxe.xoxp-1-good", "xoxe-1-good");
        let events_url = "https://z.example/channels/inbound/b";
        p.lose_next_answer();
        assert_eq!(
            p.create(&NoRequests, &good, &manifest(events_url)).await.err(),
            Some(ProvisionError::Unavailable)
        );
        let lost = p.find(&NoRequests, &good, events_url).await.unwrap().unwrap();
        assert_eq!(p.apps()[0].external_app_id, lost, "made, though its answer was lost");
        assert_eq!(p.find(&NoRequests, &good, "https://z.example/other").await, Ok(None));
        p.fail_next_delete();
        assert_eq!(
            p.delete(&NoRequests, &good, &lost).await,
            Err(ProvisionError::Unavailable)
        );
        assert_eq!(p.delete(&NoRequests, &good, &lost).await, Ok(()));
        assert_eq!(
            p.delete(&NoRequests, &good, &lost).await,
            Err(ProvisionError::AppNotFound)
        );
        assert_eq!(
            p.update(&NoRequests, &good, &lost, &manifest(events_url)).await,
            Err(ProvisionError::AppNotFound)
        );
        assert_eq!(p.find(&NoRequests, &good, events_url).await, Ok(None));
        assert!(p.apps()[0].deleted);
    }

    #[tokio::test]
    async fn without_a_provider_does_nothing() {
        let good = tokens("xoxe.xoxp-1-good", "xoxe-1-good");
        assert_eq!(
            NoChannelProvisioner
                .check_tokens(&NoRequests, ChannelProviderKind::Slack, &good)
                .await,
            Err(ProvisionError::NotConfigured)
        );
        assert_eq!(
            NoChannelProvisioner
                .create(&NoRequests, &good, &manifest("https://a"))
                .await
                .err(),
            Some(ProvisionError::NotConfigured)
        );
    }
}
