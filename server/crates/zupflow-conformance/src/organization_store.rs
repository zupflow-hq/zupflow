//! The `OrganizationStore` suite: one user per identity, invitees claimed only
//! by a verified email, members reached only through their own organization,
//! invitations that aren't memberships yet and that only their invitee
//! answers, once, and an organization never left without an active owner.

use zupflow_core::identity::{Profile, VerifiedIdentity};
use zupflow_core::ids::{MemberId, OrganizationId};
use zupflow_core::organizations::{
    EXPIRED_INVITATION_SHOWN_MS, INVITATION_LIST_LIMIT, INVITATION_TTL_MS, Member, MemberRole, MemberStatus,
    Organization, OrganizationStore, StoreError, User,
};
use zupflow_core::time::Timestamp;

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
    shows_an_invitation_to_its_invitee_only(store).await;
    joins_only_when_the_invitee_accepts(store).await;
    refuses_an_expired_invitation(store).await;
    withdraws_an_invitation_its_invitee_declines(store).await;
    joins_once_when_answers_race(store).await;
    grants_only_what_the_grantor_can_still_grant(store).await;
    cancels_only_an_invitation(store).await;
    lists_the_newest_invitations_and_none_long_expired(store).await;
    never_lets_an_accept_outrun_its_grantor(store).await;
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
    let (org, owner_member) = organization(store, &owner).await;
    let email = format!("{}@people.example", unique());

    // Someone registers the address first, unverified: no user comes of it.
    let squatter = identity(&unique(), &email, false);
    assert_eq!(
        store.user_for_identity(&squatter).await,
        Err(StoreError::EmailUnverified),
        "an unverified email creates no user"
    );
    let invitation = store
        .invite_member(org.id, &email.to_uppercase(), MemberRole::Owner, owner_member.id)
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
        .invite_member(
            b.id,
            &format!("{}@people.example", unique()),
            MemberRole::Viewer,
            b_owner.id,
        )
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
            store.remove_member(a.id, foreign.id, a_owner.id, false).await,
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
        store.remove_member(a.id, a_owner.id, b_owner.id, false).await,
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
    let (a, alice_member) = organization(store, &alice).await;
    let invitation = store
        .invite_member(a.id, &bob.email, MemberRole::Owner, alice_member.id)
        .await
        .unwrap();
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
        .invite_member(
            a.id,
            &format!("{}@people.example", unique()),
            MemberRole::Owner,
            alice_member.id,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .update_member_role(a.id, alice_member.id, MemberRole::Admin, alice_member.id)
            .await,
        Err(StoreError::LastOwner)
    );
    assert_eq!(
        store.remove_member(a.id, alice_member.id, alice_member.id, false).await,
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
        store
            .remove_member(a.id, alice_member.id, invited_owner.id, false)
            .await,
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
        .remove_member(a.id, invited_owner.id, alice_member.id, false)
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
    let (a, alice_member) = organization(store, &alice).await;
    let email = format!("{}@people.example", unique());
    store
        .invite_member(a.id, &email, MemberRole::Viewer, alice_member.id)
        .await
        .unwrap();
    assert_eq!(
        store
            .invite_member(a.id, &email, MemberRole::Admin, alice_member.id)
            .await,
        Err(StoreError::AlreadyMember)
    );
    assert_eq!(
        store
            .invite_member(a.id, &alice.email, MemberRole::Admin, alice_member.id)
            .await,
        Err(StoreError::AlreadyMember),
        "nor the owner"
    );
    assert_eq!(store.list_members(a.id).await.unwrap().len(), 2);
}

