//! The `OrganizationStore` suite: one user per identity, invitees claimed only
//! by a verified email, members reached only through their own organization,
//! invitations that aren't memberships yet, and an organization never left
//! without an active owner.

use zupflow_core::identity::{Profile, VerifiedIdentity};
use zupflow_core::ids::{MemberId, OrganizationId};
use zupflow_core::organizations::{
    Member, MemberRole, MemberStatus, Organization, OrganizationStore, StoreError, User,
};

/// A unique token, so the suite can run against a store that already has data.
fn unique() -> String {
    OrganizationId::generate().uuid().simple().to_string()
}

fn identity(subject: &str, email: &str, email_verified: bool) -> VerifiedIdentity {
    VerifiedIdentity {
        provider: "conformance".to_owned(),
        subject: subject.to_owned(),
        profile: Profile {
            name: "Test Person".to_owned(),
            email: email.to_owned(),
            email_verified,
            avatar_url: None,
        },
    }
}

async fn person(store: &dyn OrganizationStore) -> User {
    let token = unique();
    store
        .user_for_identity(&identity(&token, &format!("{token}@people.example"), true))
        .await
        .unwrap()
}

async fn organization(store: &dyn OrganizationStore, owner: &User) -> (Organization, Member) {
    // A slug of 1–40 lowercase letters, numbers and hyphens.
    let slug = format!("org-{}", &unique()[..30]);
    store
        .create_organization(OrganizationId::generate(), "Test", &slug, owner.id)
        .await
        .unwrap()
}

pub async fn run(store: &dyn OrganizationStore) {
    maps_an_identity_to_one_user(store).await;
    lets_only_a_verified_email_claim_an_invitee(store).await;
    creates_an_organization_with_its_owner(store).await;
    keeps_slugs_unique(store).await;
    keeps_members_inside_their_organization(store).await;
    treats_an_invitation_as_not_yet_a_membership(store).await;
    never_leaves_an_organization_without_an_active_owner(store).await;
    refuses_a_second_membership(store).await;
}

async fn maps_an_identity_to_one_user(store: &dyn OrganizationStore) {
    let token = unique();
    let signed_in = identity(&token, &format!("{token}@people.example"), true);
    let first = store.user_for_identity(&signed_in).await.unwrap();
    let again = store.user_for_identity(&signed_in).await.unwrap();
    assert_eq!(first, again, "the same identity is the same user");
    assert_eq!(first.email, format!("{token}@people.example"));
    let other = person(store).await;
    assert_ne!(other.id, first.id);
}

async fn lets_only_a_verified_email_claim_an_invitee(store: &dyn OrganizationStore) {
    let owner = person(store).await;
    let (org, _) = organization(store, &owner).await;
    let email = format!("{}@people.example", unique());

    // Someone registers the address first, unverified: no user comes of it.
    let squatter = identity(&unique(), &email, false);
    assert_eq!(
        store.user_for_identity(&squatter).await,
        Err(StoreError::EmailUnverified),
        "an unverified email creates no user"
    );
    let invitation = store
        .invite_member(org.id, &email.to_uppercase(), MemberRole::Owner)
        .await
        .unwrap();
    assert_eq!(invitation.user.email, email, "emails are lowercased");
    assert_eq!(
        store.user_for_identity(&squatter).await,
        Err(StoreError::EmailUnverified),
        "nor claims the invitee"
    );

    // The real person, verified, claims the invitee and takes their profile.
    let token = unique();
    let mut real = identity(&token, &email, true);
    real.profile.name = "Real Person".to_owned();
    let claimed = store.user_for_identity(&real).await.unwrap();
    assert_eq!(claimed.id, invitation.user.id);
    assert_eq!(claimed.name, "Real Person");

    // Linked from now on, whatever the email says; the profile follows.
    real.profile.email_verified = false;
    real.profile.name = "Renamed".to_owned();
    let again = store.user_for_identity(&real).await.unwrap();
    assert_eq!((again.id, again.name.as_str()), (claimed.id, "Renamed"));

    // Nobody else claims a user somebody already signs in as.
    assert_eq!(
        store.user_for_identity(&identity(&unique(), &email, true)).await,
        Err(StoreError::EmailInUse)
    );
}

