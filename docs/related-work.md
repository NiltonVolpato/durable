# Related work

## Infinity (Hydro)

<https://infinity.hydro.run/> · <https://github.com/hydro-project/infinity>. The inspiration for this project. A Rust runtime for concurrent LLM agents that "yield while they wait" and "cost nothing until the next event arrives". It runs the same agent code in a resident process or on AWS Lambda.

How it works (from `docs/docs/infinity-runtime/architecture.md` in the repo):

- **Agent state is data.** Between turns, an agent is its conversation history in a `ConversationStore`, plus processed-message ids in a `StateStore`. In their words: "There is no in-process state machine that tracks 'waiting for tool X'."
- **Slices.** A slice is one or more input messages for a thread: load history → run one model completion → persist the turn → dispatch the tool call → exit. The model's decision to call a tool is the yield point.
- **Tools don't hold connections.** A tool call is a fire-and-forget HTTP POST with a callback URL (their Reactive Agent Protocol). The result comes back as a new input message, whether it takes 100 ms or three days. Tools that finish in microseconds can run synchronously: the slice loops back into another completion instead of yielding.
- **Everything is a message.** User text, tool results, subscription events, child-thread reports and timer wake-ups are all `InputMessage`s, keyed by thread (`group_id`).
- **Durability and delivery:**
  - the turn is flushed before the tool call is sent, so the call is durable before its result can arrive;
  - messages carry stable ids, and redeliveries are skipped;
  - per-thread FIFO groups (SQS FIFO on AWS) serialize each thread's slices without locks.

### Compared with durable futures

| | Infinity | Durable futures |
|---|---|---|
| What is suspended | Conversation history (data by design) | Any async Rust control flow, captured from the compiler's coroutine state |
| Where the control flow lives | The LLM loop, which is fixed | User code: loops, joins, deadlines, retries |
| Mechanism | Load → step → persist → exit | Serialize the frame at a slow await; rebuild it and keep polling |
| Price | Only fits agent-shaped programs | rustc internals, and a long tail of language cases |
| Maturity | A working runtime: stores, queues, Lambda | Alpha: single-threaded runtime on SQLite |

Infinity sidesteps capturing program state: for an LLM agent, the control state *is* the transcript, so it is data from the start. Durable futures target the opposite case: control flow written in Rust, with state that only the compiler can see.

The two fit together. Orchestration that does not belong to the model could be written as a durable `async fn` inside an Infinity-style runtime: multi-step tool sequences, parallel calls with deadlines, retries.

Its runtime design shapes our v2 plan: messages, per-task FIFO, processed ids, and persist-before-dispatch. See [runtime](runtime.md#lessons-from-infinity).

## Flawless

<https://flawless.dev/>. Durable Rust workflows by replay. Workflows are compiled to WebAssembly so that the code itself is deterministic. The results of side effects (HTTP calls, random numbers) are logged. After a crash the workflow re-runs from the start, and logged results replace the calls. A side effect that may or may not have happened returns `RequestInterrupted`, unless it is marked `.idempotent()`, in which case it is retried. The examples:

- `extend_subscription`: charge via Stripe → email the invoice → extend the subscription. A crash in between leaves the system inconsistent without durability.
- a long HTTP POST, plain and idempotent;
- a workflow that picks a random xkcd comic, which is deterministic only because `rand` goes through the log.

The same guarantees here, without replay: each side effect is a request, stored before it runs, with its result stored before the next step. A crash repeats at most the one step in flight, with the same idempotency key (`apps/billing`). Code between steps can use `rand` and the clock directly.

## Lunatic

<https://github.com/lunatic-solutions/lunatic>. An Erlang-style runtime on WebAssembly: lightweight, preemptively scheduled processes with isolation, supervision and distribution. Its examples are about processes (HTTP handlers, websockets, background email). It does not make processes durable.

## Hand-written state machines plus replayed scripts

A pattern seen in a large production system for the same use case:

- Slow steps (malware scanning, ~15 minutes; manual review, 1–3 days) are hand-written state machines.
- Per-client logic (user code, in Python and later Go) decides what happens after each result. It is re-executed from the start on every resume, with earlier results fed back in.

That is replay, with its classic failure: any non-determinism in the client code (randomness, the clock, a config read) can take a different path on resume. The same problem appears twice:

- in the client logic that decides the next step;
- in the services that implement the slow calls, which keep their own hand-written state machines.

`apps/moderation` models both sides as durable workflows.

## Replay-based durable execution

Engines such as Temporal, Restate and DBOS make workflows durable by journaling the results of steps. On recovery they re-run the workflow code and replay results from the journal. The workflow must therefore be deterministic, and code changes must keep old journals replayable.

Durable futures store the suspended state itself, so there is no replay and no determinism requirement. The cost is depending on the compiler's coroutine layout instead of on the program's observable behavior.