async fn shows_an_invitation_to_its_invitee_only(store: &dyn OrganizationStore) {
    let (alice, bob, carol) = (person(store).await, person(store).await, person(store).await);
    let (a, alice_member) = organization(store, &alice).await;
    let sent = store
        .invite_member(a.id, &bob.email, MemberRole::Developer, alice_member.id)
        .await
        .unwrap();
    assert_eq!(
        sent.expires_at.map(Timestamp::millis),
        Some(sent.invited_at.millis() + INVITATION_TTL_MS)
    );
    let listed = store.list_invitations_of_user(bob.id, Timestamp::now()).await.unwrap();
    assert_eq!(listed.len(), 1);
    let invitation = &listed[0];
    assert_eq!((invitation.id, invitation.role), (sent.id, MemberRole::Developer));
    assert_eq!(invitation.organization, a);
    let inviter = &invitation.invited_by;
    assert_eq!((&inviter.name, &inviter.email), (&alice.name, &alice.email));
    assert_eq!(
        invitation.expires_at.millis(),
        invitation.invited_at.millis() + INVITATION_TTL_MS
    );
    for other in [&alice, &carol] {
        assert!(
            store
                .list_invitations_of_user(other.id, Timestamp::now())
                .await
                .unwrap()
                .is_empty(),
            "nobody else sees it"
        );
    }
    // Another organization's invitation comes first: newest first.
    let (b, carol_member) = organization(store, &carol).await;
    let newer = store
        .invite_member(b.id, &bob.email, MemberRole::Viewer, carol_member.id)
        .await
        .unwrap();
    let listed = store.list_invitations_of_user(bob.id, Timestamp::now()).await.unwrap();
    let mut ids: Vec<MemberId> = listed.iter().map(|invitation| invitation.id).collect();
    assert!(
        listed.windows(2).all(|pair| pair[0].invited_at >= pair[1].invited_at),
        "newest first"
    );
    ids.sort_by_key(|id| id.uuid());
    let mut expected = vec![newer.id, sent.id];
    expected.sort_by_key(|id| id.uuid());
    assert_eq!(ids, expected);
}

async fn joins_only_when_the_invitee_accepts(store: &dyn OrganizationStore) {
    let (alice, bob, carol) = (person(store).await, person(store).await, person(store).await);
    let (a, alice_member) = organization(store, &alice).await;
    let invitation = store
        .invite_member(a.id, &bob.email, MemberRole::Viewer, alice_member.id)
        .await
        .unwrap();
    // Nobody but bob answers it, the inviter included, as if it didn't exist.
    for other in [&alice, &carol] {
        assert_eq!(
            store
                .accept_invitation_of_user(other.id, invitation.id, Timestamp::now())
                .await,
            Err(StoreError::NotFound)
        );
        assert_eq!(
            store.decline_invitation_of_user(other.id, invitation.id).await,
            Err(StoreError::NotFound)
        );
    }
    assert_eq!(
        store
            .accept_invitation_of_user(bob.id, MemberId::generate(), Timestamp::now())
            .await,
        Err(StoreError::NotFound)
    );
    // Nor is anyone's membership an invitation of someone else's.
    assert_eq!(
        store
            .accept_invitation_of_user(bob.id, alice_member.id, Timestamp::now())
            .await,
        Err(StoreError::NotFound)
    );

    // The role is the invitation's as accepted, not as sent.
    store
        .update_member_role(a.id, invitation.id, MemberRole::Admin, alice_member.id)
        .await
        .unwrap();
    let now = Timestamp::now();
    let (joined, member) = store
        .accept_invitation_of_user(bob.id, invitation.id, now)
        .await
        .unwrap();
    assert_eq!(joined, a);
    assert_eq!((member.id, member.user.id), (invitation.id, bob.id));
    assert_eq!((member.role, member.status), (MemberRole::Admin, MemberStatus::Active));
    assert_eq!((member.joined_at, member.expires_at), (Some(now), None));
    assert!(store.find_active_membership(a.id, bob.id).await.unwrap().is_some());
    assert!(
        store
            .list_invitations_of_user(bob.id, Timestamp::now())
            .await
            .unwrap()
            .is_empty()
    );

    // Answered once: a second accept or a decline is refused, and changes nothing.
    assert_eq!(
        store.accept_invitation_of_user(bob.id, invitation.id, now).await,
        Err(StoreError::InvitationAccepted)
    );
    assert_eq!(
        store.decline_invitation_of_user(bob.id, invitation.id).await,
        Err(StoreError::InvitationAccepted)
    );
    assert_eq!(
        store.find_member(a.id, invitation.id).await.unwrap().map(|m| m.status),
        Some(MemberStatus::Active)
    );
}

