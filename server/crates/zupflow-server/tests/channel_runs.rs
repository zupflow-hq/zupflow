//! Channel runs end to end, through the inbound route: a team's messages,
//! signed by each app's own secret, decide which agent answers (a mention,
//! a DM, a follow-up in a thread it was last addressed in), start runs in
//! the thread's session, join a run at its next turn boundary or wait for
//! it, and stop at the conversation's breaker, the sender rules and the
//! agent's own messages. One workspace's app never reaches another's, nor
//! does the same team linked in two workspaces mix them.

#![allow(clippy::unwrap_used)]

#[path = "common/channel_world.rs"]
mod channel_world;
mod common;
#[path = "common/faults.rs"]
mod faults;
#[path = "common/run_setup.rs"]
mod run_setup;

use std::time::Duration;

use axum::http::{Method, StatusCode};
use channel_world::{ANA, BO, Installed, SUPPORT, TEAM, World, world};
use common::{call, code};
use run_setup::{id, setup};
use serde_json::{Value, json};
use zupflow_channels::fake::Poster;
use zupflow_core::channels::handling::INBOUND_JOB;
use zupflow_core::channels::store::{NewConversation, NewMessage, NewPerson};
use zupflow_core::channels::{ConversationKind, Direction};
use zupflow_core::ids::{ChannelConnectionId, ChannelMessageId};
use zupflow_core::scope::Name;

async fn main_world() -> World {
    world(setup(&[("Main", "tk_main")]).await).await
}

