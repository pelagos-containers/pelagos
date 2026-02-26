# Ongoing Tasks

## Current Task: Dual Executor Model Complete (Feb 25, 2026) ✅

### Context

The previous session (Feb 25, 2026) completed the Future/Executor model for
declarative container orchestration. The session ended mid-design-discussion
about how `then` should behave when its lambda returns a Future (monadic bind
style, a.k.a. Promise chaining).

The open question: **static graph** (full graph declared upfront, topo-sortable
before any execution) vs **dynamic graph** (lazy unfolding — `then`'s lambda
is not called until its upstream resolves, so returned Futures are discovered at
runtime).

The user confirmed interest in the dynamic/monadic approach:
> "yes, I believe 'then's lambda would return a Future' describes it.
>  Let's play with that"
> "I have questions around dynamic resolution vs static upfront evaluation"

---

### What was completed this session (Feb 25, 2026)

All work is on `main`. No tag yet — waiting for design to stabilise.

#### stdlib quality-of-life macros

- `unless` — `(unless test body...)` → `(when (not test) body...)`
- `zero?` — `(zero? x)` → `(= x 0)`
- `logf` — `(logf fmt arg...)` → `(log (format fmt arg...))` (reduces `(log (format ...))` noise)
- `errorf` — `(errorf fmt arg...)` → `(error (format fmt arg...))` (same for errors)
- Updated all usages in stdlib and example files

#### Result type (stdlib.lisp)

Tagged list ADT, like Rust's `Result<T,E>`:

```lisp
(define (ok  v) (list 'ok  v))
(define (err r) (list 'err r))
(define (ok?  r) (and (pair? r) (eq? (car r) 'ok)))
(define (err? r) (and (pair? r) (eq? (car r) 'err)))
(define (ok-value  r) (cadr r))
(define (err-reason r) (cadr r))
```

#### `with-cleanup` updated signature

Cleanup lambda now receives a `Result` (not a zero-arg thunk):

