//! Seeded races on the run engine, with the kit in `common/faults.rs`: two
//! workers and the API's cancels and decisions, interleaved at random
//! boundaries chosen by a seeded generator, then every invariant the engine
//! owes checked against the app's server's ledger, the store and the
//! timeline (`Setup::violations`), and every worker's step against the
//! claim it was made under.
//!
//! Deterministic: a seed fixes every choice, and only one worker runs at a
//! time (the others are stopped at their points), so a seed replays the
//! same interleaving. Each run uses seeds 1 to 10 and every seed in
//! [`REGRESSIONS`]. Locally, `ZUPFLOW_RUN_RACE_SEEDS=1000` runs seeds 1 to
//! 1000 instead, and `ZUPFLOW_RUN_RACE_SEED=<seed>` replays one, printing
//! what it did. A seed that ever breaks an invariant goes into
//! [`REGRESSIONS`] with the change that fixes it.

#![allow(clippy::unwrap_used)]

mod common;
#[path = "common/faults.rs"]
mod faults;
#[path = "common/run_setup.rs"]
mod run_setup;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::{Method, StatusCode};
use faults::{Attempted, Gate, Halt, Point, Running, Worker, Write};
use run_setup::{Setup, setup};
use serde_json::json;
use zupflow_core::ids::RunId;
use zupflow_core::runs::CallState;
use zupflow_server::run_engine;

/// How many seeds a run takes by default: 1 to this.
const SEEDS: u64 = 10;

/// Seeds that once broke an invariant, run every time from then on.
const REGRESSIONS: &[u64] = &[];

/// The random actions each seed takes before the run is worked to its end.
const ROUNDS: usize = 40;

/// Every boundary a worker can be stopped at: before and after each of its
/// writes, around the model's answer and around the send.
const POINTS: [Point; 22] = [
    Point::Before(Write::Claim),
    Point::After(Write::Claim),
    Point::Before(Write::PrepareModel),
    Point::After(Write::PrepareModel),
    Point::BeforeModel,
    Point::AfterModel,
    Point::Before(Write::CommitTurn),
    Point::After(Write::CommitTurn),
    Point::Before(Write::Park),
    Point::After(Write::Park),
    Point::Before(Write::AuthorizeCall),
    Point::After(Write::AuthorizeCall),
    Point::BeforeSend,
    Point::AfterSend,
    Point::Before(Write::RecordOutcome),
    Point::After(Write::RecordOutcome),
    Point::Before(Write::SettleUnknown),
    Point::After(Write::SettleUnknown),
    Point::Before(Write::Yield),
    Point::After(Write::Yield),
    Point::Before(Write::Finish),
    Point::After(Write::Finish),
];

/// SplitMix64: small, fast and the same everywhere.
struct Seeded(u64);

impl Seeded {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// From 0 to `n - 1`.
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).unwrap()).unwrap()
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

/// A worker stopped at its gate.
struct Stopped {
    worker: Worker,
    running: Running,
    gate: Arc<Gate>,
}

/// What one seed did and found.
struct Raced {
    seed: u64,
    actions: Vec<String>,
    /// How the run ended, with its error's code.
    ending: String,
    violations: Vec<String>,
}

/// Every worker's step committed under the claim that held the run then:
/// none under a claim another had superseded. (A sender's outcome is fenced
/// by its send claim instead, and a finished run's dead send by its send
/// claim alone, so neither is held to the owner.)
fn fenced(writes: &[Attempted]) -> Vec<String> {
    let mut broken = Vec::new();
    let mut held: Option<u64> = None;
    for attempted in writes.iter().filter(|attempted| attempted.result.is_ok()) {
        match attempted.write {
            Write::Claim => held = attempted.epoch,
            Write::PrepareModel
            | Write::CommitTurn
            | Write::Park
            | Write::AuthorizeCall
            | Write::Yield
            | Write::Finish
                if attempted.epoch != held =>
            {
                broken.push(format!(
                    "{:?} committed {:?} under epoch {:?} while epoch {held:?} held the run",
                    attempted.worker, attempted.write, attempted.epoch
                ));
            }
            _ => {}
        }
    }
    broken
}

