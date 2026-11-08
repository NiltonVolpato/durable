//! Content moderation with slow steps on both sides of the call.
//!
//! - The moderation service runs `moderate`: a malware scan (~15 min), then
//!   per-client logic written by the client (here `clients::acme`,
//!   `clients::globex`), which may ask for a manual review (1-3 days) and,
//!   for some categories, a legal review.
//! - The scanner service answers the scan. It is itself a durable workflow
//!   (`scan`): submit to a sandbox, poll it, resubmit if it crashes.
//!
//! Neither side is a hand-written state machine, and neither needs to be
//! deterministic: the client code uses randomness and the clock, and the
//! decisions it made before parking are part of the stored state.
//!
//! Each command is a separate process (state is in moderation.db / scanner.db):
//!   moderation submit <id> <client> <content...>
//!   moderation run <secs>             run both services' automated work
//!   moderation pending                requests waiting for a human
//!   moderation deliver <task> <req> <json>
//!   moderation status <task>
//!
//! Times are scaled down for the demo (see the constants).

use durable::{Either, Outcome, Runtime, Status};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

durable::glue!();

/// "5 minutes" between sandbox polls.
const POLL_MS: u64 = 300;
/// "30 minutes" before a sandbox run is abandoned and resubmitted.
const SCAN_TIMEOUT_MS: u64 = 2_000;
/// "2 days" for globex's review SLA.
const REVIEW_SLA_MS: u64 = 4_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub client: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Scan {
    Clean,
    Suspicious(String),
    Malware(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Category {
    Spam,
    Copyright,
    Defamation,
    Violence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub flags: Vec<Category>,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Legal {
    pub takedown: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Action {
    Publish,
    Restrict(String),
    Remove(String),
}

// ---------- moderation service ----------

#[durable::workflow]
async fn moderate(item: Item) -> Action {
    // ~15 minutes; answered by the scanner service.
    let scan: Scan = durable::request("malware_scan", &item).await;
    // Per-client logic, written by the client.
    match item.client.as_str() {
        "acme" => clients::acme(item, scan).await,
        "globex" => clients::globex(item, scan).await,
        _ => clients::default(item, scan).await,
    }
}

/// Client-written policies. Plain async Rust: no determinism rules.
mod clients {
    use super::*;

    /// Spot-checks a share of clean items; defamation and copyright go to legal.
    pub async fn acme(item: Item, scan: Scan) -> Action {
        if let Scan::Malware(family) = &scan {
            return Action::Remove(format!("malware: {family}"));
        }
        // Random spot check. Under replay this would re-roll on every resume,
        // and a resumed item could skip the review it is waiting for.
        let spot_check = random_percent() < spot_check_percent();
        if scan == Scan::Clean && !spot_check {
            return Action::Publish;
        }
        let review: Review = durable::request("manual_review", &item).await; // 1-3 days
        if review.flags.iter().any(|c| matches!(c, Category::Defamation | Category::Copyright)) {
            let legal: Legal = durable::request("legal_review", (item.clone(), review.clone())).await;
            return if legal.takedown { Action::Remove("legal takedown".into()) } else { Action::Publish };
        }
        if review.flags.is_empty() { Action::Publish } else { Action::Remove(format!("{:?}", review.flags)) }
    }

    /// Suspicious items get a review with an SLA; if it is missed, the item stays restricted.
    pub async fn globex(item: Item, scan: Scan) -> Action {
        match &scan {
            Scan::Malware(family) => return Action::Remove(format!("malware: {family}")),
            Scan::Clean => return Action::Publish,
            Scan::Suspicious(_) => {}
        }
        // Decided now, from the wall clock. Under replay a resume during
        // business hours would see a different answer.
        let flagged_after_hours = !business_hours();
        let sla = durable::now_ms() + REVIEW_SLA_MS;
        match durable::select2(durable::request::<Review>("manual_review", &item), durable::sleep_until(sla)).await {
            Either::Left(r) if r.flags.is_empty() => Action::Publish,
            Either::Left(r) => Action::Remove(format!("{:?}", r.flags)),
            Either::Right(()) => Action::Restrict(format!(
                "review SLA missed{}",
                if flagged_after_hours { ", flagged after hours" } else { "" }
            )),
        }
    }

    pub async fn default(_item: Item, scan: Scan) -> Action {
        match scan {
            Scan::Clean => Action::Publish,
            other => Action::Restrict(format!("{other:?}")),
        }
    }

    fn random_percent() -> u64 {
        use std::hash::{BuildHasher, RandomState};
        RandomState::new().hash_one(durable::now_ms()) % 100
    }

    fn spot_check_percent() -> u64 {
        std::env::var("SPOT_CHECK_PERCENT").ok().and_then(|v| v.parse().ok()).unwrap_or(10)
    }

    fn business_hours() -> bool {
        let hour = durable::now_ms() / 3_600_000 % 24; // UTC
        (9..17).contains(&hour)
    }
}

// ---------- scanner service ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Sandbox {
    Running,
    Crashed,
    Done(Scan),
}

/// The server side of `malware_scan`: submit, poll, resubmit on crash or timeout.
#[durable::workflow]
async fn scan(item: Item) -> Scan {
    for attempt in 1..=3u64 {
        let run: String = durable::request("sandbox_submit", &item).await;
        let give_up = durable::now_ms() + SCAN_TIMEOUT_MS;
        loop {
            durable::sleep_until(durable::now_ms() + POLL_MS).await;
            match durable::request::<Sandbox>("sandbox_status", &run).await {
                Sandbox::Done(verdict) => return verdict,
                Sandbox::Running if durable::now_ms() < give_up => {}
                _ => break,
            }
        }
        durable::sleep_until(durable::now_ms() + POLL_MS * attempt).await; // back off, then resubmit
    }
    Scan::Suspicious("sandbox failed 3 times".into())
}

/// A fake sandbox API with state in sandbox.json (each command is a process).
mod sandbox {
    use super::*;
    use std::collections::HashMap;

    const FILE: &str = "sandbox.json";

    fn load() -> HashMap<String, (u64, String)> {
        std::fs::read_to_string(FILE).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn submit(run_id: String, item: Item) -> Result<String, String> {
        let mut runs = load();
        runs.insert(run_id.clone(), (durable::now_ms(), item.content));
        std::fs::write(FILE, serde_json::to_string(&runs).unwrap()).map_err(|e| e.to_string())?;
        Ok(run_id)
    }

    /// Done ~600 ms after submit. "crash" crashes the first run, "virus" is
    /// malware, "macro" is suspicious.
    pub fn status(run_id: &str) -> Result<Sandbox, String> {
        let (started, content) = load().remove(run_id).ok_or("unknown sandbox run")?;
        let first_run = run_id.ends_with(":0");
        Ok(if content.contains("crash") && first_run {
            Sandbox::Crashed
        } else if durable::now_ms() < started + 600 {
            Sandbox::Running
        } else if content.contains("virus") {
            Sandbox::Done(Scan::Malware("EICAR".into()))
        } else if content.contains("macro") {
            Sandbox::Done(Scan::Suspicious("office macro".into()))
        } else {
            Sandbox::Done(Scan::Clean)
        })
    }
}

// ---------- the two services ----------

/// Scan jobs handed from moderation to the scanner (queue.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Job {
    task: String,
    req: u64,
    item: Item,
    started: bool,
    reported: bool,
}

impl Job {
    fn scan_id(&self) -> String {
        format!("scan:{}#{}", self.task, self.req)
    }
}

fn load_queue() -> Vec<Job> {
    std::fs::read_to_string("queue.json").ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save_queue(q: &[Job]) {
    std::fs::write("queue.json", serde_json::to_string_pretty(q).unwrap()).unwrap();
}

fn moderation_runtime() -> Result<Runtime, durable::Error> {
    let mut rt = Runtime::open("moderation.db", GLUE, |o| {
        if o.kind == "malware_scan" {
            let mut q = load_queue();
            let item = serde_json::from_value(o.payload.clone()).expect("scan payload is an Item");
            q.push(Job { task: o.task_id.clone(), req: o.req_id, item, started: false, reported: false });
            save_queue(&q);
            println!("  -> {}: queued malware scan", o.task_id);
        } else {
            println!("  -> {}: {} #{} sent to the human queue", o.task_id, o.kind, o.req_id);
        }
    })?;
    rt.register(moderate)?;
    rt.redispatch()?;
    Ok(rt)
}

fn scanner_runtime() -> Result<Runtime, durable::Error> {
    let mut rt = Runtime::open("scanner.db", GLUE, |o| println!("  -> scanner: unexpected request {}", o.kind))?;
    rt.register(scan)?;
    rt.handle("sandbox_submit", |o| async move {
        let item: Item = serde_json::from_value(o.payload).map_err(|e| e.to_string())?;
        // (task, req) is the idempotency key: a retried submit reuses the run.
        sandbox::submit(format!("{}:{}", o.task_id, o.req_id), item)
    });
    rt.handle("sandbox_status", |o| async move {
        let run: String = serde_json::from_value(o.payload).map_err(|e| e.to_string())?;
        sandbox::status(&run)
    });
    rt.redispatch()?;
    Ok(rt)
}

/// One round of the scanner service: start new jobs, poll sandboxes, report verdicts.
async fn scanner_round(moderation: &Runtime, scanner: &Runtime) -> Result<usize, Box<dyn std::error::Error>> {
    let mut q = load_queue();
    let mut n = 0;
    for job in q.iter_mut().filter(|j| !j.started) {
        scanner.start(&job.scan_id(), scan(job.item.clone())).await?;
        job.started = true;
        n += 1;
    }
    save_queue(&q);
    n += scanner.work().await?;
    for job in q.iter_mut().filter(|j| j.started && !j.reported) {
        if let Status::Done(verdict) = scanner.status(&job.scan_id())? {
            println!("  <- scanner: {} is {verdict}", job.item.id);
            print_status(moderation, &job.task, &moderation.deliver(&job.task, job.req, verdict).await?);
            job.reported = true;
            n += 1;
        }
    }
    save_queue(&q);
    Ok(n)
}

fn print_status(rt: &Runtime, task: &str, s: &Status) {
    match s {
        Status::Done(v) => println!("{task}: {v}"),
        Status::Failed(e) => println!("{task}: failed: {e}"),
        Status::Parked { waiting, wake_at, error } => {
            let timer = wake_at.map(|w| format!(", timer in {} ms", w.saturating_sub(durable::now_ms())));
            println!("{task}: parked on requests {waiting:?}{}", timer.unwrap_or_default());
            if let Some(e) = error {
                println!("  last resume failed: {e}");
            }
            if let Ok(Some(snap)) = rt.snapshot(task) {
                println!("  stored: {}", summarize(&snap));
            }
        }
    }
}

/// The stored locals of the innermost frames, without the bookkeeping fields.
fn summarize(v: &Value) -> String {
    fn walk(v: &Value, out: &mut Vec<String>) {
        if let Value::Object(m) = v {
            for (k, x) in m {
                match k.as_str() {
                    "sig" | "at" | "state" => {}
                    k if k.ends_with(".init") => {}
                    "__awaitee" | "a" | "b" if x.get("state").is_some() || x.get("a").is_some() => walk(x, out),
                    _ => out.push(format!("{k}={x}")),
                }
            }
        }
    }
    let mut out = vec![];
    walk(v, &mut out);
    out.join(" ")
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
    let moderation = moderation_runtime()?;
    let scanner = scanner_runtime()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or_default();
    match arg(0) {
        "submit" => {
            let item = Item { id: arg(1).into(), client: arg(2).into(), content: args[3..].join(" ") };
            match moderation.start(arg(1), moderate(item)).await? {
                Outcome::Done(a) => println!("{}: {}", arg(1), json!(a)),
                Outcome::Parked => print_status(&moderation, arg(1), &moderation.status(arg(1))?),
            }
        }
        "run" => {
            let until = durable::now_ms() + (arg(1).parse::<f64>().unwrap_or(1.0) * 1000.0) as u64;
            while durable::now_ms() < until {
                let n = moderation.work().await? + scanner_round(&moderation, &scanner).await?;
                if n == 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
        "pending" => {
            for o in moderation.open_requests()?.iter().filter(|o| o.kind != "malware_scan") {
                println!("{} #{} {}: {}", o.task_id, o.req_id, o.kind, o.payload);
            }
        }
        "deliver" => {
            let v: Value = serde_json::from_str(arg(3))?;
            print_status(&moderation, arg(1), &moderation.deliver(arg(1), arg(2).parse()?, v).await?);
        }
        "status" => print_status(&moderation, arg(1), &moderation.status(arg(1))?),
        _ => eprintln!("usage: moderation submit|run|pending|deliver|status ..."),
    }
    Ok(())
}
