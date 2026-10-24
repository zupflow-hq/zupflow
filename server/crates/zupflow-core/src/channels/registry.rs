//! [`ChannelProviders`]: every channel provider a server reaches, at once,
//! by [`ChannelProviderKind`]. One server links Slack teams and, later,
//! other providers' side by side; nothing picks a single one.
//!
//! Each kind has one [`AppProvisioner`], which makes agents' apps in linked
//! teams, and one [`ChannelProvider`], which reads deliveries to those apps
//! and acts with their connections' tokens. Everything that works with a
//! team, an app or a connection asks for the pair of the provider it was
//! stored with, never a global one.
//!
//! A kind the server doesn't reach answers through stand-ins, so callers
//! need no special case. Its provisioner says `NotConfigured` to what a
//! person asks at once (linking a team, allowing an app), and `Unreached`
//! to what a job does for a team, app or connection already stored, which
//! waits, using no attempt, for a server that reaches it: a restart with
//! the provider's settings picks it up. Its provider refuses every delivery
//! and answers every call `Unavailable`.

use std::sync::Arc;

use async_trait::async_trait;

use super::ChannelProviderKind;
use super::ManagerMethod;
use super::provider::{
    ChannelProvider, InboundDelivery, InboundRefusal, InboundRequest, JoinedConversation, MessageRef,
    NormalizedMessage, OutgoingPost, ProviderCallError, ProviderPerson, SignatureHeaders,
};
use super::provision::{
    AppManifest, AppProvisioner, CheckedTokens, ConsentExchange, ConsentRequest, ConsentedInstall, CreatedApp,
    ManagerTokens, ProvisionError, RotatedTokens,
};
use crate::outbound::OutboundHttp;
use crate::time::Timestamp;

/// One kind's pair.
#[derive(Clone)]
struct Entry {
    kind: ChannelProviderKind,
    provisioner: Arc<dyn AppProvisioner>,
    provider: Arc<dyn ChannelProvider>,
}

/// The channel providers a server reaches, by kind. Cheap to clone: clones
/// share the same providers.
#[derive(Clone, Default)]
pub struct ChannelProviders {
    entries: Vec<Entry>,
}

impl std::fmt::Debug for ChannelProviders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChannelProviders")
            .field("available", &self.available())
            .finish()
    }
}

impl ChannelProviders {
    /// None: no team can be linked, and no delivery is read.
    pub fn none() -> Self {
        Self::default()
    }

    /// Adds the pair for `provider.kind()`, replacing one of that kind.
    #[must_use]
    pub fn with(mut self, provisioner: Arc<dyn AppProvisioner>, provider: Arc<dyn ChannelProvider>) -> Self {
        let kind = provider.kind();
        self.entries.retain(|entry| entry.kind != kind);
        self.entries.push(Entry {
            kind,
            provisioner,
            provider,
        });
        self
    }

    fn entry(&self, kind: ChannelProviderKind) -> Option<&Entry> {
        self.entries.iter().find(|entry| entry.kind == kind)
    }

    /// The kinds this server reaches, in [`ChannelProviderKind::ALL`]'s order.
    pub fn available(&self) -> Vec<ChannelProviderKind> {
        ChannelProviderKind::ALL
            .iter()
            .copied()
            .filter(|kind| self.is_available(*kind))
            .collect()
    }

    pub fn is_available(&self, kind: ChannelProviderKind) -> bool {
        self.entry(kind).is_some()
    }

    /// What makes apps in `kind`'s teams: when the server doesn't reach it,
    /// one that answers `NotConfigured` to what a person asks and
    /// `Unreached` to what a job does.
    pub fn provisioner(&self, kind: ChannelProviderKind) -> Arc<dyn AppProvisioner> {
        match self.entry(kind) {
            Some(entry) => entry.provisioner.clone(),
            None => Arc::new(Unreached),
        }
    }

    /// What reads `kind`'s deliveries and acts with its connections'
    /// tokens: when the server doesn't reach it, one that refuses every
    /// delivery and answers every call `Unavailable`.
    pub fn provider(&self, kind: ChannelProviderKind) -> Arc<dyn ChannelProvider> {
        match self.entry(kind) {
            Some(entry) => entry.provider.clone(),
            None => Arc::new(Unreachable { kind }),
        }
    }

    /// Every provider this server reaches, in [`Self::available`]'s order.
    pub fn providers(&self) -> Vec<Arc<dyn ChannelProvider>> {
        self.available().into_iter().map(|kind| self.provider(kind)).collect()
    }
}

/// The provisioner of a kind the server doesn't reach: `NotConfigured` to
/// checking tokens and consent, `Unreached` to the rest.
struct Unreached;