async fn refuses_an_expired_invitation(store: &dyn OrganizationStore) {
    let (alice, bob) = (person(store).await, person(store).await);
    let (a, alice_member) = organization(store, &alice).await;
    let invitation = store
        .invite_member(a.id, &bob.email, MemberRole::Developer, alice_member.id)
        .await
        .unwrap();
    let expires_at = invitation.expires_at.expect("an invitation expires");
    let just_before = Timestamp::from_millis(expires_at.millis() - 1);
    assert_eq!(
        store.accept_invitation_of_user(bob.id, invitation.id, expires_at).await,
        Err(StoreError::InvitationExpired)
    );
    assert_eq!(store.find_active_membership(a.id, bob.id).await.unwrap(), None);
    // Until then it's accepted.
    let (_, member) = store
        .accept_invitation_of_user(bob.id, invitation.id, just_before)
        .await
        .unwrap();
    assert_eq!(member.status, MemberStatus::Active);
}

async fn withdraws_an_invitation_its_invitee_declines(store: &dyn OrganizationStore) {
    let (alice, bob) = (person(store).await, person(store).await);
    let (a, alice_member) = organization(store, &alice).await;
    let invitation = store
        .invite_member(a.id, &bob.email, MemberRole::Developer, alice_member.id)
        .await
        .unwrap();
    store.decline_invitation_of_user(bob.id, invitation.id).await.unwrap();
    assert_eq!(store.find_member(a.id, invitation.id).await.unwrap(), None);
    assert!(
        store
            .list_invitations_of_user(bob.id, Timestamp::now())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.decline_invitation_of_user(bob.id, invitation.id).await,
        Err(StoreError::NotFound)
    );
    assert_eq!(
        store
            .accept_invitation_of_user(bob.id, invitation.id, Timestamp::now())
            .await,
        Err(StoreError::NotFound)
    );
    // Withdrawn by an admin, it's just as gone.
    let again = store
        .invite_member(a.id, &bob.email, MemberRole::Developer, alice_member.id)
        .await
        .unwrap();
    store
        .remove_member(a.id, again.id, alice_member.id, false)
        .await
        .unwrap();
    assert_eq!(
        store
            .accept_invitation_of_user(bob.id, again.id, Timestamp::now())
            .await,
        Err(StoreError::NotFound)
    );
    assert_eq!(store.find_active_membership(a.id, bob.id).await.unwrap(), None);
}