/// A new agent in `ws`, as `user`.
async fn agent_in(world: &World, user: &str, org: &str, ws: &str, name: &str) -> String {
    let (status, agent) = world
        .setup
        .kit
        .send(
            call(user, Method::POST, format!("/v1/workspaces/{ws}/agents"))
                .naming(org)
                .body(json!({ "name": name, "description": "", "instructions": "Answer in one line." })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{agent}");
    id(&agent["data"]).to_owned()
}

/// Another agent of the setup's workspace, with its app in [`TEAM`].
async fn billing(world: &World) -> Installed {
    let setup = &world.setup;
    let agent = agent_in(world, "alice", &setup.org, &setup.ws, "Billing").await;
    world
        .install("alice", &setup.org, &setup.ws, &agent, TEAM, "U0BILLING", "billing")
        .await
}

fn reasons(runs: &[Value]) -> Vec<&str> {
    runs.iter()
        .map(|run| run["channel"]["reason"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn answers_a_mention_a_follow_up_in_its_thread_and_a_dm() {
    let world = main_world().await;
    let main = &world.main;
    let asked = world
        .say(ANA, &format!("{} is the build green?", main.mention()), None)
        .await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    let first = &runs[0];
    assert_eq!(first["status"], "completed", "{first}");
    assert_eq!(first["input"], "[U0ANA] @helper is the build green?");
    assert_eq!(first["output"], "You said: [U0ANA] @helper is the build green?");
    assert_eq!(first["channel"]["connection_id"], main.connection.as_str());
    assert_eq!(first["channel"]["thread_key"], asked.ts.as_str());
    assert_eq!(first["channel"]["reason"], "mentioned");
    assert!(first["channel"]["origin_message_id"].is_string());
    assert!(first["channel"]["silent_reason"].is_null());
    let first_id = World::run_id(first);
    assert_eq!(world.taken_in(first_id).await, [("started".to_owned(), 1)]);

    // A reply in the thread, mentioning nobody: the agent was the last one
    // addressed there, so it follows up, in the same session.
    world.say(BO, "and the deploy?", Some(&asked.ts)).await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(reasons(&runs), ["mentioned", "thread_follow_up"]);
    assert_eq!(runs[1]["session_id"], first["session_id"]);
    assert_eq!(runs[1]["input"], "[U0BO] and the deploy?");

    // A reply addressed to someone else wakes nobody, but the next run
    // reads it as context.
    world.say(ANA, "<@U0BO> can you check?", Some(&asked.ts)).await;
    world.work().await;
    assert_eq!(world.runs(main).await.len(), 2);
    world.say(BO, "It's done.", Some(&asked.ts)).await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs.len(), 3);
    assert_eq!(
        runs[2]["input"],
        "[U0ANA, not to you] @U0BO can you check?\n[U0BO] It's done."
    );

    // A DM is its own session, and needs no mention.
    world.post(&[main], "D0ANA", ANA, "hello there", None).await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs.len(), 4);
    assert_eq!(runs[3]["channel"]["reason"], "direct_message");
    assert_eq!(runs[3]["input"], "[U0ANA] hello there");
    assert_ne!(runs[3]["session_id"], first["session_id"]);
}

#[tokio::test]
async fn follows_up_only_for_the_agent_last_addressed_in_the_thread() {
    let world = main_world().await;
    let main = world.main.clone();
    let billing = billing(&world).await;
    let both = [&main, &billing];
    // Each app gets the delivery, as the provider sends it to every app in
    // the conversation.
    let asked = world
        .post(
            &both,
            SUPPORT,
            ANA,
            &format!("{} why was I charged twice?", billing.mention()),
            None,
        )
        .await;
    world.work().await;
    assert_eq!(world.runs(&main).await.len(), 0);
    assert_eq!(reasons(&world.runs(&billing).await), ["mentioned"]);
    world
        .post(&both, SUPPORT, ANA, "it was on Monday", Some(&asked.ts))
        .await;
    world.work().await;
    assert_eq!(world.runs(&main).await.len(), 0, "never addressed in the thread");
    assert_eq!(reasons(&world.runs(&billing).await), ["mentioned", "thread_follow_up"]);

    // Now the other agent is addressed: it alone follows up from here.
    world
        .post(
            &both,
            SUPPORT,
            BO,
            &format!("{} can you look too?", main.mention()),
            Some(&asked.ts),
        )
        .await;
    world.work().await;
    world.post(&both, SUPPORT, ANA, "thanks both", Some(&asked.ts)).await;
    world.work().await;
    assert_eq!(reasons(&world.runs(&main).await), ["mentioned", "thread_follow_up"]);
    assert_eq!(reasons(&world.runs(&billing).await), ["mentioned", "thread_follow_up"]);

    // With follow-ups off, only a mention wakes it.
    world.configure(&main, json!({ "thread_follow_up": "off" })).await;
    world.post(&both, SUPPORT, ANA, "one more thing", Some(&asked.ts)).await;
    world.work().await;
    assert_eq!(world.runs(&main).await.len(), 2);
}

#[tokio::test]
async fn steers_a_message_into_a_run_paused_for_approval_at_its_next_turn() {
    let world = main_world().await;
    let main = &world.main;
    let send = world.setup.calling("send", r#"{"to":"ops"}"#).await;
    let asked = world
        .say(ANA, &format!("{} tell ops\n{send}", main.mention()), None)
        .await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs[0]["status"], "waiting_for_approval", "{runs:?}");
    let run = World::run_id(&runs[0]);

    // Arrives while it waits: it waits for the run's next turn.
    world.say(BO, "also cc finance", Some(&asked.ts)).await;
    world.handle().await;
    assert_eq!(world.runs(main).await.len(), 1);
    world.approve(run).await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs.len(), 1, "taken in, not left for another run: {runs:?}");
    assert_eq!(runs[0]["status"], "completed", "{}", runs[0]);
    // The model read it as the turn after the call's result.
    assert_eq!(
        runs[0]["output"],
        "You said: New in the thread while you worked:\n[U0BO] also cc finance"
    );
    assert_eq!(
        world.taken_in(run).await,
        [("started".to_owned(), 1), ("steered".to_owned(), 1)]
    );
}

#[tokio::test]
async fn collects_what_arrives_mid_run_into_one_run_after_it() {
    let world = main_world().await;
    let main = &world.main;
    world.configure(main, json!({ "mid_run": "collect" })).await;
    let send = world.setup.calling("send", r#"{"to":"ops"}"#).await;
    let asked = world
        .say(ANA, &format!("{} tell ops\n{send}", main.mention()), None)
        .await;
    world.work().await;
    let first = World::run_id(&world.runs(main).await[0]);
    world.say(BO, "also cc finance", Some(&asked.ts)).await;
    world.say(ANA, "and legal", Some(&asked.ts)).await;
    world.handle().await;
    world.approve(first).await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(runs[0]["status"], "completed");
    assert_eq!(world.taken_in(first).await, [("started".to_owned(), 1)]);
    let collected = &runs[1];
    assert_eq!(collected["status"], "completed", "{collected}");
    assert_eq!(collected["session_id"], runs[0]["session_id"]);
    assert_eq!(collected["input"], "[U0BO] also cc finance\n[U0ANA] and legal");
    assert_eq!(
        world.taken_in(World::run_id(collected)).await,
        [("collected".to_owned(), 2)]
    );
}

#[tokio::test]
async fn pauses_agents_in_a_conversation_once_its_breaker_trips() {
    let world = main_world().await;
    let main = &world.main;
    let (status, body) = world
        .setup
        .kit
        .send(
            call(
                "alice",
                Method::PUT,
                format!("/v1/organizations/{}/channel-settings", world.setup.org),
            )
            .body(json!({
                "breaker_runs": 2, "breaker_window_minutes": 60, "breaker_pause_minutes": 30,
                "message_retention_days": 90,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for n in 0..4 {
        world.say(ANA, &format!("{} question {n}", main.mention()), None).await;
    }
    world.handle().await;
    assert_eq!(
        world.runs(main).await.len(),
        2,
        "the third trips it, the fourth finds it paused"
    );
    let support = world
        .conversations(main)
        .await
        .into_iter()
        .find(|conversation| conversation["external_id"] == SUPPORT)
        .unwrap();
    assert!(support["paused_until"].is_string(), "{support}");
    // Elsewhere, the agent still answers.
    world.post(&[main], "D0ANA", ANA, "hello", None).await;
    world.handle().await;
    assert_eq!(world.runs(main).await.len(), 3);
}

#[tokio::test]
async fn admits_only_the_senders_its_rules_allow() {
    let world = main_world().await;
    let main = &world.main;
    let (org, ws) = main.scope();
    let connection = ChannelConnectionId::parse(&main.connection).unwrap();
    // The provider's lookup knows Ana's email.
    world
        .setup
        .kit
        .state(faults::Worker::A)
        .channels
        .record_message(
            org,
            ws,
            connection,
            &NewMessage {
                id: ChannelMessageId::generate(),
                conversation: NewConversation {
                    external_id: "C0OTHER".to_owned(),
                    kind: ConversationKind::Channel,
                    name: Some("other".to_owned()),
                    is_external: false,
                    identity_is_member: true,
                },
                direction: Direction::Inbound,
                author: Some(NewPerson {
                    external_id: ANA.to_owned(),
                    handle: Some("ana".to_owned()),
                    display_name: Some("Ana".to_owned()),
                    email: Some("ana@northwind.example".to_owned()),
                    is_automated: false,
                    team_id: Some(TEAM.to_owned()),
                }),
                external_id: "1000000000.000001".to_owned(),
                thread_key: "1000000000.000001".to_owned(),
                text: "earlier".to_owned(),
                mentions: Vec::new(),
                run_id: None,
                tool_call_id: None,
                agent_hop: 0,
                sent_at: world.now(),
            },
            world.now(),
        )
        .await
        .unwrap();

    // An app posting automatically is refused until the agent allows them.
    let ci = world.chat.post_automated(
        SUPPORT,
        "channel",
        "U0CI",
        &format!("{} build failed", main.mention()),
        None,
        world.now(),
    );
    world.deliver(main, &world.chat.delivery(&ci)).await;
    world.work().await;
    assert_eq!(world.runs(main).await.len(), 0);
    world.configure(main, json!({ "allow_automated": true })).await;
    let ci = world.chat.post_automated(
        SUPPORT,
        "channel",
        "U0CI",
        &format!("{} build fixed", main.mention()),
        None,
        world.now(),
    );
    world.deliver(main, &world.chat.delivery(&ci)).await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["input"], "[U0CI, an app] @helper build fixed");

    // Listed senders only, matched by email in any case.
    world
        .configure(
            main,
            json!({ "senders": "listed", "sender_list": ["Ana@Northwind.example"] }),
        )
        .await;
    world.say(BO, &format!("{} let me in", main.mention()), None).await;
    world.work().await;
    assert_eq!(world.runs(main).await.len(), 1, "Bo isn't listed");
    world.say(ANA, &format!("{} it's Ana", main.mention()), None).await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1]["input"], "[Ana] @helper it's Ana");
}

#[tokio::test]
async fn ignores_its_own_messages_and_a_delivery_made_twice() {
    let world = main_world().await;
    let main = &world.main;
    // Its own message, even one mentioning itself, is recorded only.
    world
        .say(&main.identity, &format!("{} noted", main.mention()), None)
        .await;
    world.work().await;
    assert_eq!(world.runs(main).await.len(), 0);

    // The provider retries a delivery, with the same event ID and with a
    // new one: one message, one run.
    let asked = world.chat.post(
        SUPPORT,
        "channel",
        ANA,
        &format!("{} ping", main.mention()),
        None,
        world.now(),
    );
    let delivery = world.chat.delivery(&asked);
    world.deliver(main, &delivery).await;
    world.deliver(main, &delivery).await;
    world.deliver(main, &world.chat.delivery(&asked)).await;
    world.work().await;
    assert_eq!(world.runs(main).await.len(), 1);
    let support = world
        .conversations(main)
        .await
        .into_iter()
        .find(|conversation| conversation["external_id"] == SUPPORT)
        .unwrap();
    let (status, messages) = world
        .setup
        .kit
        .send(
            call(
                "alice",
                Method::GET,
                format!(
                    "/v1/workspaces/{}/channel-connections/{}/conversations/{}/messages",
                    main.ws,
                    main.connection,
                    id(&support)
                ),
            )
            .naming(&main.org),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{messages}");
    let directions: Vec<&str> = messages["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["direction"].as_str().unwrap())
        .collect();
    assert_eq!(directions, ["inbound", "outbound"]);
}

#[tokio::test]
async fn keeps_each_workspaces_events_and_threads_its_own() {
    let world = main_world().await;
    let main = world.main.clone();
    // A sibling workspace's agent, its app in another team.
    let elsewhere = world.setup.sibling("gamma", "tk_gamma").await;
    let beta = world
        .install(
            "alice",
            &elsewhere.org,
            &elsewhere.ws,
            &elsewhere.agent,
            "T0BETA",
            "U0BETA",
            "beta",
        )
        .await;
    let asked = world.chat.post(
        SUPPORT,
        "channel",
        ANA,
        &format!("{} hi", main.mention()),
        None,
        world.now(),
    );
    let delivery = world.chat.delivery(&asked);
    // Signed by the other app, or sent to it: nothing of it is read.
    let (status, refused) = world.deliver_signed(&main, &delivery, Some(&beta.signing_secret)).await;
    assert_eq!(
        (status, code(&refused)),
        (StatusCode::UNAUTHORIZED, "invalid_signature")
    );
    world.deliver(&beta, &delivery).await;
    world.work().await;
    assert!(world.conversations(&beta).await.is_empty(), "another team's event");
    assert!(world.conversations(&main).await.is_empty());
    assert!(world.runs(&beta).await.is_empty() && world.runs(&main).await.is_empty());

    // The same team linked in both workspaces: each reads the thread, and
    // each answers only what's addressed to it there.
    let sibling = world.setup.sibling("beta", "tk_beta").await;
    let shared = world
        .install(
            "alice",
            &sibling.org,
            &sibling.ws,
            &sibling.agent,
            TEAM,
            "U0SHARED",
            "shared",
        )
        .await;
    let both = [&main, &shared];
    let asked = world
        .post(&both, SUPPORT, ANA, &format!("{} hi", main.mention()), None)
        .await;
    world.work().await;
    assert_eq!(world.runs(&main).await.len(), 1);
    assert!(world.runs(&shared).await.is_empty());
    assert_eq!(world.conversations(&shared).await.len(), 1, "it saw the message");
    world
        .post(
            &both,
            SUPPORT,
            BO,
            &format!("{} and you?", shared.mention()),
            Some(&asked.ts),
        )
        .await;
    world.work().await;
    // Each workspace's last addressed agent is its own: both follow up.
    world.post(&both, SUPPORT, ANA, "thanks", Some(&asked.ts)).await;
    world.work().await;
    assert_eq!(reasons(&world.runs(&main).await), ["mentioned", "thread_follow_up"]);
    assert_eq!(reasons(&world.runs(&shared).await), ["mentioned", "thread_follow_up"]);
    for run in world.runs(&shared).await {
        assert_eq!(run["workspace_id"], sibling.ws.as_str());
        assert_eq!(run["channel"]["connection_id"], shared.connection.as_str());
    }
    // And another organization's app, in the same team, sees nothing it
    // isn't sent.
    let other = world.setup.other_organization("bob", "larkspur", "tk_other").await;
    let outside = world
        .install("bob", &other.org, &other.ws, &other.agent, TEAM, "U0OUTSIDE", "outside")
        .await;
    world
        .say(ANA, &format!("{} last one", outside.mention()), Some(&asked.ts))
        .await;
    world.work().await;
    assert!(world.conversations(&outside).await.is_empty());
    assert!(world.runs(&outside).await.is_empty());
}

/// The conversation `external_id` as `installed`'s connection reads it.
async fn conversation(world: &World, installed: &Installed, external_id: &str) -> Value {
    world
        .conversations(installed)
        .await
        .into_iter()
        .find(|conversation| conversation["external_id"] == external_id)
        .unwrap()
}

#[tokio::test]
async fn answers_people_of_another_organization_only_when_it_opts_in() {
    let world = main_world().await;
    let main = &world.main;
    // Someone of another organization's team, in a channel shared with it.
    let guest = Poster::external("U0GUEST", "T0PARTNER");
    world
        .post_as(
            &[main],
            SUPPORT,
            &guest,
            &format!("{} hello from outside", main.mention()),
            None,
        )
        .await;
    world.work().await;
    assert!(world.runs(main).await.is_empty(), "another organization's person");
    assert_eq!(conversation(&world, main, SUPPORT).await["is_external"], true);
    // The team's own people still wake it there.
    world.say(ANA, &format!("{} and from us", main.mention()), None).await;
    world.work().await;
    assert_eq!(world.runs(main).await.len(), 1);

    world.configure(main, json!({ "allow_external": true })).await;
    world
        .post_as(&[main], SUPPORT, &guest, &format!("{} again", main.mention()), None)
        .await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1]["input"], "[U0GUEST] @helper again");
}

#[tokio::test]
async fn knows_its_own_posts_by_either_id_and_another_workspaces_agent_as_an_app() {
    let world = main_world().await;
    let main = world.main.clone();
    world.configure(&main, json!({ "allow_automated": true })).await;
    // Its own post, signed with a persona: Slack sends its bot ID alone.
    let persona = Poster::app_without_user(main.bot_id.as_deref().unwrap());
    world
        .post_as(&[&main], SUPPORT, &persona, &format!("{} noted", main.mention()), None)
        .await;
    world.work().await;
    assert!(
        world.runs(&main).await.is_empty(),
        "its own post, though apps may wake it"
    );
    assert_eq!(world.messages_in(&main, SUPPORT).await[0]["direction"], "outbound");

    // An install that reports no bot ID learns it from its first post.
    let billing_agent = agent_in(&world, "alice", &world.setup.org, &world.setup.ws, "Billing").await;
    let billing = world
        .install_reporting(
            "alice",
            &world.setup.org,
            &world.setup.ws,
            &billing_agent,
            TEAM,
            "U0BILLING",
            "billing",
            None,
        )
        .await;
    world.configure(&billing, json!({ "allow_automated": true })).await;
    world
        .post_as(&[&billing], "C0BILLING", &billing.poster(), "invoice sent", None)
        .await;
    let alone = Poster::app_without_user(&channel_world::bot_id_of(&billing.identity));
    world
        .post_as(
            &[&billing],
            "C0BILLING",
            &alone,
            &format!("{} noted", billing.mention()),
            None,
        )
        .await;
    world.work().await;
    assert!(world.runs(&billing).await.is_empty());
    let directions: Vec<Value> = world
        .messages_in(&billing, "C0BILLING")
        .await
        .iter()
        .map(|message| message["direction"].clone())
        .collect();
    assert_eq!(directions, [json!("outbound"), json!("outbound")]);

    // Another workspace's agent in the same team: its posts are an app's,
    // which it doesn't answer by default, so two agents don't loop.
    let sibling = world.setup.sibling("beta", "tk_beta").await;
    let shared = world
        .install(
            "alice",
            &sibling.org,
            &sibling.ws,
            &sibling.agent,
            TEAM,
            "U0SHARED",
            "shared",
        )
        .await;
    let both = [&main, &shared];
    let asked = world
        .post_as(
            &both,
            SUPPORT,
            &main.poster(),
            &format!("{} can you check?", shared.mention()),
            None,
        )
        .await;
    world.work().await;
    assert!(world.runs(&shared).await.is_empty(), "an app's post");
    assert!(world.runs(&main).await.is_empty(), "its own post");
    world
        .post_as(
            &both,
            SUPPORT,
            &shared.poster(),
            &format!("{} checked", main.mention()),
            Some(&asked.ts),
        )
        .await;
    world.work().await;
    let runs = world.runs(&main).await;
    assert_eq!(runs.len(), 1, "main allows apps");
    assert_eq!(
        runs[0]["input"],
        "[You, earlier] @U0SHARED can you check?\n[U0SHARED, an app] @helper checked"
    );
    world
        .post_as(
            &both,
            SUPPORT,
            &main.poster(),
            &format!("{} thanks", shared.mention()),
            Some(&asked.ts),
        )
        .await;
    world.work().await;
    assert!(world.runs(&shared).await.is_empty());
    assert_eq!(world.runs(&main).await.len(), 1);
}

#[tokio::test]
async fn wakes_another_agent_once_its_hops_are_known_and_only_under_the_limit() {
    let world = main_world().await;
    let main = world.main.clone();
    let billing = billing(&world).await;
    world
        .configure(&billing, json!({ "agent_messages": "mentions", "max_agent_hops": 2 }))
        .await;
    world
        .say(ANA, &format!("{} what do we owe?", main.mention()), None)
        .await;
    world.work().await;
    let run = World::run_id(&world.runs(&main).await[0]);
    let both = [&main, &billing];
    let clock = &world.setup.kit.clock;

    // Main's post reaches billing before main's send is recorded: its hops
    // aren't known, so billing's job waits for them.
    let asked = world
        .post_as(
            &both,
            SUPPORT,
            &main.poster(),
            &format!("{} please check", billing.mention()),
            None,
        )
        .await;
    world.handle().await;
    assert!(world.runs(&billing).await.is_empty(), "its hops aren't known yet");
    world.sent(&main, &asked, run, 1).await;
    clock.advance(Duration::from_secs(10));
    world.handle().await;
    let runs = world.runs(&billing).await;
    assert_eq!(reasons(&runs), ["mentioned"]);
    assert_eq!(runs[0]["input"], "[helper, an agent] @billing please check");

    // A message at the limit wakes no one; the higher of a send's hops holds.
    let deeper = world
        .post_as(
            &both,
            SUPPORT,
            &main.poster(),
            &format!("{} one more", billing.mention()),
            None,
        )
        .await;
    world.sent(&main, &deeper, run, 2).await;
    world.sent(&main, &deeper, run, 1).await;
    world.handle().await;
    assert_eq!(world.runs(&billing).await.len(), 1, "2 hops, at the limit of 2");
    let recorded = world
        .messages_in(&main, SUPPORT)
        .await
        .into_iter()
        .find(|message| message["external_id"] == deeper.ts.as_str())
        .unwrap();
    assert_eq!(
        (recorded["agent_hop"].clone(), recorded["direction"].clone()),
        (json!(2), json!("outbound"))
    );

    // A post whose send is never recorded wakes no one, after its attempts.
    world
        .post_as(
            &[&billing],
            SUPPORT,
            &main.poster(),
            &format!("{} lost", billing.mention()),
            None,
        )
        .await;
    for _ in 0..6 {
        world.handle().await;
        clock.advance(Duration::from_secs(30));
    }
    world.handle().await;
    assert_eq!(world.runs(&billing).await.len(), 1);
    let state = world.setup.kit.state(faults::Worker::A);
    assert!(!zupflow_server::channel_inbound::work_once(&state).await, "settled");
}

#[tokio::test]
async fn reads_each_message_once_when_jobs_finish_out_of_order() {
    let world = main_world().await;
    let main = &world.main;
    let clock = &world.setup.kit.clock;
    let first = world
        .say(ANA, &format!("{} first question", main.mention()), None)
        .await;
    clock.advance(Duration::from_millis(5));
    world
        .say(BO, &format!("{} second question", main.mention()), Some(&first.ts))
        .await;
    // Two workers claim the jobs in order, and the second finishes first.
    let state = world.setup.kit.state(faults::Worker::A);
    let kind = Name::new(INBOUND_JOB).unwrap();
    let mut claimed = Vec::new();
    while let Some(job) = state
        .jobs
        .claim_next(std::slice::from_ref(&kind), Duration::from_secs(60))
        .await
        .unwrap()
    {
        claimed.push(job);
    }
    assert_eq!(claimed.len(), 2);
    for job in claimed.into_iter().rev() {
        zupflow_server::channel_inbound::handle(&state, job).await;
    }
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs.len(), 1, "the first's late job finds it read: {runs:?}");
    assert_eq!(
        runs[0]["input"], "[U0ANA] @helper first question\n[U0BO] @helper second question",
        "each once, and each to the agent by what it says"
    );
    // The session's next turn reads neither again.
    world.say(ANA, "and a third", Some(&first.ts)).await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(reasons(&runs), ["mentioned", "thread_follow_up"]);
    assert_eq!(runs[1]["input"], "[U0ANA] and a third");
}

#[tokio::test]
async fn counts_the_runs_that_start_against_the_breaker_not_the_messages() {
    let world = main_world().await;
    let main = &world.main;
    let (status, body) = world
        .setup
        .kit
        .send(
            call(
                "alice",
                Method::PUT,
                format!("/v1/organizations/{}/channel-settings", world.setup.org),
            )
            .body(json!({
                "breaker_runs": 2, "breaker_window_minutes": 60, "breaker_pause_minutes": 30,
                "message_retention_days": 90,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    world.configure(main, json!({ "mid_run": "collect" })).await;
    let send = world.setup.calling("send", r#"{"to":"ops"}"#).await;
    let asked = world
        .say(ANA, &format!("{} tell ops\n{send}", main.mention()), None)
        .await;
    world.work().await;
    let first = World::run_id(&world.runs(main).await[0]);
    for text in ["one", "two", "three"] {
        world
            .say(BO, &format!("{} {text}", main.mention()), Some(&asked.ts))
            .await;
    }
    world.handle().await;
    world.approve(first).await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(
        world.taken_in(World::run_id(&runs[1])).await,
        [("collected".to_owned(), 3)]
    );
    assert!(
        conversation(&world, main, SUPPORT).await["paused_until"].is_null(),
        "two runs, within the breaker"
    );
    // The third run goes over it.
    world
        .say(ANA, &format!("{} something else", main.mention()), None)
        .await;
    world.work().await;
    assert_eq!(world.runs(main).await.len(), 2);
    assert!(conversation(&world, main, SUPPORT).await["paused_until"].is_string());
}

#[tokio::test]
async fn answers_the_url_check_for_an_app_whose_secret_is_stored() {
    let world = main_world().await;
    let setup = &world.setup;
    let agent = agent_in(&world, "alice", &setup.org, &setup.ws, "Billing").await;
    let made = world.make_app("alice", &setup.org, &setup.ws, &agent).await;
    let check = json!({ "type": "url_verification", "token": "legacy", "challenge": "c-3f9a" });
    // Awaiting install, and still being made with its secret stored, as the
    // provider checks the URL once it's set on the app.
    for status in ["awaiting_install", "creating"] {
        world.set_app_status(&setup.org, &setup.ws, &made.app, status).await;
        let (code_, answer) = world.deliver_to(&made.inbound_ref, &check, &made.signing_secret).await;
        assert_eq!(
            (code_, answer),
            (StatusCode::OK, json!({ "challenge": "c-3f9a" })),
            "{status}"
        );
        let (code_, refused) = world.deliver_to(&made.inbound_ref, &check, b"not-its-secret").await;
        assert_eq!(
            (code_, code(&refused)),
            (StatusCode::UNAUTHORIZED, "invalid_signature"),
            "{status}"
        );
    }
}

#[tokio::test]
async fn reads_a_provider_event_up_to_a_mebibyte() {
    let world = main_world().await;
    let main = &world.main;
    let event = |text: String| {
        json!({
            "type": "event_callback", "event_id": format!("Ev{}", text.len()), "team_id": TEAM,
            "event": {
                "type": "message", "channel": SUPPORT, "channel_type": "channel", "user": ANA,
                "text": "a long one", "ts": "1800000000.000001",
                "blocks": [{ "type": "section", "text": { "type": "mrkdwn", "text": text } }],
            },
        })
    };
    // More than an API request may be, within what Slack sends.
    let (status, answer) = world.deliver_signed(main, &event("x".repeat(300 << 10)), None).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    let (status, refused) = world.deliver_signed(main, &event("x".repeat(1 << 20)), None).await;
    assert_eq!(
        (status, code(&refused)),
        (StatusCode::PAYLOAD_TOO_LARGE, "body_too_large")
    );
}

/// `installed`'s connection learns conversation `external_id` by `name`,
/// as it does from a message there.
async fn name_conversation(world: &World, installed: &Installed, external_id: &str, name: &str) {
    let (org, ws) = installed.scope();
    world
        .setup
        .kit
        .state(faults::Worker::A)
        .channels
        .record_message(
            org,
            ws,
            ChannelConnectionId::parse(&installed.connection).unwrap(),
            &NewMessage {
                id: ChannelMessageId::generate(),
                conversation: NewConversation {
                    external_id: external_id.to_owned(),
                    kind: ConversationKind::Channel,
                    name: Some(name.to_owned()),
                    is_external: false,
                    identity_is_member: true,
                },
                direction: Direction::Inbound,
                author: Some(NewPerson {
                    external_id: "U0CAROL".to_owned(),
                    handle: None,
                    display_name: None,
                    email: None,
                    is_automated: false,
                    team_id: Some(TEAM.to_owned()),
                }),
                external_id: "1000000000.000009".to_owned(),
                thread_key: "1000000000.000009".to_owned(),
                text: "earlier".to_owned(),
                mentions: Vec::new(),
                run_id: None,
                tool_call_id: None,
                agent_hop: 0,
                sent_at: world.now(),
            },
            world.now(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn names_a_mentioned_channel_as_its_own_workspace_knows_it() {
    let world = main_world().await;
    let main = world.main.clone();
    // The same team linked in a sibling workspace, which knows other names.
    let sibling = world.setup.sibling("beta", "tk_beta").await;
    let shared = world
        .install(
            "alice",
            &sibling.org,
            &sibling.ws,
            &sibling.agent,
            TEAM,
            "U0SHARED",
            "shared",
        )
        .await;
    name_conversation(&world, &main, "C0OTHER", "other").await;
    name_conversation(&world, &shared, "C0LAUNCH", "launch-plans").await;
    let both = [&main, &shared];
    let channels = "see <#C0OTHER>, <#C0LAUNCH|old> and <#C0NEW>";
    world
        .post(&both, SUPPORT, ANA, &format!("{} {channels}", main.mention()), None)
        .await;
    world
        .post(&both, SUPPORT, ANA, &format!("{} {channels}", shared.mention()), None)
        .await;
    world.work().await;
    let runs = world.runs(&main).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["input"], "[U0ANA] @helper see #other, #old and #C0NEW");
    let runs = world.runs(&shared).await;
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(
        runs[0]["input"],
        "[U0ANA] @shared see #C0OTHER, #launch-plans and #C0NEW"
    );
}

// --- Channel tools ---------------------------------------------------------

/// `installed`'s agent's runs' calls, in the order made.
async fn calls_of(world: &World, run: &Value) -> Vec<faults::RecordedCall> {
    world.setup.calls(World::run_id(run)).await
}

#[tokio::test]
async fn replies_with_its_tool_once_and_shows_it_is_working() {
    let world = main_world().await;
    let main = &world.main;
    // Without the tool, its answer is its reply, posted by the reply job.
    let asked = world
        .say(ANA, &format!("{} is the build green?", main.mention()), None)
        .await;
    world.work().await;
    world.reply().await;
    let posted = world.chat.posted_by_apps();
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert!(posted[0].text.starts_with("You said: "), "{}", posted[0].text);
    assert_eq!(posted[0].thread_ts.as_deref(), Some(asked.ts.as_str()));
    assert_eq!(posted[0].user.as_deref(), Some(main.identity.as_str()));
    // It showed it was working while it asked its model, then stopped.
    assert_eq!(
        world.chat.working(),
        [
            (SUPPORT.to_owned(), asked.ts.clone(), true),
            (SUPPORT.to_owned(), asked.ts.clone(), false)
        ]
    );

    // With it, the tool's post is the answer: nothing is posted again.
    // A new thread, so its session offers the tool.
    world.grant(main, &["reply"]).await;
    let reply = world.calling(main, "reply", json!({ "text": "On it." })).await;
    let second = world
        .say(ANA, &format!("{} check the deploy\n{reply}", main.mention()), None)
        .await;
    world.work().await;
    world.reply().await;
    let runs = world.runs(main).await;
    assert_eq!(runs[1]["status"], "completed", "{}", runs[1]);
    let posted = world.chat.posted_by_apps();
    assert_eq!(posted.len(), 2, "the tool's post, and no answer after it: {posted:?}");
    assert_eq!(posted[1].text, "On it.");
    assert_eq!(posted[1].thread_ts.as_deref(), Some(second.ts.as_str()));
    let calls = calls_of(&world, &runs[1]).await;
    assert_eq!(calls.len(), 1);
    assert_eq!(posted[1].send_id.as_deref(), Some(calls[0].id.to_string().as_str()));
    // Recorded as the connection's own message, from the call, one hop on
    // from the person who asked.
    let messages = world.messages_in(main, SUPPORT).await;
    let own = messages
        .iter()
        .find(|message| message["external_id"] == posted[1].ts.as_str())
        .unwrap();
    assert_eq!(own["direction"], "outbound");
    assert_eq!(own["run_id"], id(&runs[1]));
    assert_eq!(own["tool_call_id"], calls[0].id.to_string().as_str());
    assert_eq!(own["agent_hop"], 1);
}

#[tokio::test]
async fn posts_once_when_the_answer_to_its_post_is_lost() {
    let world = main_world().await;
    let main = &world.main;
    world.grant(main, &["reply"]).await;
    let reply = world.calling(main, "reply", json!({ "text": "Looking." })).await;
    world.chat.lose_answers(1);
    world
        .say(ANA, &format!("{} anything new?\n{reply}", main.mention()), None)
        .await;
    world.work().await;
    world.reply().await;
    let runs = world.runs(main).await;
    assert_eq!(runs[0]["status"], "completed", "{}", runs[0]);
    let calls = calls_of(&world, &runs[0]).await;
    assert_eq!(calls[0].state, zupflow_core::runs::CallState::Completed);
    // Found by the call's ID, not posted twice.
    let posted = world.chat.posted_by_apps();
    assert_eq!(posted.len(), 1, "{posted:?}");
    let own: Vec<Value> = world
        .messages_in(main, SUPPORT)
        .await
        .into_iter()
        .filter(|message| message["direction"] == "outbound")
        .collect();
    assert_eq!(own.len(), 1);
    assert_eq!(own[0]["external_id"], posted[0].ts.as_str());
}

#[tokio::test]
async fn stays_silent_with_a_reason_and_posts_nothing() {
    let world = main_world().await;
    let main = &world.main;
    // Offered in every channel session, granted or not.
    let silent = "call stay_silent {\"reason\":\"Ana answered it herself.\"}";
    world
        .say(ANA, &format!("{} never mind\n{silent}", main.mention()), None)
        .await;
    world.work().await;
    world.reply().await;
    let runs = world.runs(main).await;
    assert_eq!(runs[0]["status"], "completed", "{}", runs[0]);
    assert_eq!(runs[0]["channel"]["silent_reason"], "Ana answered it herself.");
    assert!(world.chat.posted_by_apps().is_empty());
    // An app tool of a session is never offered as a channel one, nor the
    // other way round: the model's names are all it has.
    let calls = calls_of(&world, &runs[0]).await;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].model_tool_name, "stay_silent");
}

#[tokio::test]
async fn refuses_what_is_outside_its_audience() {
    let world = main_world().await;
    let main = &world.main;
    world
        .grant(main, &["send_message", "edit_message", "react", "reply"])
        .await;
    // Another conversation the connection knows, and a message in it.
    let general = world.post(&[main], "C0GENERAL", BO, "lunch?", None).await;
    world.work().await;
    let general_id = world.conversation_id(main, "C0GENERAL").await;
    let lunch = world.message_id(main, &general).await;

    let asked = world.say(ANA, &format!("{} hi", main.mention()), None).await;
    world.work().await;
    let asked_id = world.message_id(main, &asked).await;
    let elsewhere = world
        .calling(
            main,
            "send_message",
            json!({ "conversation": general_id, "text": "hello all" }),
        )
        .await;
    let edit = world
        .calling(main, "edit_message", json!({ "message": asked_id, "text": "changed" }))
        .await;
    let react = world
        .calling(main, "react", json!({ "message": lunch, "emoji": "eyes" }))
        .await;
    let unknown = world
        .calling(
            main,
            "send_message",
            json!({ "conversation": "conv_0000000000000000000000", "text": "hi" }),
        )
        .await;
    world
        .say(
            ANA,
            &format!("try these\n{elsewhere}\n{edit}\n{react}\n{unknown}"),
            Some(&asked.ts),
        )
        .await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1]["status"], "completed", "{}", runs[1]);
    let calls = calls_of(&world, &runs[1]).await;
    assert_eq!(calls.len(), 4);
    for call in &calls {
        assert_eq!(call.state, zupflow_core::runs::CallState::Refused, "{call:?}");
        let decision = call.decision.as_ref().unwrap();
        assert_eq!(decision.rule, "outside_audience", "{call:?}");
        assert_eq!(decision.policy, "channel");
    }
    let output = runs[1]["output"].as_str().unwrap();
    assert!(
        output.contains("You can only post in the Slack conversation this run started in."),
        "{output}"
    );
    assert!(
        output.contains("You can only edit or delete messages you posted."),
        "{output}"
    );
    assert!(output.contains("isn't one this connection knows"), "{output}");
    assert!(world.chat.reactions().is_empty());

    // Allowed anywhere it's a member: the reaction is made.
    world.configure(main, json!({ "audience": "anywhere" })).await;
    world.say(ANA, &format!("again\n{react}"), Some(&asked.ts)).await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs[2]["status"], "completed", "{}", runs[2]);
    let reactions = world.chat.reactions();
    assert_eq!(reactions.len(), 1, "{reactions:?}");
    assert_eq!(
        (
            reactions[0].channel.as_str(),
            reactions[0].ts.as_str(),
            reactions[0].emoji.as_str()
        ),
        ("C0GENERAL", general.ts.as_str(), "eyes")
    );
}

#[tokio::test]
async fn sends_elsewhere_only_once_approved_and_still_allowed() {
    let world = main_world().await;
    let main = &world.main;
    world.grant(main, &["send_message"]).await;
    world.configure(main, json!({ "audience": "anywhere" })).await;
    world.post(&[main], "C0GENERAL", BO, "lunch?", None).await;
    world.work().await;
    let general_id = world.conversation_id(main, "C0GENERAL").await;
    let send = world
        .calling(
            main,
            "send_message",
            json!({ "conversation": general_id, "text": "Deploy at 5." }),
        )
        .await;
    // The organization's level asks for approval before a send elsewhere.
    let asked = world
        .say(ANA, &format!("{} tell everyone\n{send}", main.mention()), None)
        .await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs[0]["status"], "waiting_for_approval", "{}", runs[0]);
    assert!(world.chat.posted_by_apps().is_empty());
    world.approve(World::run_id(&runs[0])).await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs[0]["status"], "completed", "{}", runs[0]);
    let posted = world.chat.posted_by_apps();
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert_eq!(
        (
            posted[0].channel.as_str(),
            posted[0].thread_ts.as_deref(),
            posted[0].text.as_str()
        ),
        ("C0GENERAL", None, "Deploy at 5.")
    );

    // Approved, then no longer allowed there: checked again, never sent.
    world.say(ANA, &format!("once more\n{send}"), Some(&asked.ts)).await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs[1]["status"], "waiting_for_approval", "{}", runs[1]);
    world.configure(main, json!({ "audience": "origin" })).await;
    world.approve(World::run_id(&runs[1])).await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs[1]["status"], "completed", "{}", runs[1]);
    let calls = calls_of(&world, &runs[1]).await;
    assert_eq!(
        calls[0].failure.as_ref().unwrap().code,
        "outside_audience",
        "{:?}",
        calls[0]
    );
    assert_eq!(world.chat.posted_by_apps().len(), 1);
}

#[tokio::test]
async fn notes_an_edit_and_a_deletion_once_in_the_next_turn() {
    let world = main_world().await;
    let main = &world.main;
    let asked = world
        .say(ANA, &format!("{} is the build red?", main.mention()), None)
        .await;
    world.work().await;
    let first = world.runs(main).await[0].clone();
    let follow = world.say(BO, "it failed at 3", Some(&asked.ts)).await;
    world.work().await;

    // Edits and deletes wake nobody.
    world
        .deliver(
            main,
            &world.chat.edit(&asked, "<@U0HELPER> is the build green?", world.now()),
        )
        .await;
    world.deliver(main, &world.chat.delete(&follow, world.now())).await;
    world.work().await;
    assert_eq!(world.runs(main).await.len(), 2);
    // The thread's next turn says so first, without rewriting either turn
    // the model read before.
    world.say(ANA, "well?", Some(&asked.ts)).await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs.len(), 3);
    assert_eq!(
        runs[2]["output"],
        "You said: [U0ANA edited an earlier message] @helper is the build green?\n\
         [U0BO deleted an earlier message]\n[U0ANA] well?"
    );
    assert_eq!(runs[0]["input"], first["input"], "what it said then stays");
    // Noted once.
    world.say(ANA, "thanks", Some(&asked.ts)).await;
    world.work().await;
    assert_eq!(world.runs(main).await[3]["output"], "You said: [U0ANA] thanks");
    // What the connection kept says both.
    let messages = world.messages_in(main, SUPPORT).await;
    let kept = |ts: &str| {
        messages
            .iter()
            .find(|message| message["external_id"] == ts)
            .unwrap()
            .clone()
    };
    assert!(kept(&asked.ts)["edited_at"].is_string());
    assert!(kept(&follow.ts)["deleted_at"].is_string());
}