async fn creates_an_organization_with_its_owner(store: &dyn OrganizationStore) {
    let owner = person(store).await;
    let (org, member) = organization(store, &owner).await;
    assert_eq!(member.organization_id, org.id);
    assert_eq!(member.user, owner);
    assert_eq!((member.role, member.status), (MemberRole::Owner, MemberStatus::Active));
    assert!(member.joined_at.is_some());

    let memberships = store.list_active_memberships_of_user(owner.id).await.unwrap();
    assert_eq!(memberships.len(), 1);
    assert_eq!(memberships[0].0, org);

    let (found_org, found) = store
        .find_active_membership(org.id, owner.id)
        .await
        .unwrap()
        .expect("the owner's membership");
    assert_eq!((found_org.id, found.id), (org.id, member.id));
    assert!(found.last_active_at.is_some(), "activity is recorded");

    let setup = store.load_setup(org.id).await.unwrap();
    assert!(!setup.has_workspace, "a new organization has no workspace");

    let renamed = store.update_organization(org.id, Some("Renamed"), None).await.unwrap();
    assert_eq!(
        (renamed.name.as_str(), renamed.slug.as_str()),
        ("Renamed", org.slug.as_str())
    );
    assert_eq!(
        store
            .update_organization(OrganizationId::generate(), Some("Nobody"), None)
            .await,
        Err(StoreError::NotFound)
    );
}

async fn keeps_slugs_unique(store: &dyn OrganizationStore) {
    let owner = person(store).await;
    let (first, _) = organization(store, &owner).await;
    let (second, _) = organization(store, &owner).await;
    assert_eq!(
        store
            .create_organization(OrganizationId::generate(), "Copy", &first.slug, owner.id)
            .await,
        Err(StoreError::SlugTaken)
    );
    assert_eq!(
        store.update_organization(second.id, None, Some(&first.slug)).await,
        Err(StoreError::SlugTaken)
    );
    assert_eq!(
        store.list_active_memberships_of_user(owner.id).await.unwrap().len(),
        2,
        "the refused organization wasn't created"
    );
}

async fn keeps_members_inside_their_organization(store: &dyn OrganizationStore) {
    let (alice, bob) = (person(store).await, person(store).await);
    let (a, a_owner) = organization(store, &alice).await;
    let (b, b_owner) = organization(store, &bob).await;
    let b_invitee = store
        .invite_member(b.id, &format!("{}@people.example", unique()), MemberRole::Viewer)
        .await
        .unwrap();

    let a_members = store.list_members(a.id).await.unwrap();
    assert_eq!(a_members.len(), 1, "only a's own member");
    assert!(a_members.iter().all(|member| member.organization_id == a.id));

    for foreign in [&b_owner, &b_invitee] {
        assert_eq!(store.find_member(a.id, foreign.id).await.unwrap(), None);
        assert_eq!(
            store
                .update_member_role(a.id, foreign.id, MemberRole::Admin, a_owner.id)
                .await,
            Err(StoreError::NotFound)
        );
        assert_eq!(
            store.remove_member(a.id, foreign.id, a_owner.id).await,
            Err(StoreError::NotFound)
        );
    }
    // Nor does b's owner act in a.
    assert_eq!(
        store
            .update_member_role(a.id, a_owner.id, MemberRole::Admin, b_owner.id)
            .await,
        Err(StoreError::NotAllowed)
    );
    assert_eq!(
        store.remove_member(a.id, a_owner.id, b_owner.id).await,
        Err(StoreError::NotAllowed)
    );
    assert_eq!(
        store.find_active_membership(a.id, bob.id).await.unwrap(),
        None,
        "bob isn't in a"
    );
    assert_eq!(
        store.list_members(b.id).await.unwrap(),
        vec![b_owner.clone(), b_invitee.clone()],
        "b's members are untouched, oldest first"
    );
    assert_eq!(store.find_member(b.id, MemberId::generate()).await.unwrap(), None);
}