async fn joins_once_when_answers_race(store: &dyn OrganizationStore) {
    let (alice, bob) = (person(store).await, person(store).await);
    let (a, alice_member) = organization(store, &alice).await;
    for _ in 0..10 {
        // Two accepts at once: one joins, the other finds it accepted.
        let invitation = store
            .invite_member(a.id, &bob.email, MemberRole::Developer, alice_member.id)
            .await
            .unwrap();
        let now = Timestamp::now();
        let (first, second) = tokio::join!(
            store.accept_invitation_of_user(bob.id, invitation.id, now),
            store.accept_invitation_of_user(bob.id, invitation.id, now),
        );
        let mut outcomes = [first.map(|_| ()), second.map(|_| ())];
        outcomes.sort_by_key(Result::is_err);
        assert_eq!(outcomes, [Ok(()), Err(StoreError::InvitationAccepted)]);
        store
            .remove_member(a.id, invitation.id, alice_member.id, false)
            .await
            .unwrap();

        // An accept and a decline at once: exactly one of them answers it.
        let invitation = store
            .invite_member(a.id, &bob.email, MemberRole::Developer, alice_member.id)
            .await
            .unwrap();
        let (accepted, declined) = tokio::join!(
            store.accept_invitation_of_user(bob.id, invitation.id, Timestamp::now()),
            store.decline_invitation_of_user(bob.id, invitation.id),
        );
        let member = store.find_member(a.id, invitation.id).await.unwrap();
        match (accepted, declined) {
            (Ok(_), Err(StoreError::InvitationAccepted)) => {
                assert_eq!(member.map(|m| m.status), Some(MemberStatus::Active));
                store
                    .remove_member(a.id, invitation.id, alice_member.id, false)
                    .await
                    .unwrap();
            }
            (Err(StoreError::NotFound), Ok(())) => assert_eq!(member, None),
            other => panic!("an accept and a decline both answered it, or neither: {other:?}"),
        }

        // An accept and a withdrawal at once: bob ends up a member only if the withdrawal found nothing to withdraw.
        let invitation = store
            .invite_member(a.id, &bob.email, MemberRole::Developer, alice_member.id)
            .await
            .unwrap();
        let (accepted, withdrawn) = tokio::join!(
            store.accept_invitation_of_user(bob.id, invitation.id, Timestamp::now()),
            store.remove_member(a.id, invitation.id, alice_member.id, false),
        );
        assert_eq!(withdrawn, Ok(()), "the owner may remove an invitation or a member");
        assert!(matches!(accepted, Ok(_) | Err(StoreError::NotFound)), "{accepted:?}");
        assert_eq!(store.find_active_membership(a.id, bob.id).await.unwrap(), None);

        // An accept and a cancel (an invitation only) at once: exactly one of them answers it.
        let invitation = store
            .invite_member(a.id, &bob.email, MemberRole::Developer, alice_member.id)
            .await
            .unwrap();
        let (accepted, cancelled) = tokio::join!(
            store.accept_invitation_of_user(bob.id, invitation.id, Timestamp::now()),
            store.remove_member(a.id, invitation.id, alice_member.id, true),
        );
        let member = store.find_member(a.id, invitation.id).await.unwrap();
        match (accepted, cancelled) {
            (Ok(_), Err(StoreError::InvitationAccepted)) => {
                assert_eq!(member.map(|m| m.status), Some(MemberStatus::Active));
                store
                    .remove_member(a.id, invitation.id, alice_member.id, false)
                    .await
                    .unwrap();
            }
            (Err(StoreError::NotFound), Ok(())) => assert_eq!(member, None),
            other => panic!("an accept and a cancel both answered it, or neither: {other:?}"),
        }
    }
}

/// `bob`, invited to `a` by `acting` with `role`, who accepts: an active member.
async fn joined(
    store: &dyn OrganizationStore,
    a: &Organization,
    bob: &User,
    role: MemberRole,
    acting: &Member,
) -> Member {
    let invitation = store.invite_member(a.id, &bob.email, role, acting.id).await.unwrap();
    store
        .accept_invitation_of_user(bob.id, invitation.id, Timestamp::now())
        .await
        .unwrap()
        .1
}

/// Whether the invitation still stands: listed for its invitee, and in its organization.
async fn stands(store: &dyn OrganizationStore, a: &Organization, invitee: &User, invitation: &Member) -> bool {
    let listed = store
        .list_invitations_of_user(invitee.id, Timestamp::now())
        .await
        .unwrap()
        .iter()
        .any(|listed| listed.id == invitation.id);
    let found = store.find_member(a.id, invitation.id).await.unwrap().is_some();
    assert_eq!(listed, found, "listed exactly while it stands");
    found
}