#[tokio::test]
async fn sweeps_messages_and_run_text_past_retention_and_starts_afresh() {
    let world = main_world().await;
    let main = &world.main;
    let asked = world
        .say(ANA, &format!("{} the secret plan", main.mention()), None)
        .await;
    world.work().await;
    let before = world.runs(main).await;
    assert_eq!(before[0]["status"], "completed");
    // A month and a day on, the sweep comes due.
    world.setup.kit.clock.advance(Duration::from_secs(31 * 24 * 60 * 60));
    let state = world.setup.kit.state(faults::Worker::A);
    assert_eq!(zupflow_server::channels::retention_due(&state).await, 1);
    assert_eq!(
        zupflow_server::channels::retention_due(&state).await,
        0,
        "queued once until it runs"
    );
    while zupflow_server::channels::work_once(&state).await {}
    assert!(world.messages_in(main, SUPPORT).await.is_empty());
    let after = world.runs(main).await;
    assert_eq!(after[0]["status"], "completed");
    assert_eq!(after[0]["input"], "(Removed after the retention period.)");
    assert!(after[0]["output"].is_null(), "{}", after[0]);
    // The thread's next message starts a new session: nothing swept is
    // read back to the model.
    world
        .say(ANA, &format!("{} again", main.mention()), Some(&asked.ts))
        .await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs.len(), 2);
    assert_ne!(runs[1]["session_id"], runs[0]["session_id"]);
    assert_eq!(runs[1]["output"], "You said: [U0ANA] @helper again");
}