```lisp
(defmacro with-cleanup (cleanup . body)
  `(guard (exn (#t (,cleanup (err exn)) (error exn)))
     (let ((result (begin ,@body)))
       (,cleanup (ok result))
       result)))
```

#### Self-evaluating keywords (eval.rs)

Symbols starting with `:` now evaluate to themselves (like Clojure keywords).
This was required for `:after`, `:inject`, `:port`, `:timeout` to work in
`container-start-async` / `await` calls without being looked up in the env.

```rust
// in eval.rs, atom eval branch:
if s.starts_with(':') {
    return Ok(Step::Done(Value::Symbol(s.clone())));
}
```

#### `Value::Future` and `FutureKind` (value.rs)

```rust
pub enum FutureKind {
    Container {
        spec:   Box<crate::compose::ServiceSpec>,
        inject: Option<Box<Value>>,          // Boxed to break recursive cycle
    },
    Transform {
        upstream_id: u64,
        transform:   Box<Value>,             // Boxed to break recursive cycle
    },
}

// Value::Future variant:
Future {
    id:    u64,
    name:  String,
    kind:  FutureKind,
    after: Vec<u64>,
}
```

#### `container-start-async`, `then`, `run-all`, `await` (runtime.rs)

- `container-start-async svc [:after list] [:inject lambda]` → `Value::Future`
- `then future lambda` → `Value::Future { kind: Transform }`, auto `:after` upstream
- `run-all (list fut ...)` → alist of `(name . resolved-value)`, Kahn topo-sort
- `await future [:port P] [:timeout T]` → `ContainerHandle`, errors on Transform futures

#### `result-ref` and `assoc` (stdlib.lisp)

- `assoc` — standard alist lookup
- `result-ref` — `(result-ref results "name")` extracts from `run-all` alist; errors if missing

#### examples/compose/imperative/compose.reml

Full graph model:
```lisp
(define db-url-fut
  (then db-fut
    (lambda (db)
      (format "postgres://app:secret@~a/appdb" (container-ip db)))))

(define app-fut
  (container-start-async svc-app
    :after  (list db-url-fut cache-url-fut)
    :inject (lambda (db-url cache-url)
              (list (cons "DATABASE_URL" db-url)
                    (cons "CACHE_URL"    cache-url)))))

(define results
  (run-all (list db-fut cache-fut db-url-fut cache-url-fut migrate-fut app-fut)))
```

#### docs/REML_EXECUTOR_MODEL.md (NEW)

Design doc covering: motivation, Futures/Executors model, π-calculus connection,
FutureKind API reference, execution order (serial vs parallel), design principles,
roadmap.

---

### Open Design Question: Static vs Dynamic `then`

**Current model (static):**
- `then`'s lambda returns a plain value (string, number, etc.)
- The entire graph is declared before `run-all` is called
- Topo-sort sees all futures upfront; cycle detection is complete
- Parallel executor could be added without changing `.reml` files

**Proposed model (dynamic/monadic):**
- `then`'s lambda returns a Future (or a plain value — the executor checks)
- When a Transform future resolves to another Future, that Future is added to
  the work queue dynamically
- Graph is discovered lazily: you don't see futures in `run-all` until their
  upstream completes

**Chain syntax the user envisions:**

```lisp
;; Monadic chain: db → migrate → app, with URL threading
(define pipeline
  (then db-fut
    (lambda (db)
      (let ((db-url (format "postgres://...@~a/appdb" (container-ip db))))
        (then (container-start-async svc-migrate
                :env (list (cons "DATABASE_URL" db-url)))
          (lambda (_)
            (container-start-async svc-app
              :env (list (cons "DATABASE_URL" db-url)))))))))
```

**Key trade-offs to discuss:**

| | Static graph | Dynamic (monadic) |
|---|---|---|
| Upfront cycle detection | ✅ yes | ❌ no |
| Parallel dispatch (known tiers) | ✅ yes | ⚠️ harder |
| `then-all` join (multi-upstream) | ✅ trivial | ⚠️ needs design |
| Chain syntax | ❌ no (names needed) | ✅ yes |
| Graph introspection | ✅ yes | ❌ not upfront |
| Incremental disclosure | ❌ no | ✅ yes |

**Proposed resolution:**
Support both — a `resolve` entry point that executes a single chain dynamically,
plus `run-all` for static graphs. The two can coexist: `run-all` remains the
preferred form for complex multi-service graphs where upfront analysis is valuable;
monadic `then` enables simple pipelines without requiring explicit `run-all`.

---

### What was completed (Feb 25, 2026)

Both executors now implemented and documented:

**Data model change:**
- `after: Vec<u64>` → `after: Vec<Value>` in `Value::Future` — futures now store
  their upstream futures as values, enabling recursive graph traversal without a
  registry
- `upstream_id: u64` → `upstream: Box<Value>` in `FutureKind::Transform` — same
  reason; allows `resolve` to walk chains recursively
- Added `Value::future_id() -> Option<u64>` helper for extracting IDs
- `run-all` topo-sort updated to extract IDs via `filter_map(Value::future_id)`

**`resolve` builtin added (runtime.rs):**
- Free function `resolve_dynamic` implements recursive depth-first execution
- Container futures: spawns container, returns `ContainerHandle`
- Transform futures: resolves upstream first, calls lambda; if result is a `Future`,
  resolves that too (monadic flatten)
- Deduplication map prevents re-executing shared upstreams

**New example:** `examples/compose/imperative/compose-chain.reml`
- Same 3-service stack as compose.reml but using monadic chain style
- Annotated to explain when to use `resolve` vs `run-all`

**Docs:** `docs/REML_EXECUTOR_MODEL.md` updated
- Comparison table: static vs dynamic
- `(resolve ...)` API reference with chain example
- "Choosing an Executor" section with decision guide

**Tests:** 232 passing, clippy clean, fmt clean

### Next steps

- Tag v0.13.0
- Add `then-all` join operator (future): `(then-all (list f1 f2) (lambda (v1 v2) ...))`
  for multi-upstream joins in the monadic style
- Developer stack examples: `node-dev/` and `forgejo/` (can now use imperative style)

---

## Previous Session: Imperative Runtime Builtins (Feb 25, 2026) ✅

### Completed

**Phase 1 — Language additions** ✅
- `(format fmt arg...)` — `~a` / `~s` formatting → string
- `(sleep secs)` — thread sleep; int or float → `()`
- `(guard (var clause...) body...)` — SRFI-34 error handling
- `(with-cleanup cleanup-thunk body...)` — try/finally stdlib macro

**Phase 2 — `Value::ContainerHandle`** ✅
- `ContainerHandle { name, pid, ip }` in `value.rs`

**Phase 3 — `src/lisp/runtime.rs`** ✅
- `container-start`, `container-stop`, `container-wait`, `container-run`,
  `container-ip`, `container-status`, `await-port`

**Phase 4 — `Interpreter::new_with_runtime(project, compose_dir)`** ✅
- `container_registry` field; `Drop` impl sends SIGTERM on interpreter drop

**Phase 5 — CLI update** ✅
- `cmd_compose_up_reml` uses `new_with_runtime`

**Phase 6 — Tests + example** ✅
- 8 new unit tests; `examples/compose/imperative/compose.reml`

---

## Session Summary (Feb 24, 2026) — git SHA 2b9bbc6

### Completed this session

- **Dotted pair syntax** — `SExpr::DottedList`, full round-trip through macro
  expansion and `value_to_sexpr`; `define-service` handles both proper lists and
  dotted pairs in value position; variadic lambda/define shorthand now uses
  `DottedList` natively
- **monitoring/ stack** — Prometheus + Loki + Grafana compose example; all 6
  smoke tests pass; fixed Grafana startup (binary name, no ini file needed),
  fixed `image rm` to try local ref first before docker.io normalization
- **rust-builder/ stack** — Alpine + rustc + cargo + sccache; named volume mounts
  for cargo registry and sccache cache; 7 smoke tests pass including sccache
  cache activity; added `:volume`, `:bind`, `:bind-ro` Lisp service options
- **215 lib tests** pass; `cargo clippy -D warnings` and `cargo fmt` clean

### Remaining developer stack backlog

Next: **`node-dev/`** → then **`forgejo/`** (see detail below).

---

## Completed: `defmacro` + `define-service` + dotted pairs (Feb 24, 2026) ✅

### Context

Add a general macro system to the Lisp interpreter, then implement `define-service`
as a Lisp macro so service definitions are concise and keyword-driven:

```lisp
(define-service svc-jupyterlab "jupyterlab"
  (:image      "jupyter-jupyterlab:latest")
  (:network    "jupyter-net")
  (:depends-on "redis" 6379)
  (:env        "REDIS_HOST" "redis")
  (:port       jupyter-port 8888)
  (:memory     mem-jupyter)
  (:cpus       cpu-jupyter))
```

### Status

**COMPLETE.** All files created, `cargo build` + `cargo clippy -- -D warnings` + `cargo fmt`
+ `cargo test --lib` (205 tests) all pass. Two integration tests pass:
`test_lisp_compose_basic` and `test_lisp_evaluator_tco_and_higher_order`. Docs updated.

---

## Pending: Developer Stack Examples (Feb 24, 2026)

### Context

Build a suite of developer-oriented compose examples under `examples/compose/`,
each with a `Remfile` per service, a `compose.reml` demonstrating Lisp features,
a `run.sh` smoke test, and a `README.md`. All stacks use Alpine base images.

### Stack Backlog (priority order)

---

#### 4. `node-dev/` — Node.js app with hot reload + PostgreSQL  ⬅ NEXT
**Status:** Not started

**Architecture:**
```
network: node-net (10.89.3.0/24)
  postgres   — port 5432 (internal only)
  node-app   — port 3000 → host; depends-on postgres:5432
```

**Remfile notes:**
- Node base: `FROM alpine:latest`; APK: `nodejs npm build-base python3 gcompat`
- `gcompat` for packages with precompiled glibc binaries
- Global: `npm install -g nodemon`
- Named volume for `node_modules` (prevents host/container platform conflicts)
- Source bind-mounted at `/app`

**compose.reml features to demonstrate:**
- `env` for `DATABASE_URL` constructed from service name
- Named volume for `node_modules` separating host and container module trees
- `on-ready "postgres"` hook: log "database ready — starting app"
- Bind-mount for live source reload

---

#### 5. `forgejo/` — Self-hosted Git (Forgejo + PostgreSQL)
**Status:** Not started

**Architecture:**
```
network: forgejo-net (10.89.4.0/24)
  postgres   — port 5432 (internal)
  forgejo    — port 3000 → host; SSH port 2222 → host; depends-on postgres:5432
```

---

### Implementation Notes (all stacks)

- Each stack lives under `examples/compose/<name>/`
- Remfiles use `FROM alpine:latest` unless Alpine is genuinely not viable
- `run.sh` pattern mirrors `examples/compose/web-stack/run.sh`
- Each `compose.reml` must use at least: `define`, `env` with fallback, `on-ready`
