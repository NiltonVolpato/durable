//! Flawless's `extend_subscription`, as a durable workflow: charge the card,
//! email the invoice, extend the subscription. A crash between any two steps
//! must not lose the subscription or charge the card twice.
//!
//! Each side effect is a request run by a handler. The task is stored before
//! the request runs, and the result is stored before the next step, so a
//! crash re-runs at most the one step that was in flight. That step gets
//! the same idempotency key, `(task_id, req_id)`, so the fake Stripe and
//! mailer below do not repeat it. The workflow code itself is never re-run.
//!
//!   billing start <user> <months>
//!   billing work                       (CRASH_AFTER=<kind> exits right after that side effect)
//!   billing status <user>
//!   billing ledger                     what the outside world saw

use durable::{Outcome, Runtime, Status};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

durable::glue!();

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Charge {
    id: String,
    cents: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Receipt {
    charge: String,
    paid_until: String,
}

#[durable::workflow]
async fn extend_subscription(user: String, months: u32) -> Receipt {
    let charge: Charge = durable::request("stripe_charge", (user.clone(), 999 * months as u64)).await;
    let _sent: bool = durable::request("send_invoice", (user.clone(), charge.clone())).await;
    let paid_until: String = durable::request("extend", (user.clone(), months)).await;
    Receipt { charge: charge.id, paid_until }
}

/// The outside world: Stripe, the mailer and the accounts DB, in ledger.json.
#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    /// idempotency key -> charge
    charges: BTreeMap<String, Charge>,
    /// idempotency key -> email
    emails: BTreeMap<String, String>,
    /// user -> months paid
    months: BTreeMap<String, u32>,
    /// idempotency keys already applied to `months`
    extended: Vec<String>,
}

fn ledger() -> Ledger {
    std::fs::read_to_string("ledger.json").ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn save(l: &Ledger) {
    std::fs::write("ledger.json", serde_json::to_string_pretty(l).unwrap()).unwrap();
}

/// Simulate a crash right after a side effect, before its result is stored.
fn maybe_crash(kind: &str) {
    if std::env::var("CRASH_AFTER").as_deref() == Ok(kind) {
        println!("  !! crash after {kind} (its result was not stored)");
        std::process::exit(1);
    }
}

fn runtime() -> Result<Runtime, durable::Error> {
    let mut rt = Runtime::open("billing.db", GLUE, |o| println!("  -> unexpected request {}", o.kind))?;
    rt.register(extend_subscription)?;
    rt.handle("stripe_charge", |o| async move {
        let key = format!("{}#{}", o.task_id, o.req_id);
        let (user, cents): (String, u64) = serde_json::from_value(o.payload).map_err(|e| e.to_string())?;
        let mut l = ledger();
        if l.charges.contains_key(&key) {
            println!("  stripe: already charged for key {key}, returning that charge");
        }
        let charge = l.charges.entry(key.clone()).or_insert_with(|| {
            println!("  stripe: charged {user} {cents} cents (key {key})");
            Charge { id: format!("ch_{}", key.replace('#', "_")), cents }
        });
        let charge = charge.clone();
        save(&l);
        maybe_crash("stripe_charge");
        Ok::<_, String>(charge)
    });
    rt.handle("send_invoice", |o| async move {
        let key = format!("{}#{}", o.task_id, o.req_id);
        let (user, charge): (String, Charge) = serde_json::from_value(o.payload).map_err(|e| e.to_string())?;
        let mut l = ledger();
        if !l.emails.contains_key(&key) {
            println!("  mailer: invoice for {} sent to {user}", charge.id);
            l.emails.insert(key, format!("invoice {} to {user}", charge.id));
        } else {
            println!("  mailer: invoice already sent for key {key}, not sending again");
        }
        save(&l);
        maybe_crash("send_invoice");
        Ok::<_, String>(true)
    });
    rt.handle("extend", |o| async move {
        let key = format!("{}#{}", o.task_id, o.req_id);
        let (user, months): (String, u32) = serde_json::from_value(o.payload).map_err(|e| e.to_string())?;
        let mut l = ledger();
        if !l.extended.contains(&key) {
            *l.months.entry(user.clone()).or_default() += months;
            l.extended.push(key);
        }
        let total = l.months[&user];
        save(&l);
        maybe_crash("extend");
        Ok::<_, String>(format!("{total} months paid"))
    });
    Ok(rt)
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
    let rt = runtime()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or_default();
    match arg(0) {
        "start" => match rt.start(arg(1), extend_subscription(arg(1).into(), arg(2).parse()?)).await? {
            Outcome::Done(r) => println!("{}: {r:?}", arg(1)),
            Outcome::Parked => println!("{}: started, waiting on {:?}", arg(1), rt.status(arg(1))?),
        },
        "work" => {
            rt.work().await?;
        }
        "status" => match rt.status(arg(1))? {
            Status::Done(v) => println!("{}: done: {v}", arg(1)),
            other => println!("{}: {other:?}", arg(1)),
        },
        "ledger" => println!("{}", serde_json::to_string_pretty(&ledger())?),
        _ => eprintln!("usage: billing start|work|status|ledger ..."),
    }
    Ok(())
}