#[tokio::test]
async fn never_reaches_another_workspaces_or_organizations_connection() {
    let world = main_world().await;
    let main = world.main.clone();
    world
        .grant(
            &main,
            &[
                "read_messages",
                "read_thread",
                "search_messages",
                "send_message",
                "react",
                "edit_message",
            ],
        )
        .await;
    world.configure(&main, json!({ "audience": "anywhere" })).await;
    // A sibling workspace's agent and another organization's, each with
    // its own app in the same team, each with a conversation and a post of
    // its own.
    let sibling = world.setup.sibling("beta", "tk_beta").await;
    let beta = world
        .install(
            "alice",
            &sibling.org,
            &sibling.ws,
            &sibling.agent,
            TEAM,
            "U0BETA",
            "beta",
        )
        .await;
    let other = world.setup.other_organization("bob", "larkspur", "tk_other").await;
    let outside = world
        .install("bob", &other.org, &other.ws, &other.agent, TEAM, "U0OUTSIDE", "outside")
        .await;
    let mut theirs = Vec::new();
    for (installed, channel) in [(&beta, "C0BETA"), (&outside, "C0OUTSIDE")] {
        let posted = world.post(&[installed], channel, BO, "the numbers", None).await;
        world.work().await;
        theirs.push((
            world.conversation_id(installed, channel).await,
            world.message_id(installed, &posted).await,
        ));
    }
    let asked = world.say(ANA, &format!("{} hi", main.mention()), None).await;
    world.work().await;
    for (conversation, message) in &theirs {
        let mut lines = Vec::new();
        for (tool, arguments) in [
            ("read_messages", json!({ "conversation": conversation })),
            ("read_thread", json!({ "message": message })),
            ("send_message", json!({ "conversation": conversation, "text": "hi" })),
            ("react", json!({ "message": message, "emoji": "eyes" })),
            ("edit_message", json!({ "message": message, "text": "mine now" })),
        ] {
            lines.push(world.calling(&main, tool, arguments).await);
        }
        world
            .say(ANA, &format!("go\n{}", lines.join("\n")), Some(&asked.ts))
            .await;
        world.work().await;
    }
    let runs = world.runs(&main).await;
    assert_eq!(runs.len(), 3);
    for run in &runs[1..] {
        assert_eq!(run["status"], "completed", "{run}");
        let output = run["output"].as_str().unwrap();
        assert!(!output.contains("the numbers"), "{output}");
        let calls = calls_of(&world, run).await;
        assert_eq!(calls.len(), 5);
        for call in &calls {
            assert!(
                matches!(
                    call.state,
                    zupflow_core::runs::CallState::Refused | zupflow_core::runs::CallState::Failed
                ),
                "{call:?}"
            );
            assert!(
                call.outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.content.contains("isn't one this connection knows")),
                "{call:?}"
            );
        }
    }
    // Nothing reached the chat from it, and the others' records are as
    // they were.
    assert!(world.chat.posted_by_apps().is_empty());
    assert!(world.chat.reactions().is_empty());
    for (installed, channel) in [(&beta, "C0BETA"), (&outside, "C0OUTSIDE")] {
        let messages = world.messages_in(installed, channel).await;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["text"], "the numbers");
        assert!(messages[0]["edited_at"].is_null());
    }
}