/// The first call waiting for approval, if the run waits on one.
async fn awaiting(setup: &Setup, run: RunId) -> Option<String> {
    setup
        .calls(run)
        .await
        .into_iter()
        .find(|call| call.state == CallState::AwaitingApproval)
        .map(|call| call.id.to_string())
}

async fn race(seed: u64) -> Raced {
    let mut rng = Seeded(seed);
    let mut actions = Vec::new();
    let mut violations = Vec::new();
    let setup = setup(&[("Main", "tk_main")]).await;
    let key = setup.api_key().await;
    // An allowed call, one that needs approval, and another allowed one.
    let asked = [json!({ "n": 1 }), json!({ "n": 2 }), json!({ "n": 3 })];
    let run = setup.run_calling(&["lookup", "send", "lookup"], &asked).await;
    let mut stopped: Vec<Stopped> = Vec::new();
    // Whether a worker died holding an answer it never recorded.
    let mut answer_lost = false;
    for _ in 0..ROUNDS {
        if stopped.is_empty() && setup.kit.run_finished(setup.scope().0, setup.scope().1, run).await {
            break;
        }
        match rng.below(100) {
            // A free worker works until a boundary, or the end of its claim.
            0..=44 => {
                let free: Vec<Worker> = [Worker::A, Worker::B]
                    .into_iter()
                    .filter(|worker| stopped.iter().all(|stop| stop.worker != *worker))
                    .collect();
                if free.is_empty() {
                    continue;
                }
                let worker = free[rng.below(free.len())];
                let point = POINTS[rng.below(POINTS.len())];
                let nth = if rng.chance(75) { 1 } else { 2 };
                let gate = setup.kit.faults.arm_nth(worker, point, nth);
                match setup.kit.spawn(worker).until(&gate).await {
                    Halt::Stopped(running) => {
                        actions.push(format!("{worker:?} stopped at {point:?} #{nth}"));
                        stopped.push(Stopped { worker, running, gate });
                    }
                    Halt::Finished(had_one) => {
                        setup.kit.faults.disarm_all();
                        actions.push(format!(
                            "{worker:?} worked (job: {had_one}), never reaching {point:?} #{nth}"
                        ));
                    }
                }
            }
            // A stopped worker goes on to the end of its claim.
            45..=59 => {
                if stopped.is_empty() {
                    continue;
                }
                let stop = stopped.remove(rng.below(stopped.len()));
                stop.gate.release();
                stop.running.finish().await;
                actions.push(format!("{:?} went on from {:?}", stop.worker, stop.gate.point()));
            }
            // A stopped worker dies there.
            60..=69 => {
                if stopped.is_empty() {
                    continue;
                }
                let stop = stopped.remove(rng.below(stopped.len()));
                answer_lost |= matches!(
                    stop.gate.point(),
                    Point::AfterSend | Point::Before(Write::RecordOutcome)
                );
                stop.running.crash().await;
                actions.push(format!("{:?} died at {:?}", stop.worker, stop.gate.point()));
            }
            // Time passes: a claim's lease, a send's lease, or a moment.
            70..=87 => {
                let by = match rng.below(10) {
                    0..=4 => run_engine::LEASE + Duration::from_secs(1),
                    5..=7 => Duration::from_secs(61),
                    _ => Duration::from_secs(5),
                };
                setup.kit.clock.advance(by);
                actions.push(format!("{}s passed", by.as_secs()));
            }
            // The organization's backend decides the call the run waits on,
            // its request perhaps stopped, or crashed and sent again.
            88..=96 => {
                let Some(call_id) = awaiting(&setup, run).await else {
                    continue;
                };
                let decision = if rng.chance(75) { "approved" } else { "denied" };
                let body = json!({ "call_id": call_id, "decision": decision });
                let uri = format!("/v1/workspaces/{}/runs/{run}/approvals", setup.ws);
                let (status, answer) = if rng.chance(50) {
                    let point = if rng.chance(50) {
                        Point::Before(Write::ResolveApproval)
                    } else {
                        Point::After(Write::ResolveApproval)
                    };
                    let dies = rng.chance(50);
                    actions.push(format!("{decision} stopped at {point:?}, dies: {dies}"));
                    setup.api_stopped(point, dies, &key, uri, Some(body)).await
                } else {
                    setup.decide(&key, run, &call_id, decision).await
                };
                actions.push(format!("{decision}: {status}"));
                if !matches!(status, StatusCode::OK | StatusCode::CONFLICT) {
                    violations.push(format!("a decision answered {status}: {answer}"));
                }
            }
            // A cancel, perhaps stopped, or crashed and sent again.
            _ => {
                let uri = format!("/v1/workspaces/{}/runs/{run}/cancel", setup.ws);
                let (status, answer) = if rng.chance(50) {
                    let point = if rng.chance(50) {
                        Point::Before(Write::Cancel)
                    } else {
                        Point::After(Write::Cancel)
                    };
                    let dies = rng.chance(50);
                    actions.push(format!("cancel stopped at {point:?}, dies: {dies}"));
                    setup.api_stopped(point, dies, "alice", uri, None).await
                } else {
                    setup
                        .kit
                        .send(common::call("alice", Method::POST, uri).naming(&setup.org))
                        .await
                };
                actions.push(format!("cancel: {status}"));
                if !matches!(status, StatusCode::OK | StatusCode::CONFLICT) {
                    violations.push(format!("a cancel answered {status}: {answer}"));
                }
            }
        }
    }
    // Whoever is still stopped dies or goes on; then the run is worked to
    // its end, approving what it waits on, the sweep failing it if its job
    // died.
    for stop in stopped.drain(..) {
        if rng.chance(50) {
            answer_lost |= matches!(
                stop.gate.point(),
                Point::AfterSend | Point::Before(Write::RecordOutcome)
            );
            stop.running.crash().await;
            actions.push(format!("{:?} died at {:?}", stop.worker, stop.gate.point()));
        } else {
            stop.gate.release();
            stop.running.finish().await;
            actions.push(format!("{:?} went on from {:?}", stop.worker, stop.gate.point()));
        }
    }
    setup.kit.faults.disarm_all();
    for _ in 0..20 {
        setup.kit.drain(Worker::B).await;
        run_engine::sweep(&setup.kit.state(Worker::B)).await;
        let status = setup.get(run).await["status"].clone();
        if matches!(status.as_str(), Some("completed" | "failed" | "cancelled")) {
            actions.push(format!("ended {status}"));
            break;
        }
        if let Some(call_id) = awaiting(&setup, run).await {
            setup.decide(&key, run, &call_id, "approved").await;
            actions.push("approved at the end".to_owned());
        }
    }
    violations.extend(setup.violations(run, &asked, !answer_lost).await);
    violations.extend(fenced(&setup.kit.faults.writes()));
    let done = setup.get(run).await;
    let ending = match done["error"]["code"].as_str() {
        Some(code) => format!("{} {code}", done["status"].as_str().unwrap_or_default()),
        None => done["status"].as_str().unwrap_or_default().to_owned(),
    };
    Raced {
        seed,
        actions,
        ending,
        violations,
    }
}

