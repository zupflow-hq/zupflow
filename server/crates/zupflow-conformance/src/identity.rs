//! The `Identity` suite, for adapters that check a credential (`oidc`, or a
//! private one). `local` vouches for everyone on purpose and has its own tests.
//!
//! An adapter vouches only for a credential it can verify, reports an outage
//! as [`IdentityError::Unavailable`] rather than as a bad credential (each
//! adapter tests that against its own provider: the suite has no way to take
//! one down), and
//! hands over a profile Zupflow can rely on: a stable provider, the subject
//! as issued, a lowercased email, and `email_verified` only when the provider
//! asserts it (decision 0027 rests on that flag).

use zupflow_core::identity::{Identity, IdentityError};

/// What the suite needs from the adapter's test harness.
pub trait Credentials: Send + Sync {
    /// A credential the adapter must accept for this person.
    fn issue(&self, subject: &str, email: &str, email_verified: bool) -> String;
    /// Credentials it must refuse, each with what's wrong with it: forged,
    /// expired, for another audience, and so on.
    fn forged(&self) -> Vec<(String, String)>;
}

pub async fn run(identity: &dyn Identity, credentials: &dyn Credentials) {
    refuses_a_missing_or_forged_credential(identity, credentials).await;
    vouches_for_who_the_credential_names(identity, credentials).await;
    asserts_a_verified_email_only_when_the_provider_does(identity, credentials).await;
}

async fn refuses_a_missing_or_forged_credential(identity: &dyn Identity, credentials: &dyn Credentials) {
    assert_eq!(
        identity.verify(None).await,
        Err(IdentityError::Unauthenticated),
        "no credential"
    );
    let forged = credentials.forged();
    assert!(!forged.is_empty(), "the harness offers forged credentials");
    for (what, credential) in forged {
        assert_eq!(
            identity.verify(Some(&credential)).await,
            Err(IdentityError::Unauthenticated),
            "{what}: refused as a bad credential, not an outage"
        );
    }
}

async fn vouches_for_who_the_credential_names(identity: &dyn Identity, credentials: &dyn Credentials) {
    let first = identity
        .verify(Some(&credentials.issue(
            "subject-1",
            "First.Person@People.Example",
            true,
        )))
        .await
        .unwrap();
    let second = identity
        .verify(Some(&credentials.issue("subject-2", "second@people.example", true)))
        .await
        .unwrap();
    assert!(!first.provider.is_empty());
    assert_eq!(first.provider, second.provider, "one adapter, one stable provider");
    assert_eq!(
        (first.subject.as_str(), second.subject.as_str()),
        ("subject-1", "subject-2")
    );
    assert_eq!(
        first.profile.email, "first.person@people.example",
        "emails are lowercased"
    );
    let again = identity
        .verify(Some(&credentials.issue(
            "subject-1",
            "first.person@people.example",
            true,
        )))
        .await
        .unwrap();
    assert_eq!(
        (again.provider, again.subject),
        (first.provider, first.subject),
        "the same person again"
    );
}

async fn asserts_a_verified_email_only_when_the_provider_does(identity: &dyn Identity, credentials: &dyn Credentials) {
    let unverified = identity
        .verify(Some(&credentials.issue("subject-3", "third@people.example", false)))
        .await
        .unwrap();
    assert!(!unverified.profile.email_verified);
    let verified = identity
        .verify(Some(&credentials.issue("subject-3", "third@people.example", true)))
        .await
        .unwrap();
    assert!(verified.profile.email_verified);
}
