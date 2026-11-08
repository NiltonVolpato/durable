//! The runtime: runs workflows, parks them in SQLite at slow calls, and
//! resumes them when a result is delivered or a timer is due.
//!
//! - `start` runs the fast path in memory. Nothing is stored unless the task
//!   parks on a slow call.
//! - On park, the serialized task and its new requests are stored in one
//!   transaction, and only then dispatched (outbox).
//! - `deliver` records a result once (duplicates are ignored) and runs a
//!   slice: load, rebuild, poll until done or parked again, store.
//! - Slices of one task never overlap (a lock per task id).
//!
//! Single-threaded: use a current-thread tokio runtime (or a `LocalSet`).

use crate::glue::{self, Entry};
use crate::task::{TaskCtx, enter};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// A request sent by a parked task. Its result comes back through `Runtime::deliver`.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub task_id: String,
    pub req_id: u64,
    pub kind: String,
    pub payload: Value,
}

#[derive(Debug)]
pub enum Outcome<T> {
    /// Finished on the fast path; nothing was stored.
    Done(T),
    /// Parked on a slow call; stored and waiting.
    Parked,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Waiting on these requests and/or a timer (ms since the epoch).
    /// `error` is set if the last resume attempt failed (e.g. incompatible build).
    Parked { waiting: Vec<u64>, wake_at: Option<u64>, error: Option<String> },
    Done(Value),
    Failed(String),
}

#[derive(Debug)]
pub enum Error {
    /// No glue for this workflow: not `#[durable::workflow]`, or not built with `cargo durable build`.
    NoGlue(String),
    /// The workflow was not registered with `Runtime::register`.
    NotRegistered(String),
    /// The task parked in a state that cannot be serialized. The task failed.
    NotSerializable(String),
    /// The stored task does not match this build. It stays parked.
    Incompatible(String),
    /// The workflow panicked. The task failed.
    Panicked(String),
    UnknownTask(String),
    UnknownRequest(String, u64),
    TaskExists(String),
    Store(rusqlite::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoGlue(m) | Error::Incompatible(m) => write!(f, "{m}"),
            Error::NotRegistered(n) => write!(f, "workflow {n} is not registered (Runtime::register)"),
            Error::NotSerializable(m) => write!(f, "task parked in a state that cannot be stored: {m}"),
            Error::Panicked(m) => write!(f, "workflow panicked: {m}"),
            Error::UnknownTask(t) => write!(f, "unknown task {t}"),
            Error::UnknownRequest(t, r) => write!(f, "task {t} has no request {r}"),
            Error::TaskExists(t) => write!(f, "task {t} already exists"),
            Error::Store(e) => write!(f, "store: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Store(e)
    }
}

/// `async fn`s usable as workflow roots, by arity. Only names the future type.
pub trait Workflow<Args> {
    type Fut: Future;
}

macro_rules! impl_workflow {
    ($($a:ident),*) => {
        impl<W, Fut, $($a),*> Workflow<($($a,)*)> for W
        where
            W: Fn($($a),*) -> Fut,
            Fut: Future,
        {
            type Fut = Fut;
        }
    };
}
impl_workflow!();
impl_workflow!(A);
impl_workflow!(A, B);
impl_workflow!(A, B, C);
impl_workflow!(A, B, C, D);

/// A rebuilt task, with its root type erased.
trait Erased {
    fn poll(&mut self, cx: &mut Context) -> Poll<Value>;
    fn serialize(&self) -> glue::Result<Value>;
}

struct Live<F: Future> {
    fut: Pin<Box<F>>,
    entry: &'static Entry,
}

impl<F: Future> Erased for Live<F>
where
    F::Output: Serialize,
{
    fn poll(&mut self, cx: &mut Context) -> Poll<Value> {
        self.fut.as_mut().poll(cx).map(|o| serde_json::to_value(o).expect("workflow output must serialize"))
    }
    fn serialize(&self) -> glue::Result<Value> {
        glue::serialize(self.entry, &self.fut)
    }
}

type Resume = Box<dyn Fn(&Value) -> glue::Result<Box<dyn Erased>>>;

/// Result of polling a task until it finishes or parks.
enum Step<T> {
    Done(T),
    Parked,
    Panicked(String),
}

async fn drive<T>(ctx: &mut TaskCtx, mut poll: impl FnMut(&mut Context) -> Poll<T>) -> Step<T> {
    std::future::poll_fn(|cx| {
        ctx.parked = false;
        match enter(ctx, || catch_unwind(AssertUnwindSafe(|| poll(cx)))) {
            Err(p) => {
                let msg = p
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| p.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "panic".into());
                Poll::Ready(Step::Panicked(msg))
            }
            Ok(Poll::Ready(v)) => Poll::Ready(Step::Done(v)),
            Ok(Poll::Pending) if ctx.parked => Poll::Ready(Step::Parked),
            Ok(Poll::Pending) => Poll::Pending, // fast work: tokio wakes us
        }
    })
    .await
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY,
    root TEXT NOT NULL,
    status TEXT NOT NULL,            -- parked | done | failed
    frame TEXT,                      -- serialized task while parked
    next_req INTEGER NOT NULL DEFAULT 0,
    wake_at INTEGER,                 -- earliest durable timer, ms
    output TEXT,
    error TEXT
);
CREATE TABLE IF NOT EXISTS requests (
    task_id TEXT NOT NULL,
    req_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    dispatched INTEGER NOT NULL DEFAULT 0,
    result TEXT,
    consumed INTEGER NOT NULL DEFAULT 0,
    cancelled INTEGER NOT NULL DEFAULT 0,  -- abandoned by the task (e.g. lost a select)
    PRIMARY KEY (task_id, req_id)
);
";