async fn grants_only_what_the_grantor_can_still_grant(store: &dyn OrganizationStore) {
    let alice = person(store).await;
    let bob = person(store).await;
    let (carol, dave, erin, frank, gus) = (
        person(store).await,
        person(store).await,
        person(store).await,
        person(store).await,
        person(store).await,
    );
    let (a, alice_member) = organization(store, &alice).await;
    let bob_member = joined(store, &a, &bob, MemberRole::Owner, &alice_member).await;
    let invite = |email: String, role: MemberRole, acting: MemberId| async move {
        store.invite_member(a.id, &email, role, acting).await
    };
    let for_carol = invite(carol.email.clone(), MemberRole::Owner, bob_member.id)
        .await
        .unwrap();
    let for_dave = invite(dave.email.clone(), MemberRole::Admin, bob_member.id)
        .await
        .unwrap();
    let for_erin = invite(erin.email.clone(), MemberRole::Viewer, bob_member.id)
        .await
        .unwrap();
    // Bob's invitations elsewhere are no business of a's.
    let (b, b_bob) = organization(store, &bob).await;
    let in_b = store
        .invite_member(b.id, &gus.email, MemberRole::Owner, b_bob.id)
        .await
        .unwrap();

    // Demoted to admin: bob can't grant owner any more, so carol's invitation goes, at once.
    store
        .update_member_role(a.id, bob_member.id, MemberRole::Admin, alice_member.id)
        .await
        .unwrap();
    assert!(!stands(store, &a, &carol, &for_carol).await);
    assert_eq!(
        store
            .accept_invitation_of_user(carol.id, for_carol.id, Timestamp::now())
            .await,
        Err(StoreError::NotFound)
    );
    assert!(stands(store, &a, &dave, &for_dave).await);
    assert!(stands(store, &a, &erin, &for_erin).await);
    // An admin invites, but never an owner.
    assert_eq!(
        invite(frank.email.clone(), MemberRole::Owner, bob_member.id).await,
        Err(StoreError::NotAllowed)
    );
    let for_frank = invite(frank.email.clone(), MemberRole::Developer, bob_member.id)
        .await
        .unwrap();

    // Changing an invitation's role makes whoever changed it its grantor.
    store
        .update_member_role(a.id, for_erin.id, MemberRole::Developer, alice_member.id)
        .await
        .unwrap();
    let erins = store.list_invitations_of_user(erin.id, Timestamp::now()).await.unwrap();
    assert_eq!(erins[0].invited_by.email, alice.email);

    // Removed: everything bob still granted here goes, nothing anyone else did.
    store
        .remove_member(a.id, bob_member.id, alice_member.id, false)
        .await
        .unwrap();
    assert!(!stands(store, &a, &dave, &for_dave).await);
    assert!(!stands(store, &a, &frank, &for_frank).await);
    assert!(stands(store, &a, &erin, &for_erin).await);
    assert!(stands(store, &b, &gus, &in_b).await, "nor in another organization");
    // Nor can a removed member invite again.
    assert_eq!(
        invite(frank.email.clone(), MemberRole::Viewer, bob_member.id).await,
        Err(StoreError::NotAllowed)
    );
    let (_, member) = store
        .accept_invitation_of_user(erin.id, for_erin.id, Timestamp::now())
        .await
        .unwrap();
    assert_eq!(member.role, MemberRole::Developer);

    // A developer grants nothing; demoted to one, an admin's invitations all go.
    let dave_member = joined(store, &a, &dave, MemberRole::Admin, &alice_member).await;
    let for_frank = invite(frank.email.clone(), MemberRole::Viewer, dave_member.id)
        .await
        .unwrap();
    store
        .update_member_role(a.id, dave_member.id, MemberRole::Developer, alice_member.id)
        .await
        .unwrap();
    assert!(!stands(store, &a, &frank, &for_frank).await);
    assert_eq!(
        invite(frank.email.clone(), MemberRole::Viewer, dave_member.id).await,
        Err(StoreError::NotAllowed)
    );
    // Promoted again, nothing comes back.
    store
        .update_member_role(a.id, dave_member.id, MemberRole::Owner, alice_member.id)
        .await
        .unwrap();
    assert_eq!(store.find_member(a.id, for_frank.id).await.unwrap(), None);
}

async fn cancels_only_an_invitation(store: &dyn OrganizationStore) {
    let (alice, bob, carol) = (person(store).await, person(store).await, person(store).await);
    let (a, alice_member) = organization(store, &alice).await;
    let bob_member = joined(store, &a, &bob, MemberRole::Developer, &alice_member).await;
    assert_eq!(
        store.remove_member(a.id, bob_member.id, alice_member.id, true).await,
        Err(StoreError::InvitationAccepted)
    );
    assert!(store.find_active_membership(a.id, bob.id).await.unwrap().is_some());
    let invitation = store
        .invite_member(a.id, &carol.email, MemberRole::Viewer, alice_member.id)
        .await
        .unwrap();
    store
        .remove_member(a.id, invitation.id, alice_member.id, true)
        .await
        .unwrap();
    assert_eq!(store.find_member(a.id, invitation.id).await.unwrap(), None);
}