#[async_trait]
impl AppProvisioner for Unreached {
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
        Err(ProvisionError::Unreached)
    }

    async fn create(
        &self,
        _: &dyn OutboundHttp,
        _: &ManagerTokens,
        _: &AppManifest,
    ) -> Result<CreatedApp, ProvisionError> {
        Err(ProvisionError::Unreached)
    }

    async fn find(&self, _: &dyn OutboundHttp, _: &ManagerTokens, _: &str) -> Result<Option<String>, ProvisionError> {
        Err(ProvisionError::Unreached)
    }

    async fn update(
        &self,
        _: &dyn OutboundHttp,
        _: &ManagerTokens,
        _: &str,
        _: &AppManifest,
    ) -> Result<(), ProvisionError> {
        Err(ProvisionError::Unreached)
    }

    async fn delete(&self, _: &dyn OutboundHttp, _: &ManagerTokens, _: &str) -> Result<(), ProvisionError> {
        Err(ProvisionError::Unreached)
    }

    fn consent_url(&self, _: &ConsentRequest<'_>) -> Result<String, ProvisionError> {
        Err(ProvisionError::NotConfigured)
    }

    async fn complete_consent(
        &self,
        _: &dyn OutboundHttp,
        _: &ConsentExchange<'_>,
    ) -> Result<ConsentedInstall, ProvisionError> {
        Err(ProvisionError::NotConfigured)
    }

    async fn revoke_install(&self, _: &dyn OutboundHttp, _: &str) -> Result<(), ProvisionError> {
        Err(ProvisionError::Unreached)
    }
}

/// The provider of a kind the server doesn't reach.
struct Unreachable {
    kind: ChannelProviderKind,
}

#[async_trait]
impl ChannelProvider for Unreachable {
    fn kind(&self) -> ChannelProviderKind {
        self.kind
    }

    fn signature_headers(&self) -> SignatureHeaders {
        SignatureHeaders {
            timestamp: "",
            signature: "",
        }
    }

    fn check_timestamp(&self, _: &InboundRequest<'_>, _: Timestamp) -> Result<(), InboundRefusal> {
        Err(InboundRefusal::InvalidSignature)
    }

    fn verify(&self, _: &InboundRequest<'_>, _: &[u8], _: Timestamp) -> Result<(), InboundRefusal> {
        Err(InboundRefusal::InvalidSignature)
    }

    fn read(&self, _: &[u8], _: Timestamp) -> Result<InboundDelivery, InboundRefusal> {
        Err(InboundRefusal::Unreadable)
    }

    async fn person(
        &self,
        _: &dyn OutboundHttp,
        _: &str,
        _: &str,
    ) -> Result<Option<ProviderPerson>, ProviderCallError> {
        Err(ProviderCallError::Unavailable)
    }

    async fn post(&self, _: &dyn OutboundHttp, _: &str, _: &OutgoingPost<'_>) -> Result<String, ProviderCallError> {
        Err(ProviderCallError::Unavailable)
    }

    async fn find_sent(
        &self,
        _: &dyn OutboundHttp,
        _: &str,
        _: &OutgoingPost<'_>,
        _: Timestamp,
    ) -> Result<Option<String>, ProviderCallError> {
        Err(ProviderCallError::Unavailable)
    }

    async fn edit(&self, _: &dyn OutboundHttp, _: &str, _: &MessageRef<'_>, _: &str) -> Result<(), ProviderCallError> {
        Err(ProviderCallError::Unavailable)
    }

    async fn delete(&self, _: &dyn OutboundHttp, _: &str, _: &MessageRef<'_>) -> Result<(), ProviderCallError> {
        Err(ProviderCallError::Unavailable)
    }

    async fn react(
        &self,
        _: &dyn OutboundHttp,
        _: &str,
        _: &MessageRef<'_>,
        _: &str,
        _: bool,
    ) -> Result<(), ProviderCallError> {
        Err(ProviderCallError::Unavailable)
    }

    async fn open_direct(&self, _: &dyn OutboundHttp, _: &str, _: &str) -> Result<String, ProviderCallError> {
        Err(ProviderCallError::Unavailable)
    }

