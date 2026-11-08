//! Abuse classification with durable workflows.
//!
//! Most reports get a verdict from a fast model right away. Unclear ones go
//! to two human reviewers in parallel, with a deadline; if the deadline
//! passes first, a supervisor decides. Each command is a separate process:
//!
//!   abuse submit <id> <text>        classify a report (fast path, or park)
//!   abuse pending                   requests waiting for a human
//!   abuse deliver <task> <req> <block:true|false>
//!   abuse timers                    resume tasks whose deadline passed
//!   abuse status <task>             status and stored state
//!
//! Build with `cargo durable build -p abuse`.

use durable::{Either, Outcome, Runtime, Status};
use serde::{Deserialize, Serialize};
use std::time::Duration;

durable::glue!();

const DB: &str = "abuse.db";
/// How long reviewers get before a supervisor is asked (ms).
const REVIEW_DEADLINE_MS: u64 = 3_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Report {
    id: String,
    text: String,
}

/// A human decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Decision {
    block: bool,
}

/// Fast model: a stand-in for a millisecond-scale classifier.
async fn fast_score(text: &str) -> u32 {
    tokio::time::sleep(Duration::from_millis(5)).await;
    let t = text.to_lowercase();
    if t.contains("spam") || t.contains("scam") {
        95
    } else if t.contains("hello") || t.contains("thanks") {
        5
    } else {
        50
    }
}

async fn review(reviewer: String, report: Report) -> Decision {
    durable::request(&reviewer, &report).await
}

#[durable::workflow]
async fn classify(report: Report) -> String {
    let score = fast_score(&report.text).await;
    if score < 30 {
        return format!("allow (model, score {score})");
    }
    if score > 80 {
        return format!("block (model, score {score})");
    }
    // Unclear: two reviewers in parallel, with a deadline.
    let deadline = durable::now_ms() + REVIEW_DEADLINE_MS;
    let reviews = durable::join2(review("reviewer-a".into(), report.clone()), review("reviewer-b".into(), report.clone()));
    match durable::select2(reviews, durable::sleep_until(deadline)).await {
        Either::Left((a, b)) if a.block && b.block => "block (both reviewers)".into(),
        Either::Left((a, b)) if a.block != b.block => {
            let d: Decision = durable::request("supervisor", &report).await;
            format!("{} (reviewers split, supervisor decided)", if d.block { "block" } else { "allow" })
        }
        Either::Left(_) => "allow (both reviewers)".into(),
        Either::Right(()) => {
            let d: Decision = durable::request("supervisor", &report).await;
            format!("{} (review deadline passed, supervisor decided)", if d.block { "block" } else { "allow" })
        }
    }
}

fn print_status(rt: &Runtime, task: &str, s: &Status) {
    match s {
        Status::Done(v) => println!("{task}: done: {v}"),
        Status::Failed(e) => println!("{task}: failed: {e}"),
        Status::Parked { waiting, wake_at, error } => {
            let wake = wake_at.map(|w| format!(", timer in {} ms", w.saturating_sub(durable::now_ms())));
            println!("{task}: parked, waiting on requests {waiting:?}{}", wake.unwrap_or_default());
            if let Some(e) = error {
                println!("  last resume failed: {e}");
            }
            if let Ok(Some(snap)) = rt.snapshot(task) {
                println!("  stored state: {snap}");
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut rt = Runtime::open(DB, GLUE, |o| {
        println!("  -> sent {} request #{} for task {}: {}", o.kind, o.req_id, o.task_id, o.payload);
    })?;
    rt.register(classify)?;
    rt.redispatch()?; // anything stored but not sent before a crash

    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or_default();
    match arg(0) {
        "submit" => {
            let report = Report { id: arg(1).into(), text: args[2..].join(" ") };
            match rt.start(arg(1), classify(report)).await? {
                Outcome::Done(v) => println!("{}: {v}", arg(1)),
                Outcome::Parked => println!("{}: parked", arg(1)),
            }
        }
        "pending" => {
            for o in rt.open_requests()? {
                println!("{} #{} {}: {}", o.task_id, o.req_id, o.kind, o.payload);
            }
        }
        "deliver" => {
            let decision = Decision { block: arg(3) == "true" };
            let s = rt.deliver(arg(1), arg(2).parse()?, decision).await?;
            print_status(&rt, arg(1), &s);
        }
        "timers" => {
            for (task, r) in rt.run_due_timers().await? {
                match r {
                    Ok(s) => print_status(&rt, &task, &s),
                    Err(e) => println!("{task}: {e}"),
                }
            }
        }
        "status" => print_status(&rt, arg(1), &rt.status(arg(1))?),
        _ => eprintln!("usage: abuse submit|pending|deliver|timers|status ..."),
    }
    Ok(())
}