/// The seeds this run takes.
fn seeds() -> Vec<u64> {
    if let Ok(seed) = std::env::var("ZUPFLOW_RUN_RACE_SEED") {
        return vec![seed.parse().expect("ZUPFLOW_RUN_RACE_SEED is a number")];
    }
    let count = std::env::var("ZUPFLOW_RUN_RACE_SEEDS")
        .map(|count| count.parse().expect("ZUPFLOW_RUN_RACE_SEEDS is a number"))
        .unwrap_or(SEEDS);
    let mut seeds: Vec<u64> = (1..=count).collect();
    seeds.extend(REGRESSIONS.iter().filter(|seed| **seed > count));
    seeds
}

/// Runs one seed on a runtime of its own; a panic counts as a violation.
fn race_alone(seed: u64) -> Raced {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let raced = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runtime.block_on(race(seed))));
    raced.unwrap_or_else(|panic| Raced {
        seed,
        actions: Vec::new(),
        ending: "panicked".to_owned(),
        violations: vec![format!(
            "panicked: {}",
            panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
                .unwrap_or_default()
        )],
    })
}

#[test]
fn races_break_no_invariant_for_any_seed() {
    let started = Instant::now();
    let seeds = seeds();
    let total = seeds.len();
    let queue = Mutex::new(seeds);
    let results = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism().map_or(4, |threads| threads.get().min(8));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let Some(seed) = queue.lock().unwrap().pop() else {
                        return;
                    };
                    let raced = race_alone(seed);
                    results.lock().unwrap().push(raced);
                }
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|raced| raced.seed);
    let mut endings = std::collections::BTreeMap::<String, usize>::new();
    for raced in &results {
        *endings.entry(raced.ending.clone()).or_default() += 1;
    }
    let replaying = std::env::var("ZUPFLOW_RUN_RACE_SEED").is_ok();
    let actions: usize = results.iter().map(|raced| raced.actions.len()).sum();
    let tally = |what: &str| -> usize {
        results
            .iter()
            .flat_map(|raced| &raced.actions)
            .filter(|action| action.contains(what))
            .count()
    };
    let (stops, deaths) = (tally(" stopped at "), tally(" died at "));
    let broken: Vec<&Raced> = results.iter().filter(|raced| !raced.violations.is_empty()).collect();
    for raced in &results {
        if replaying || !raced.violations.is_empty() {
            eprintln!("seed {}:\n  {}", raced.seed, raced.actions.join("\n  "));
        }
    }
    eprintln!(
        "{total} seeds, {actions} actions ({stops} stops, {deaths} deaths), endings {endings:?}, {} violations, in {:?}",
        broken.iter().map(|raced| raced.violations.len()).sum::<usize>(),
        started.elapsed()
    );
    assert!(
        broken.is_empty(),
        "{}",
        broken
            .iter()
            .map(|raced| format!(
                "seed {} broke {:#?}\n  replay: ZUPFLOW_RUN_RACE_SEED={} cargo test -p zupflow-server --test run_races -- --nocapture",
                raced.seed, raced.violations, raced.seed
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn a_seed_replays_the_same_choices() {
    let (mut first, mut second) = (Seeded(7), Seeded(7));
    let drawn: Vec<usize> = (0..32).map(|_| first.below(100)).collect();
    assert_eq!(drawn, (0..32).map(|_| second.below(100)).collect::<Vec<_>>());
    assert_ne!(drawn, (0..32).map(|_| Seeded(8).below(100)).collect::<Vec<_>>());
}

#[test]
fn the_fence_check_catches_a_step_under_a_superseded_claim() {
    use faults::Refused;
    let committed = |worker, write, epoch| Attempted {
        worker,
        write,
        epoch,
        result: Ok::<(), Refused>(()),
    };
    let fine = [
        committed(Worker::A, Write::Claim, Some(1)),
        committed(Worker::A, Write::PrepareModel, Some(1)),
        committed(Worker::B, Write::Claim, Some(2)),
        committed(Worker::B, Write::CommitTurn, Some(2)),
        committed(Worker::A, Write::RecordOutcome, None),
    ];
    assert!(fenced(&fine).is_empty(), "{:?}", fenced(&fine));
    let stale = [
        committed(Worker::A, Write::Claim, Some(1)),
        committed(Worker::B, Write::Claim, Some(2)),
        committed(Worker::A, Write::CommitTurn, Some(1)),
    ];
    assert_eq!(fenced(&stale).len(), 1);
}