pub struct Runtime {
    db: Connection,
    glue: &'static [Entry],
    roots: HashMap<&'static str, Resume>,
    dispatch: Box<dyn Fn(&Outgoing)>,
    locks: RefCell<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Runtime {
    /// Open (or create) a store. `glue` is the table from `durable::glue!()`.
    /// `dispatch` sends a parked task's requests; it runs after they are stored,
    /// and again for undispatched ones on `redispatch` (so: at least once).
    pub fn open(path: &str, glue: &'static [Entry], dispatch: impl Fn(&Outgoing) + 'static) -> Result<Self, Error> {
        let db = Connection::open(path)?;
        db.execute_batch(SCHEMA)?;
        Ok(Runtime { db, glue, roots: HashMap::new(), dispatch: Box::new(dispatch), locks: RefCell::default() })
    }

    /// Register a workflow root, so that its parked tasks can be resumed.
    /// Fails if the binary was not built with its glue.
    pub fn register<W, Args>(&mut self, _workflow: W) -> Result<(), Error>
    where
        W: Workflow<Args>,
        W::Fut: 'static,
        <W::Fut as Future>::Output: Serialize,
    {
        let entry = glue::find::<W::Fut>(self.glue).map_err(Error::NoGlue)?;
        let resume: Resume = Box::new(move |v| {
            let fut = glue::deserialize::<W::Fut>(entry, v)?;
            Ok(Box::new(Live { fut, entry }) as Box<dyn Erased>)
        });
        self.roots.insert(entry.type_name, resume);
        Ok(())
    }

    fn lock(&self, task_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.borrow_mut().entry(task_id.to_string()).or_default().clone()
    }

    /// Run a workflow. Returns its output if it finishes on the fast path;
    /// otherwise it is stored under `task_id` and resumed by `deliver` / timers.
    pub async fn start<F>(&self, task_id: &str, fut: F) -> Result<Outcome<F::Output>, Error>
    where
        F: Future + 'static,
        F::Output: Serialize,
    {
        let entry = glue::find::<F>(self.glue).map_err(Error::NoGlue)?;
        if !self.roots.contains_key(entry.type_name) {
            return Err(Error::NotRegistered(entry.type_name.into()));
        }
        let lock = self.lock(task_id);
        let _guard = lock.lock().await;
        let mut fut = Box::pin(fut);
        let mut ctx = TaskCtx::default();
        match drive(&mut ctx, |cx| fut.as_mut().poll(cx)).await {
            Step::Done(v) => Ok(Outcome::Done(v)),
            Step::Panicked(m) => Err(Error::Panicked(m)),
            Step::Parked => {
                let frame = glue::serialize(entry, &fut).map_err(Error::NotSerializable)?;
                drop(fut);
                self.store_parked(task_id, Some(entry.type_name), &frame, &ctx)?;
                self.dispatch_task(task_id)?;
                Ok(Outcome::Parked)
            }
        }
    }

    /// Deliver the result of request `req_id` of `task_id`, and run the task
    /// until it finishes or parks again. A repeated delivery is ignored.
    pub async fn deliver(&self, task_id: &str, req_id: u64, result: impl Serialize) -> Result<Status, Error> {
        let lock = self.lock(task_id);
        let _guard = lock.lock().await;
        let prev: Option<(Option<String>, bool)> = self
            .db
            .query_row(
                "SELECT result, cancelled FROM requests WHERE task_id = ?1 AND req_id = ?2",
                params![task_id, req_id as i64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match prev {
            None => return Err(Error::UnknownRequest(task_id.into(), req_id)),
            // Duplicate, or the task no longer waits for it: ignore.
            Some((Some(_), _)) | Some((_, true)) => return self.status(task_id),
            Some((None, false)) => {}
        }
        let result = serde_json::to_string(&result).expect("result must serialize to JSON");
        self.db.execute(
            "UPDATE requests SET result = ?3 WHERE task_id = ?1 AND req_id = ?2",
            params![task_id, req_id as i64, result],
        )?;
        self.slice(task_id).await
    }

    /// Run a parked task again (e.g. after a failed resume was fixed by
    /// deploying a compatible build).
    pub async fn resume(&self, task_id: &str) -> Result<Status, Error> {
        let lock = self.lock(task_id);
        let _guard = lock.lock().await;
        self.slice(task_id).await
    }

    /// Resume every parked task whose timer is due. Returns their task ids and outcomes.
    pub async fn run_due_timers(&self) -> Result<Vec<(String, Result<Status, Error>)>, Error> {
        let ids: Vec<String> = self
            .db
            .prepare("SELECT id FROM tasks WHERE status = 'parked' AND wake_at <= ?1")?
            .query_map(params![crate::now_ms() as i64], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let mut out = vec![];
        for id in ids {
            let lock = self.lock(&id);
            let _guard = lock.lock().await;
            let r = self.slice(&id).await;
            out.push((id, r));
        }
        Ok(out)
    }

    /// The earliest timer of any parked task, ms since the epoch.
    pub fn next_wake(&self) -> Result<Option<u64>, Error> {
        let w: Option<i64> =
            self.db.query_row("SELECT MIN(wake_at) FROM tasks WHERE status = 'parked'", [], |r| r.get(0))?;
        Ok(w.map(|w| w as u64))
    }

    /// Dispatch requests that were stored but not dispatched (e.g. after a crash).
    pub fn redispatch(&self) -> Result<usize, Error> {
        let tasks: Vec<String> = self
            .db
            .prepare("SELECT DISTINCT task_id FROM requests WHERE dispatched = 0 AND cancelled = 0")?
            .query_map([], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        let mut n = 0;
        for t in tasks {
            n += self.dispatch_task(&t)?;
        }
        Ok(n)
    }

    /// Requests that are waiting for a result.
    pub fn open_requests(&self) -> Result<Vec<Outgoing>, Error> {
        let rows = self
            .db
            .prepare(
                "SELECT r.task_id, r.req_id, r.kind, r.payload FROM requests r JOIN tasks t ON t.id = r.task_id
                 WHERE r.result IS NULL AND r.cancelled = 0 AND t.status = 'parked' ORDER BY r.task_id, r.req_id",
            )?
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .map(|(task_id, req_id, kind, payload)| Outgoing {
                task_id,
                req_id: req_id as u64,
                kind,
                payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
            })
            .collect())
    }

    pub fn status(&self, task_id: &str) -> Result<Status, Error> {
        let row: Option<(String, Option<String>, Option<String>, Option<i64>)> = self
            .db
            .query_row("SELECT status, output, error, wake_at FROM tasks WHERE id = ?1", params![task_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?;
        let Some((status, output, error, wake_at)) = row else { return Err(Error::UnknownTask(task_id.into())) };
        Ok(match status.as_str() {
            "done" => Status::Done(output.and_then(|o| serde_json::from_str(&o).ok()).unwrap_or(Value::Null)),
            "failed" => Status::Failed(error.unwrap_or_default()),
            _ => {
                let waiting = self
                    .db
                    .prepare("SELECT req_id FROM requests WHERE task_id = ?1 AND result IS NULL AND cancelled = 0 ORDER BY req_id")?
                    .query_map(params![task_id], |r| r.get::<_, i64>(0))?
                    .map(|r| r.map(|v| v as u64))
                    .collect::<Result<_, _>>()?;
                Status::Parked { waiting, wake_at: wake_at.map(|w| w as u64), error }
            }
        })
    }

    /// The output of a finished task, if it finished.
    pub fn output<T: DeserializeOwned>(&self, task_id: &str) -> Result<Option<T>, Error> {
        Ok(match self.status(task_id)? {
            Status::Done(v) => serde_json::from_value(v).ok(),
            _ => None,
        })
    }

    /// The stored state of a parked task (for inspection).
    pub fn snapshot(&self, task_id: &str) -> Result<Option<Value>, Error> {
        let f: Option<Option<String>> = self
            .db
            .query_row("SELECT frame FROM tasks WHERE id = ?1", params![task_id], |r| r.get(0))
            .optional()?;
        Ok(f.flatten().and_then(|f| serde_json::from_str(&f).ok()))
    }

    // ---------- internals ----------

    /// Load a parked task, run it until it finishes or parks, store the result.
    /// The caller holds the task's lock.
    async fn slice(&self, task_id: &str) -> Result<Status, Error> {
        let row: Option<(String, String, Option<String>, i64)> = self
            .db
            .query_row("SELECT root, status, frame, next_req FROM tasks WHERE id = ?1", params![task_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .optional()?;
        let Some((root, status, frame, next_req)) = row else { return Err(Error::UnknownTask(task_id.into())) };
        if status != "parked" {
            return self.status(task_id);
        }
        let resume = self.roots.get(root.as_str()).ok_or_else(|| Error::NotRegistered(root.clone()))?;
        let frame: Value = serde_json::from_str(&frame.unwrap_or_default()).unwrap_or(Value::Null);
        let mut task = match resume(&frame) {
            Ok(t) => t,
            Err(e) => {
                // Incompatible build: keep it parked for a build that can resume it.
                self.db.execute("UPDATE tasks SET error = ?2 WHERE id = ?1", params![task_id, e])?;
                return Err(Error::Incompatible(e));
            }
        };
        let inbox: HashMap<u64, Value> = self
            .db
            .prepare("SELECT req_id, result FROM requests WHERE task_id = ?1 AND result IS NOT NULL AND consumed = 0")?
            .query_map(params![task_id], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?)))?
            .map(|r| r.map(|(id, v)| (id, serde_json::from_str(&v).unwrap_or(Value::Null))))
            .collect::<Result<_, _>>()?;
        let mut ctx = TaskCtx { next_req: next_req as u64, inbox, ..Default::default() };
        match drive(&mut ctx, |cx| task.poll(cx)).await {
            Step::Done(out) => {
                drop(task);
                let tx = self.db.unchecked_transaction()?;
                Self::mark_consumed(&tx, task_id, &ctx)?;
                tx.execute(
                    "UPDATE tasks SET status = 'done', frame = NULL, wake_at = NULL, error = NULL, output = ?2 WHERE id = ?1",
                    params![task_id, out.to_string()],
                )?;
                tx.commit()?;
                Ok(Status::Done(out))
            }
            Step::Panicked(m) => {
                drop(task);
                self.fail(task_id, &format!("panicked: {m}"))?;
                Err(Error::Panicked(m))
            }
            Step::Parked => match task.serialize() {
                Ok(frame) => {
                    drop(task);
                    self.store_parked(task_id, None, &frame, &ctx)?;
                    self.dispatch_task(task_id)?;
                    self.status(task_id)
                }
                Err(e) => {
                    drop(task);
                    self.fail(task_id, &format!("parked in a state that cannot be stored: {e}"))?;
                    Err(Error::NotSerializable(e))
                }
            },
        }
    }

    /// Record which requests this slice consumed or abandoned.
    fn mark_consumed(tx: &rusqlite::Transaction, task_id: &str, ctx: &TaskCtx) -> Result<(), Error> {
        for id in &ctx.consumed {
            tx.execute("UPDATE requests SET consumed = 1 WHERE task_id = ?1 AND req_id = ?2", params![task_id, *id as i64])?;
        }
        for id in &ctx.cancelled {
            tx.execute("UPDATE requests SET cancelled = 1 WHERE task_id = ?1 AND req_id = ?2", params![task_id, *id as i64])?;
        }
        Ok(())
    }

    /// Store a parked task and its new requests in one transaction.
    /// `new_root` is set for a task that was never stored.
    fn store_parked(&self, task_id: &str, new_root: Option<&str>, frame: &Value, ctx: &TaskCtx) -> Result<(), Error> {
        let tx = self.db.unchecked_transaction()?;
        let frame = frame.to_string();
        let wake = ctx.wake_at.map(|w| w as i64);
        match new_root {
            Some(root) => {
                let inserted = tx.execute(
                    "INSERT OR IGNORE INTO tasks (id, root, status, frame, next_req, wake_at) VALUES (?1, ?2, 'parked', ?3, ?4, ?5)",
                    params![task_id, root, frame, ctx.next_req as i64, wake],
                )?;
                if inserted == 0 {
                    return Err(Error::TaskExists(task_id.into()));
                }
            }
            None => {
                tx.execute(
                    "UPDATE tasks SET frame = ?2, next_req = ?3, wake_at = ?4, error = NULL WHERE id = ?1",
                    params![task_id, frame, ctx.next_req as i64, wake],
                )?;
            }
        }
        for o in &ctx.outbox {
            tx.execute(
                "INSERT INTO requests (task_id, req_id, kind, payload) VALUES (?1, ?2, ?3, ?4)",
                params![task_id, o.req_id as i64, o.kind, o.payload.to_string()],
            )?;
        }
        Self::mark_consumed(&tx, task_id, ctx)?;
        tx.commit()?;
        Ok(())
    }

    /// Send this task's stored, undispatched requests.
    fn dispatch_task(&self, task_id: &str) -> Result<usize, Error> {
        let pending = self
            .db
            .prepare("SELECT req_id, kind, payload FROM requests WHERE task_id = ?1 AND dispatched = 0 AND cancelled = 0 ORDER BY req_id")?
            .query_map(params![task_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        for (req_id, kind, payload) in &pending {
            (self.dispatch)(&Outgoing {
                task_id: task_id.into(),
                req_id: *req_id as u64,
                kind: kind.clone(),
                payload: serde_json::from_str(payload).unwrap_or(Value::Null),
            });
            self.db.execute(
                "UPDATE requests SET dispatched = 1 WHERE task_id = ?1 AND req_id = ?2",
                params![task_id, req_id],
            )?;
        }
        Ok(pending.len())
    }

    fn fail(&self, task_id: &str, error: &str) -> Result<(), Error> {
        self.db.execute(
            "UPDATE tasks SET status = 'failed', frame = NULL, wake_at = NULL, error = ?2 WHERE id = ?1",
            params![task_id, error],
        )?;
        Ok(())
    }
}