#[tokio::test]
async fn leaves_a_post_it_cant_find_unknown_and_never_answers_after_it() {
    let world = main_world().await;
    let main = &world.main;
    world.grant(main, &["reply"]).await;
    let reply = world.calling(main, "reply", json!({ "text": "Looking." })).await;
    // Its answer lost, and the provider's search not showing it yet.
    world.chat.lose_answers(1);
    world.chat.lag_searches(true);
    world
        .say(ANA, &format!("{} anything new?\n{reply}", main.mention()), None)
        .await;
    world.work().await;
    world.reply().await;
    let runs = world.runs(main).await;
    assert_eq!(runs[0]["status"], "completed", "{}", runs[0]);
    assert!(!runs[0]["output"].as_str().unwrap().trim().is_empty());
    let calls = calls_of(&world, &runs[0]).await;
    assert_eq!(calls[0].state, zupflow_core::runs::CallState::Unknown);
    // Posted once: not made again for not being found, and its answer not
    // posted after it, though nothing recorded the post.
    let posted = world.chat.posted_by_apps();
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert_eq!(posted[0].text, "Looking.");
    assert!(
        world
            .messages_in(main, SUPPORT)
            .await
            .iter()
            .all(|message| message["direction"] == "inbound")
    );
}

#[tokio::test]
async fn refuses_shared_conversations_and_other_teams_and_reads_only_its_own_dm() {
    let world = main_world().await;
    let main = &world.main;
    world
        .grant(
            main,
            &["react", "send_direct_message", "read_messages", "get_conversation"],
        )
        .await;
    world.configure(main, json!({ "audience": "anywhere" })).await;
    // A conversation shared with another organization, where its guest
    // posted; and someone's direct message to the agent.
    let guest = Poster::external("U0GUEST", "T0PARTNER");
    let shared = world.post_as(&[main], "C0SHARED", &guest, "the numbers", None).await;
    world.post(&[main], "D0BO", BO, "my review is due", None).await;
    world.work().await;
    let shared_message = world.message_id(main, &shared).await;
    let guest_id = world.messages_in(main, "C0SHARED").await[0]["author"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let dm_id = world.conversation_id(main, "D0BO").await;
    let react = world
        .calling(main, "react", json!({ "message": shared_message, "emoji": "eyes" }))
        .await;
    let direct = world
        .calling(
            main,
            "send_direct_message",
            json!({ "person": guest_id, "text": "hello" }),
        )
        .await;
    let read = world
        .calling(main, "read_messages", json!({ "conversation": dm_id }))
        .await;
    let look = world
        .calling(main, "get_conversation", json!({ "conversation": dm_id }))
        .await;
    let asked = world
        .say(
            ANA,
            &format!("{} try\n{react}\n{direct}\n{read}\n{look}", main.mention()),
            None,
        )
        .await;
    world.work().await;
    let runs = world.runs(main).await;
    let run = runs.last().unwrap();
    assert_eq!(run["status"], "completed", "{run}");
    let calls = calls_of(&world, run).await;
    assert_eq!(calls.len(), 4);
    for call in &calls[..2] {
        assert_eq!(call.state, zupflow_core::runs::CallState::Refused, "{call:?}");
        assert_eq!(call.decision.as_ref().unwrap().rule, "outside_audience");
    }
    let said = |call: &faults::RecordedCall| call.outcome.as_ref().unwrap().content.clone();
    assert!(
        said(&calls[0]).contains("shared with another organization"),
        "{calls:?}"
    );
    assert!(said(&calls[1]).contains("isn't in your team"), "{calls:?}");
    for call in &calls[2..] {
        assert_eq!(call.state, zupflow_core::runs::CallState::Failed, "{call:?}");
        assert!(
            said(call).contains("direct message this run didn't start in"),
            "{call:?}"
        );
        assert!(!said(call).contains("my review"), "{call:?}");
    }
    assert!(world.chat.reactions().is_empty());

    // The direct message's own run reads it.
    let dm_runs: Vec<Value> = runs
        .iter()
        .filter(|run| run["channel"]["conversation_id"] == dm_id.as_str())
        .cloned()
        .collect();
    assert_eq!(dm_runs.len(), 1, "{runs:?}");
    world.post(&[main], "D0BO", BO, &format!("again\n{read}"), None).await;
    world.work().await;
    let runs = world.runs(main).await;
    let calls = calls_of(&world, runs.last().unwrap()).await;
    assert_eq!(calls[0].state, zupflow_core::runs::CallState::Completed, "{calls:?}");
    assert!(said(&calls[0]).contains("my review is due"), "{calls:?}");

    // Allowed outsiders, it acts in the shared conversation.
    world.configure(main, json!({ "allow_external": true })).await;
    world.say(ANA, &format!("again\n{react}"), Some(&asked.ts)).await;
    world.work().await;
    assert_eq!(world.chat.reactions().len(), 1);
}

#[tokio::test]
async fn reads_messages_as_its_turns_render_them() {
    let world = main_world().await;
    let main = &world.main;
    world.grant(main, &["read_messages", "search_messages"]).await;
    name_conversation(&world, main, "C0OTHER", "other").await;
    world
        .post(
            &[main],
            "C0GENERAL",
            BO,
            "<@U0HELPER> see <#C0OTHER> &amp; the plan",
            None,
        )
        .await;
    world.work().await;
    let general = world.conversation_id(main, "C0GENERAL").await;
    let read = world
        .calling(main, "read_messages", json!({ "conversation": general }))
        .await;
    // Found by what it reads as, not by its markup.
    let search = world
        .calling(
            main,
            "search_messages",
            json!({ "query": "@helper see #other & the", "conversation": general }),
        )
        .await;
    let markup = world
        .calling(
            main,
            "search_messages",
            json!({ "query": "U0HELPER", "conversation": general }),
        )
        .await;
    world
        .say(
            ANA,
            &format!("{} look\n{read}\n{search}\n{markup}", main.mention()),
            None,
        )
        .await;
    world.work().await;
    let runs = world.runs(main).await;
    let calls = calls_of(&world, runs.last().unwrap()).await;
    assert_eq!(calls.len(), 3);
    for call in &calls {
        assert_eq!(call.state, zupflow_core::runs::CallState::Completed, "{call:?}");
        let said = &call.outcome.as_ref().unwrap().content;
        assert!(!said.contains("U0HELPER") && !said.contains("C0OTHER"), "{said}");
    }
    for call in &calls[..2] {
        let said = &call.outcome.as_ref().unwrap().content;
        assert!(said.contains("@helper see #other & the plan"), "{said}");
    }
    let found: Value = serde_json::from_str(&calls[2].outcome.as_ref().unwrap().content).unwrap();
    assert_eq!(found["messages"], json!([]), "a provider ID isn't what it reads");
}

#[tokio::test]
async fn keeps_the_live_session_when_an_older_sessions_run_ages_out() {
    const DAY: u64 = 24 * 60 * 60;
    let world = main_world().await;
    let main = &world.main;
    let asked = world.say(ANA, &format!("{} day one", main.mention()), None).await;
    world.work().await;
    // A day on, its tools change: the thread's next run starts a new
    // session, and the first one's is left behind.
    world.setup.kit.clock.advance(Duration::from_secs(DAY));
    world.grant(main, &["reply"]).await;
    world
        .say(ANA, &format!("{} day two", main.mention()), Some(&asked.ts))
        .await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs.len(), 2);
    assert_ne!(runs[0]["session_id"], runs[1]["session_id"]);
    // The first run ages out; the second, the live session's, doesn't.
    world.setup.kit.clock.advance(Duration::from_secs(29 * DAY + DAY / 2));
    let state = world.setup.kit.state(faults::Worker::A);
    assert_eq!(zupflow_server::channels::retention_due(&state).await, 1);
    while zupflow_server::channels::work_once(&state).await {}
    let runs = world.runs(main).await;
    assert_eq!(runs[0]["input"], "(Removed after the retention period.)");
    assert!(
        runs[1]["input"].as_str().unwrap().ends_with("[U0ANA] @helper day two"),
        "{}",
        runs[1]
    );
    // The live session goes on.
    world
        .say(ANA, &format!("{} day thirty", main.mention()), Some(&asked.ts))
        .await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[2]["status"], "completed", "{}", runs[2]);
    assert_eq!(runs[2]["session_id"], runs[1]["session_id"]);
}

