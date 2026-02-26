# Remora Lisp Executor Model

**Status:** Implemented (v0.13.x)
**Location:** `src/lisp/runtime.rs`, `src/lisp/value.rs`, `src/lisp/stdlib.lisp`

---

## Motivation

Static compose formats (YAML, TOML, JSON) express *what* containers to run but
not *how* they relate at runtime.  Relationships that require runtime values —
connection strings derived from assigned IPs, migration steps that must
complete before the app starts, health checks that gate dependent services —
cannot be expressed declaratively.  They require a programming language.

Remora's `.reml` format is that language.  Earlier versions of the runtime
exposed imperative primitives (`container-start`, `container-wait`) that worked
but forced sequential execution and buried the dependency structure in
evaluation order.  This document describes the current model, which makes the
dependency graph explicit and separable from its execution policy.

---

## Core Idea: Futures and Executors

The model separates two concerns:

- **Futures** — pure descriptions of work, with no side effects on creation
- **Executors** — policies for *when* and *how* to run that work

A future is created by `container-start-async` or `then`.  It captures a
service spec, optional dependency declarations, and optional data
transformations.  Nothing happens when a future is created.

Remora ships two executors:

| Executor | Form | Graph model | Cycle detection | Best for |
|----------|------|-------------|-----------------|----------|
| Serial static | `run-all` | Full graph declared upfront | ✅ Yes (Kahn's) | Complex multi-service graphs, independent services |
| Dynamic monadic | `resolve` | Graph unfolds lazily | ❌ No | Linear pipelines, next step depends on previous value |

```
.reml file           futures (descriptions of work)
     │                     │
     ▼                     ▼
(container-start-async)  Value::Future { id, kind, after: Vec<Value> }
(then)                   Value::Future { id, kind: Transform, after }
     │                     │
     ▼                     ▼
(run-all)            (resolve)
 static, topo-sort    dynamic, recursive walk, monadic flatten
 cycle detection      no cycle detection, but simpler chain syntax
 future: parallel     future: parallel work-queue
```

---

## Connection to π-Calculus

The model is a simplified version of the π-calculus.  In π-calculus,
processes communicate by sending and receiving values on named channels.
The dependency structure of a concurrent computation *is* its communication
pattern — you don't declare a graph, the graph emerges from which names are
bound and where they are consumed.

In Remora's model:

| π-calculus concept | Remora equivalent |
|--------------------|-------------------|
| Process            | `container-start-async` future |
| Channel            | Lisp name binding (`define`) |
| Send               | Future resolving to a value |
| Receive / block    | `:after` dependency + `:inject` |
| Value on channel   | `ContainerHandle`, URL string, etc. |

`db-url` is not just a string — it is the value communicated between the
database future and the downstream futures that need it.  The executor
discovers the graph by observing which futures appear in `:after` declarations,
and which resolved values flow through `:inject` lambdas.

---

## Future Kinds

### `FutureKind::Container`

Created by `container-start-async`.  When executed, spawns a container and
resolves to a `ContainerHandle`.

Optional `:inject` lambda: called with the resolved values of all `:after`
dependencies (in declaration order).  Returns a list of `(key . value)` pairs
merged into the service environment before spawning.  This is how data
produced by upstream futures flows into downstream containers.

### `FutureKind::Transform`

Created by `then`.  When executed, calls a lambda with the resolved value
of its single upstream future.  Resolves to whatever the lambda returns — no
container is spawned.

The lambda can return either a plain value or another `Future`:

- **Plain value** — the typical case.  Use this to derive connection strings,
  port numbers, or config blobs from a `ContainerHandle` before passing them
  to downstream containers via `:inject`.
- **Another `Future`** — use this when the next container to start depends on
  a value only known at runtime (e.g. a schema version that determines whether
  migrations need to run).  The executor resolves the returned future
  automatically and uses its result as the transform's resolved value.

### `FutureKind::Join`

Created by `then-all`.  Like `Transform` but waits for multiple upstreams and
passes all their resolved values to the lambda.  The upstreams are stored in
`Value::Future { after: Vec<Value> }` — no duplication in the kind is needed.
Both executors resolve all `after` futures first, then call the lambda with the
results in declaration order.  If the lambda returns a `Future`, it is flattened
automatically.

---

## API Reference

### `(container-start-async svc [:after list] [:inject lambda])`

Returns a `Future` — nothing starts.

- `svc` — a `ServiceSpec` value (from `define-service`)
- `:after (list fut ...)` — futures that must complete before this one starts
- `:inject (lambda (dep1 dep2 ...) ...)` — called at execution time with
  resolved `:after` values; return value must be a list of `(key . value)` pairs

```lisp
(define app-fut
  (container-start-async svc-app
    :after  (list db-url-fut cache-url-fut)
    :inject (lambda (db-url cache-url)
              (list (cons "DATABASE_URL" db-url)
                    (cons "CACHE_URL"    cache-url)))))
```

### `(then future lambda)`

Returns a `Transform` future that applies `lambda` to the resolved value of
`future`.  Automatically declares `:after` the upstream future.

```lisp
(define db-url-fut
  (then db-fut
    (lambda (db)
      (format "postgres://app:secret@~a/appdb" (container-ip db)))))
```

### `(run-all (list fut ...))`

Serial executor.  Topologically sorts futures by `:after` dependencies (Kahn's
algorithm), executes each in order, passes resolved values to `:inject` and
`then` transforms.  Returns an alist of `(name . resolved-value)` pairs.

Futures not in the list whose IDs appear in `:after` declarations are treated
as already resolved — they were executed by a prior `await` or `run-all` call.

Raises an error if a dependency cycle is detected.

```lisp
(define results
  (run-all (list db-fut cache-fut db-url-fut cache-url-fut migrate-fut app-fut)))
```

### `(await future [:port P] [:timeout T])`

Single-future serial executor for `Container` futures.  Starts the container
and optionally waits for a TCP port to accept connections.  Raises an error
(not `#f`) on timeout — if you are awaiting a service, it is required.

Only works with `Container` futures; `Transform` futures must go through
`run-all`.

```lisp
(define db (await db-fut :port 5432 :timeout 60))
```

### `(result-ref results "name")`

Extract a resolved value from a `run-all` alist by service name.  Raises an
error if the name is not found.

```lisp
(define app (result-ref results "app"))
```

### `(then-all (list fut ...) lambda)`

Returns a `Join` future.  When executed, waits for all listed futures to
resolve, then calls `lambda` with their resolved values in declaration order.
If the lambda returns a `Future`, it is resolved automatically (same rule as
`then`).

Use `then-all` when a downstream step genuinely requires the values from
multiple independent upstreams — for example, an app container that needs both
a database URL and a cache URL before it can be configured.

```lisp
;; db and cache resolve independently; app needs both URLs.
(define db-url-fut    (then db-fut    (lambda (db)    (format "postgres://...@~a/db"    (container-ip db)))))
(define cache-url-fut (then cache-fut (lambda (cache) (format "redis://~a:6379"         (container-ip cache)))))

(define app-fut
  (then-all (list db-url-fut cache-url-fut)
    (lambda (db-url cache-url)
      (container-start-async svc-app
        :inject (lambda (_) (list (cons "DATABASE_URL" db-url)
                                  (cons "CACHE_URL"    cache-url)))))))

(define app (resolve app-fut))
```

In a `run-all` graph, include all futures in the list and `then-all` fits
naturally into the topo-sort — its `:after` edges point to both upstreams:

```lisp
(define results
  (run-all (list db-fut cache-fut db-url-fut cache-url-fut app-fut)))
```

### `(resolve future)`

Dynamic executor.  Resolves `future` recursively:

1. **Container future** — spawns the container; returns a `ContainerHandle`.
2. **Transform future** — resolves the upstream first, then calls the
   transform lambda with its value.  If the lambda returns another
   `Future`, that future is resolved too (**monadic flatten**).  Repeats
   until a non-`Future` value is produced.
3. **Plain value** — returned as-is.

A deduplication map prevents re-execution when two chains share an upstream.

Unlike `run-all`, the full graph need not be declared upfront.  Use `resolve`
for linear pipelines where the *value* produced by one step determines what
the next step is.

```lisp
(define pipeline
  (then db-fut
    (lambda (db)
      (let ((db-url (format "postgres://app:secret@~a/appdb" (container-ip db))))
        ;; Returning a Future here — resolve flattens it automatically.
        (then (container-start-async svc-migrate
                :inject (lambda (_) (list (cons "DATABASE_URL" db-url))))
          (lambda (_)
            (container-start-async svc-app
              :inject (lambda (_) (list (cons "DATABASE_URL" db-url))))))))))

(define app (resolve pipeline))
```

---

## Execution Order

The serial executor resolves execution order from the `:after` graph.
Given this declaration:

```lisp
(define db-fut        (container-start-async svc-db))
(define cache-fut     (container-start-async svc-cache))
(define db-url-fut    (then db-fut    (lambda (db)    ...)))
(define cache-url-fut (then cache-fut (lambda (cache) ...)))
(define migrate-fut   (container-start-async svc-migrate :after (list db-url-fut)))
(define app-fut       (container-start-async svc-app     :after (list db-url-fut cache-url-fut)))
```

The dependency graph is:

```
db-fut ──────────→ db-url-fut ──────────→ migrate-fut
                               └─────────→ app-fut
cache-fut ───────→ cache-url-fut ─────────→ app-fut
```

Serial execution order (one valid topological sort):

```
db → cache → db-url → cache-url → migrate → app
```

Parallel execution order (with a parallel executor):

```
Round 1:  db ∥ cache
Round 2:  db-url ∥ cache-url          (unblocked after round 1)
Round 3:  migrate                      (unblocked after db-url)
Round 4:  app                          (unblocked after migrate + cache-url)
```

**No changes to the `.reml` file are needed to switch between serial and
parallel execution.**  The graph is fully specified by the declarations;
the executor policy is external.

---

## Complete Example

```lisp
;; Service declarations
(define-service svc-db    "db"    :image "postgres:16"    :network "app-net" ...)
(define-service svc-cache "cache" :image "redis:7-alpine" :network "app-net")
(define-service svc-app   "app"   :image "myapp:latest"   :network "app-net")

;; Declare the graph — nothing executes yet
(define db-fut    (container-start-async svc-db))
(define cache-fut (container-start-async svc-cache))

(define db-url-fut
  (then db-fut
    (lambda (db) (format "postgres://app:secret@~a/appdb" (container-ip db)))))

(define cache-url-fut
  (then cache-fut
    (lambda (cache) (format "redis://~a:6379" (container-ip cache)))))

(define app-fut
  (container-start-async svc-app
    :after  (list db-url-fut cache-url-fut)
    :inject (lambda (db-url cache-url)
              (list (cons "DATABASE_URL" db-url)
                    (cons "CACHE_URL"    cache-url)))))

;; Execute the graph
(define results
  (run-all (list db-fut cache-fut db-url-fut cache-url-fut app-fut)))

;; Use resolved handles
(define app (result-ref results "app"))
(container-wait app)
```

---

## Choosing an Executor

| Situation | Recommended executor |
|-----------|----------------------|
| Multiple independent services (db ∥ cache ∥ redis) | `run-all` |
| You want upfront cycle detection | `run-all` |
| A future parallel executor matters to you | `run-all` (graph is already complete) |
| Linear pipeline: each step's future is determined by the previous value | `resolve` |
| Short chains without a name for every intermediate future | `resolve` |
| Mix: static topology + one conditional branch | `run-all` — see below |

Both executors share the same `Value::Future` type and the same
`container-start-async` / `then` vocabulary — you can switch between them
or mix them freely.

---

## Mixing Static and Conditional Execution

Most graphs are fully known upfront and belong entirely in `run-all`.  But
sometimes one step needs to choose *which* container to start based on a value
only available at runtime — a schema version, a feature flag, a health check
result.

You do not need to split this into two executor calls.  A single `run-all`
handles it: include the decision step in the list like any other future.
When its lambda runs and returns a `Future` instead of a plain value, the
executor resolves that future inline and stores the result.  Every downstream
future in the static graph sees the result normally.

```
Static graph (topo-sorted upfront)
     │
     ├── db-fut ──→ db-url-fut ────────────────────────────────┐
     │                                                         │
     └── check-fut (then db-url-fut ...)                       │
             lambda inspects db-url at runtime:                │
               if migrations needed → returns migrate-fut      │
               else                 → returns noop-fut         │
                    │                                          │
                    │  resolved inline, result stored          │
                    ▼                                          ▼
            (migrate or noop runs)                      app-fut depends
                    │                                   on both ↑
                    └───────────────────────────────────────────┘
```

The decision step — `check-fut` — is declared in the `run-all` list like any
other future.  Its lambda is a normal `if` expression.  No special syntax
marks it as conditional.

```lisp
;; Service declarations
(define-service svc-db      "db"      :image "postgres:16"   :network "app-net" ...)
(define-service svc-migrate "migrate" :image "myapp-migrate" :network "app-net")
(define-service svc-noop    "noop"    :image "alpine:latest" :network "app-net"
  :command (list "/bin/true"))
(define-service svc-app     "app"     :image "myapp:latest"  :network "app-net")

;; Static futures — full graph known upfront
(define db-fut (container-start-async svc-db))

(define db-url-fut
  (then db-fut
    (lambda (db) (format "postgres://app:secret@~a/appdb" (container-ip db)))))

;; Gateway: Transform whose lambda returns a Future (conditional branch).
;; run-all resolves this inline using the dynamic executor.
(define migration-gate-fut
  (then db-url-fut
    (lambda (db-url)
      (if (need-migrations? db-url)
        (container-start-async svc-migrate
          :after  (list db-url-fut)
          :inject (lambda (url) (list (cons "DATABASE_URL" url))))
        (container-start-async svc-noop)))))

;; App depends on both the gateway result and db-url — static declaration.
(define app-fut
  (container-start-async svc-app
    :after  (list migration-gate-fut db-url-fut)
    :inject (lambda (_gate db-url)
              (list (cons "DATABASE_URL" db-url)))))

;; Single run-all call — handles static, conditional, and downstream together.
(define results
  (run-all (list db-fut db-url-fut migration-gate-fut app-fut)))

(define app (result-ref results "app"))
(container-wait app)
```

**Trade-offs of the hybrid approach:**

- Cycle detection still covers the static portion of the graph (everything
  declared in the `run-all` list).  The dynamic tail is not cycle-checked
  upfront, but dynamic tails created by `then` lambdas are structurally
  acyclic — a lambda cannot close over a future that does not yet exist.
- The parallel executor (future work) will dispatch static tiers as batches.
  A gateway future's dynamic tail runs after the gateway, so the tail is
  sequential relative to the gateway but not relative to other independent
  static futures.
- Prefer the hybrid form (single `run-all`) over two separate executor calls
  when the conditional branch is a leaf or short tail.  Use two calls
  (`run-all` then `resolve`) when the conditional portion is complex enough
  to benefit from being reasoned about independently.

---

## Design Principles

**Futures are values.**  A future is just a `Value::Future` in the Lisp heap.
It can be passed to functions, stored in lists, and composed with `then`.
There is no special syntax — futures participate in the normal Lisp value
model.

**The graph is complete before execution.**  `run-all` receives the entire
set of futures and can inspect all edges before running anything.  This is
what enables topological sorting, cycle detection, and — eventually —
parallel dispatch.

**Data flow is typed.**  `then` transforms produce typed values (strings,
numbers, lists) rather than requiring callers to destructure raw handles.
`:inject` receives those typed values and wires them into containers.  The
`ContainerHandle` type is an implementation detail that leaks only where
needed (e.g., `container-ip`, `container-stop`).

**Executor policy is separate from graph structure.**  The `.reml` file
declares what depends on what.  The executor decides when and how to run it.
Swapping executors is a runtime concern, not a language concern.

---

## Roadmap

The serial executor is complete.  The next step toward a parallel executor:

1. **Parallel `run-all`** — for each "ready" tier (futures with all deps
   resolved), spawn one `std::thread` per future and join them before
   proceeding to the next tier.  Container futures are naturally parallel
   since `do_container_start` is self-contained.  Transform futures are
   pure functions and trivially parallelisable.

2. **Streaming results** — rather than returning a complete alist at the end,
   expose a channel that futures push results onto as they complete.  Enables
   reactive patterns (e.g., start logging from a container as soon as it
   starts, without waiting for the whole graph).

3. **Cancellation** — if any future in a `run-all` fails, send SIGTERM to all
   running containers from that execution set.  The interpreter's `Drop` impl
   already handles this for the global registry; a per-`run-all` registry
   would scope it correctly.