    async fn recent_messages(
        &self,
        _: &dyn OutboundHttp,
        _: &str,
        _: &JoinedConversation,
        _: Timestamp,
        _: u32,
    ) -> Result<Vec<NormalizedMessage>, ProviderCallError> {
        Err(ProviderCallError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use zeroize::Zeroizing;

    use super::*;
    use crate::outbound::{Limits, OutboundError, OutboundRequest, OutboundResponse};

    /// Nothing here may send a request.
    struct NoRequests;

    #[async_trait]
    impl OutboundHttp for NoRequests {
        async fn send(&self, _: OutboundRequest, _: Limits) -> Result<OutboundResponse, OutboundError> {
            panic!("a stand-in sent a request");
        }
    }

    /// A provisioner that names itself in the one call it answers.
    struct Named(&'static str);

    #[async_trait]
    impl AppProvisioner for Named {
        fn method(&self) -> ManagerMethod {
            ManagerMethod::ConfigurationToken
        }
        async fn check_tokens(
            &self,
            _: &dyn OutboundHttp,
            _: ChannelProviderKind,
            _: &ManagerTokens,
        ) -> Result<CheckedTokens, ProvisionError> {
            Err(ProvisionError::Refused(self.0))
        }
        async fn rotate_tokens(
            &self,
            _: &dyn OutboundHttp,
            _: &ManagerTokens,
        ) -> Result<RotatedTokens, ProvisionError> {
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
        async fn find(
            &self,
            _: &dyn OutboundHttp,
            _: &ManagerTokens,
            _: &str,
        ) -> Result<Option<String>, ProvisionError> {
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
            Ok(self.0.to_owned())
        }
    }

    fn tokens() -> ManagerTokens {
        ManagerTokens {
            configuration_token: Zeroizing::new("xoxe.xoxp-1".to_owned()),
            refresh_token: Zeroizing::new("xoxe-1".to_owned()),
        }
    }

    fn consent() -> ConsentRequest<'static> {
        ConsentRequest {
            client_id: "1.2",
            scopes: &[],
            redirect_url: "https://zupflow.example/connect/oauth/callback",
            state: "s",
        }
    }

    #[tokio::test]
    async fn without_a_provider_every_call_is_refused_or_waits() {
        let none = ChannelProviders::none();
        assert!(none.available().is_empty());
        assert!(none.providers().is_empty());
        let slack = ChannelProviderKind::Slack;
        assert!(!none.is_available(slack));
        let provisioner = none.provisioner(slack);
        assert_eq!(
            provisioner.check_tokens(&NoRequests, slack, &tokens()).await.err(),
            Some(ProvisionError::NotConfigured)
        );
        assert_eq!(provisioner.consent_url(&consent()), Err(ProvisionError::NotConfigured));
        // What a job does for what's stored waits for a server that reaches it.
        assert_eq!(
            provisioner.rotate_tokens(&NoRequests, &tokens()).await.err(),
            Some(ProvisionError::Unreached)
        );
        assert_eq!(
            provisioner.delete(&NoRequests, &tokens(), "A1").await,
            Err(ProvisionError::Unreached)
        );
        assert_eq!(
            provisioner.revoke_install(&NoRequests, "xoxb-1").await,
            Err(ProvisionError::Unreached)
        );
        let provider = none.provider(slack);
        assert_eq!(provider.kind(), slack);
        let request = InboundRequest {
            timestamp: Some("1700000000"),
            signature: Some("v0=00"),
            body: b"{}",
        };
        let now = Timestamp::from_millis(1_700_000_000_000);
        assert_eq!(
            provider.check_timestamp(&request, now),
            Err(InboundRefusal::InvalidSignature)
        );
        assert_eq!(
            provider.verify(&request, b"secret", now),
            Err(InboundRefusal::InvalidSignature)
        );
        let post = OutgoingPost {
            conversation_external_id: "C1",
            thread_key: "",
            text: "hi",
            send_id: "run",
        };
        assert_eq!(
            provider.post(&NoRequests, "xoxb-1", &post).await,
            Err(ProviderCallError::Unavailable)
        );
        assert!(!provider.show_working(&NoRequests, "xoxb-1", "C1", "", true).await);
    }

    /// Unreachable stands in for a real provider here: what matters is
    /// which pair a kind gets, and that a later pair of the same kind
    /// replaces the earlier.
    #[tokio::test]
    async fn answers_each_kind_with_its_own_pair() {
        let slack = ChannelProviderKind::Slack;
        let providers = ChannelProviders::none()
            .with(Arc::new(Named("first")), Arc::new(Unreachable { kind: slack }))
            .with(Arc::new(Named("second")), Arc::new(Unreachable { kind: slack }));
        assert_eq!(providers.available(), vec![slack]);
        assert!(providers.is_available(slack));
        assert_eq!(providers.providers().len(), 1);
        assert_eq!(
            providers.provisioner(slack).consent_url(&consent()),
            Ok("second".to_owned())
        );
        assert_eq!(
            providers
                .provisioner(slack)
                .check_tokens(&NoRequests, slack, &tokens())
                .await
                .err(),
            Some(ProvisionError::Refused("second"))
        );
        assert_eq!(format!("{providers:?}"), "ChannelProviders { available: [Slack] }");
        // Clones share them.
        let clone = providers.clone();
        assert_eq!(
            clone.provisioner(slack).consent_url(&consent()),
            Ok("second".to_owned())
        );
    }
}