#[tokio::test]
async fn sweeping_a_run_leaves_none_of_its_text() {
    let world = main_world().await;
    let main = &world.main;
    world
        .say(ANA, &format!("{} the zebra plan", main.mention()), None)
        .await;
    world.work().await;
    // A run that reads it through a tool, then stays silent saying why.
    world.grant(main, &["read_messages"]).await;
    let support = world.conversation_id(main, SUPPORT).await;
    let read = world
        .calling(main, "read_messages", json!({ "conversation": support }))
        .await;
    let silent = "call stay_silent {\"reason\":\"The zebra plan is settled.\"}";
    world
        .say(ANA, &format!("{} recap\n{read}\nthen\n{silent}", main.mention()), None)
        .await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1]["channel"]["silent_reason"], "The zebra plan is settled.");
    assert!(world.database_text().await.contains("zebra"));

    world.setup.kit.clock.advance(Duration::from_secs(31 * 24 * 60 * 60));
    let state = world.setup.kit.state(faults::Worker::A);
    zupflow_server::channels::retention_due(&state).await;
    while zupflow_server::channels::work_once(&state).await {}
    let runs = world.runs(main).await;
    assert!(runs[1]["channel"]["silent_reason"].is_null(), "{}", runs[1]);
    let text = world.database_text().await;
    let left: Vec<&str> = text.lines().filter(|line| line.contains("zebra")).collect();
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test]
async fn notes_an_edit_that_arrives_while_a_steered_turn_is_taken() {
    let world = main_world().await;
    let main = &world.main;
    let kit = &world.setup.kit;
    world.grant(main, &["list_conversations"]).await;
    let list = world.calling(main, "list_conversations", json!({})).await;
    let asked = world
        .say(ANA, &format!("{} is the build red?\n{list}", main.mention()), None)
        .await;
    world.handle().await;
    // While its first turn's call is made, a follow-up arrives: its next
    // turn takes it in.
    let answered = kit.faults.arm(faults::Worker::A, faults::Point::AfterModel);
    let running = kit.spawn(faults::Worker::A);
    answered.reached().await;
    world.say(BO, "it failed at 3", Some(&asked.ts)).await;
    world.handle().await;
    let preparing = kit
        .faults
        .arm(faults::Worker::A, faults::Point::Before(faults::Write::PrepareModel));
    answered.release();
    preparing.reached().await;
    // The steered turn is rendered; the question is edited a second later,
    // before the turn is taken.
    kit.clock.advance(Duration::from_secs(1));
    world
        .deliver(
            main,
            &world.chat.edit(&asked, "<@U0HELPER> is the build green?", world.now()),
        )
        .await;
    preparing.release();
    running.finish().await;
    kit.drain(faults::Worker::A).await;
    let runs = world.runs(main).await;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["status"], "completed", "{}", runs[0]);
    // The steered turn didn't see the edit: the next one notes it.
    world.say(ANA, "well?", Some(&asked.ts)).await;
    world.work().await;
    let runs = world.runs(main).await;
    assert_eq!(runs.len(), 2);
    assert_eq!(
        runs[1]["output"],
        "You said: [U0ANA edited an earlier message] @helper is the build green?\n[U0ANA] well?"
    );
}