async fn treats_an_invitation_as_not_yet_a_membership(store: &dyn OrganizationStore) {
    let (alice, bob) = (person(store).await, person(store).await);
    let (a, _) = organization(store, &alice).await;
    let invitation = store.invite_member(a.id, &bob.email, MemberRole::Owner).await.unwrap();
    assert_eq!(
        invitation.user.id, bob.id,
        "an existing user is invited, not duplicated"
    );
    assert_eq!(invitation.status, MemberStatus::Invited);
    assert_eq!(invitation.joined_at, None);
    assert_eq!(store.find_active_membership(a.id, bob.id).await.unwrap(), None);
    assert!(store.list_active_memberships_of_user(bob.id).await.unwrap().is_empty());
}

async fn never_leaves_an_organization_without_an_active_owner(store: &dyn OrganizationStore) {
    let (alice, bob) = (person(store).await, person(store).await);
    let (a, alice_member) = organization(store, &alice).await;
    // Another organization's owner, and an invited owner here, don't count.
    let (_b, _) = organization(store, &bob).await;
    let invited_owner = store
        .invite_member(a.id, &format!("{}@people.example", unique()), MemberRole::Owner)
        .await
        .unwrap();
    assert_eq!(
        store
            .update_member_role(a.id, alice_member.id, MemberRole::Admin, alice_member.id)
            .await,
        Err(StoreError::LastOwner)
    );
    assert_eq!(
        store.remove_member(a.id, alice_member.id, alice_member.id).await,
        Err(StoreError::LastOwner)
    );
    // An invited owner can't act yet.
    assert_eq!(
        store
            .update_member_role(a.id, alice_member.id, MemberRole::Admin, invited_owner.id)
            .await,
        Err(StoreError::NotAllowed)
    );
    assert_eq!(
        store.remove_member(a.id, alice_member.id, invited_owner.id).await,
        Err(StoreError::NotAllowed)
    );
    let unchanged = store.find_member(a.id, alice_member.id).await.unwrap().unwrap();
    assert_eq!(unchanged.role, MemberRole::Owner);

    // An invited owner can always be changed or withdrawn.
    let demoted = store
        .update_member_role(a.id, invited_owner.id, MemberRole::Developer, alice_member.id)
        .await
        .unwrap();
    assert_eq!(demoted.role, MemberRole::Developer);
    store
        .remove_member(a.id, invited_owner.id, alice_member.id)
        .await
        .unwrap();
    assert_eq!(store.find_member(a.id, invited_owner.id).await.unwrap(), None);
    assert_eq!(
        store
            .update_member_role(a.id, alice_member.id, MemberRole::Owner, alice_member.id)
            .await
            .unwrap()
            .role,
        MemberRole::Owner,
        "keeping the last owner an owner is fine"
    );
}

async fn refuses_a_second_membership(store: &dyn OrganizationStore) {
    let alice = person(store).await;
    let (a, _) = organization(store, &alice).await;
    let email = format!("{}@people.example", unique());
    store.invite_member(a.id, &email, MemberRole::Viewer).await.unwrap();
    assert_eq!(
        store.invite_member(a.id, &email, MemberRole::Admin).await,
        Err(StoreError::AlreadyMember)
    );
    assert_eq!(
        store.invite_member(a.id, &alice.email, MemberRole::Admin).await,
        Err(StoreError::AlreadyMember),
        "nor the owner"
    );
    assert_eq!(store.list_members(a.id).await.unwrap().len(), 2);
}