async fn lists_the_newest_invitations_and_none_long_expired(store: &dyn OrganizationStore) {
    let (alice, bob) = (person(store).await, person(store).await);
    let (a, alice_member) = organization(store, &alice).await;
    let invitation = store
        .invite_member(a.id, &bob.email, MemberRole::Viewer, alice_member.id)
        .await
        .unwrap();
    let shown_until = invitation.invited_at.millis() + INVITATION_TTL_MS + EXPIRED_INVITATION_SHOWN_MS;
    let listed_at = |millis: i64| async move {
        store
            .list_invitations_of_user(bob.id, Timestamp::from_millis(millis))
            .await
            .unwrap()
            .iter()
            .any(|listed| listed.id == invitation.id)
    };
    assert!(listed_at(shown_until - 1).await, "expired, but recently");
    assert!(!listed_at(shown_until).await, "expired long ago");

    let limit = usize::try_from(INVITATION_LIST_LIMIT).unwrap();
    for _ in 0..limit {
        let owner = person(store).await;
        let (org, owner_member) = organization(store, &owner).await;
        store
            .invite_member(org.id, &bob.email, MemberRole::Viewer, owner_member.id)
            .await
            .unwrap();
    }
    let listed = store.list_invitations_of_user(bob.id, Timestamp::now()).await.unwrap();
    assert_eq!(listed.len(), limit);
    assert!(
        listed.iter().all(|listed| listed.id != invitation.id),
        "the oldest is left out"
    );
}

async fn never_lets_an_accept_outrun_its_grantor(store: &dyn OrganizationStore) {
    let (alice, bob, carol) = (person(store).await, person(store).await, person(store).await);
    let (a, alice_member) = organization(store, &alice).await;
    for _ in 0..10 {
        // Bob, an owner, invites carol as an owner just as alice demotes him.
        let bob_member = joined(store, &a, &bob, MemberRole::Owner, &alice_member).await;
        let invitation = store
            .invite_member(a.id, &carol.email, MemberRole::Owner, bob_member.id)
            .await
            .unwrap();
        let (accepted, demoted) = tokio::join!(
            store.accept_invitation_of_user(carol.id, invitation.id, Timestamp::now()),
            store.update_member_role(a.id, bob_member.id, MemberRole::Admin, alice_member.id),
        );
        assert!(demoted.is_ok(), "{demoted:?}");
        let carol_member = store.find_active_membership(a.id, carol.id).await.unwrap();
        match accepted {
            // Accepted first, while bob could still grant it.
            Ok(_) => assert!(carol_member.is_some()),
            Err(StoreError::NotFound) => {
                assert_eq!(carol_member, None);
                assert_eq!(store.find_member(a.id, invitation.id).await.unwrap(), None);
            }
            other => panic!("{other:?}"),
        }
        if let Some((_, member)) = carol_member {
            store
                .remove_member(a.id, member.id, alice_member.id, false)
                .await
                .unwrap();
        }

        // The same, as alice removes him.
        let invitation = store
            .invite_member(a.id, &carol.email, MemberRole::Viewer, bob_member.id)
            .await
            .unwrap();
        let (accepted, removed) = tokio::join!(
            store.accept_invitation_of_user(carol.id, invitation.id, Timestamp::now()),
            store.remove_member(a.id, bob_member.id, alice_member.id, false),
        );
        assert_eq!(removed, Ok(()));
        let carol_member = store.find_active_membership(a.id, carol.id).await.unwrap();
        match accepted {
            Ok(_) => assert!(carol_member.is_some()),
            Err(StoreError::NotFound) => assert_eq!(carol_member, None),
            other => panic!("{other:?}"),
        }
        if let Some((_, member)) = carol_member {
            store
                .remove_member(a.id, member.id, alice_member.id, false)
                .await
                .unwrap();
        }
    }
}