#[tokio::test]
async fn refuses_an_approved_call_whose_connection_was_removed() {
    let world = main_world().await;
    let main = &world.main;
    world.grant(main, &["send_message"]).await;
    world.configure(main, json!({ "audience": "anywhere" })).await;
    world.post(&[main], "C0GENERAL", BO, "lunch?", None).await;
    world.work().await;
    let general_id = world.conversation_id(main, "C0GENERAL").await;
    let send = world
        .calling(
            main,
            "send_message",
            json!({ "conversation": general_id, "text": "Deploy at 5." }),
        )
        .await;
    world
        .say(ANA, &format!("{} tell everyone\n{send}", main.mention()), None)
        .await;
    world.work().await;
    let runs = world.runs(main).await;
    let waiting = runs.last().unwrap().clone();
    assert_eq!(waiting["status"], "waiting_for_approval", "{waiting}");
    let (status, body) = world
        .setup
        .kit
        .send(
            call(
                &main.user,
                Method::DELETE,
                format!("/v1/workspaces/{}/channel-connections/{}", main.ws, main.connection),
            )
            .naming(&main.org),
        )
        .await;
    assert!(status.is_success(), "{status}: {body}");
    world.approve(World::run_id(&waiting)).await;
    world.work().await;
    let calls = calls_of(&world, &waiting).await;
    assert_eq!(
        calls[0].failure.as_ref().map(|failure| failure.code.as_str()),
        Some("channel_connection_removed"),
        "{:?}",
        calls[0]
    );
    assert!(world.chat.posted_by_apps().is_empty());
}
