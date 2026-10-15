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
