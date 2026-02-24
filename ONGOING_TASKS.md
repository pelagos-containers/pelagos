# Ongoing Tasks

## Completed: Lisp Interpreter for Remora (Feb 24, 2026)

**Branch:** `lisp-interpreter`

### Context

The compose DSL uses S-expressions as data, not code. As configs grow, users hit the
limits of a fixed schema: no loops, no abstraction, no variables. This task adds a real
Lisp interpreter that uses the existing S-expression parser as its reader and exposes
remora's compose model as first-class values. Old `.rem` files continue to work unchanged;
new `.reml` files are Lisp programs.

**Decisions made:**
- **Execution model**: Hybrid — `service`/`network`/`volume` return typed values;
  `compose` collects them into a spec; `compose-up` runs the spec; `on-ready` registers
  hooks that fire after a service becomes healthy.
- **File detection**: Extension-based. `.rem` = old format unchanged. `.reml` = Lisp.
  `compose up -f compose.reml` auto-dispatches. Default discovery: `compose.reml` first,
  then `compose.rem`.
- **Lisp scope**: Full Scheme subset — TCO, quasiquote/unquote-splicing, named let, do
  loops, R5RS-ish core (~55 builtins).

### Target `.reml` Syntax

```lisp
; Define a parameterized service template
(define (web-service name port)
  (service name
    (image "myapp:latest")
    (network "backend")
    (port port port)
    (depends-on (db :ready (port 5432)))))

; Scale out with map
(define services
  (map (lambda (pair)
         (web-service (car pair) (cadr pair)))
       '(("web" 8080) ("worker" 9090))))

; on-ready hook fires after db health check passes
(on-ready "db" (lambda ()
  (log "db is ready — starting app tier")))

; compose collects specs; compose-up runs them
(compose-up
  (compose
    (network "backend" (subnet "10.89.0.0/24"))
    (service "db"
      (image "postgres:16")
      (network "backend")
      (env "POSTGRES_PASSWORD" "secret"))
    services))   ; spliced list of ServiceSpec values
```

---

### Architecture

#### `src/lisp/value.rs` — Value type

```rust
pub type NativeFn = Rc<dyn Fn(&[Value]) -> Result<Value, LispError>>;

pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Symbol(String),
    Pair(Rc<(Value, Value)>),   // proper cons cells; lists are Pair-terminated-by-Nil
    Lambda { params: Params, body: Vec<SExpr>, env: Env },
    Native(String, NativeFn),
    // Remora domain values
    ServiceSpec(Box<remora::compose::ServiceSpec>),
    NetworkSpec(Box<remora::compose::NetworkSpec>),
    VolumeSpec(String),
    ComposeSpec(Box<ComposeFile>),
}

pub enum Params {
    Fixed(Vec<String>),              // (lambda (a b c) ...)
    Variadic(Vec<String>, String),   // (lambda (a b . rest) ...)
    Rest(String),                    // (lambda args ...)
}

pub struct LispError { pub message: String, pub line: usize, pub col: usize }
```

#### `src/lisp/env.rs` — Environment

```rust
pub type Env = Rc<RefCell<EnvFrame>>;

pub struct EnvFrame {
    bindings: HashMap<String, Value>,
    parent: Option<Env>,
}
// Methods: lookup, define, set (walks up for set!), child (new frame)
```

#### `src/lisp/eval.rs` — Evaluator with TCO

```rust
enum Step { Done(Value), Tail(SExpr, Env) }
fn eval_step(expr: SExpr, env: Env) -> Result<Step, LispError>

pub fn eval(expr: SExpr, env: Env) -> Result<Value, LispError> {
    let mut cur = (expr, env);
    loop {
        match eval_step(cur.0, cur.1)? {
            Step::Done(v)      => return Ok(v),
            Step::Tail(e, env) => cur = (e, env),
        }
    }
}
pub fn eval_apply(func: &Value, args: &[Value]) -> Result<Value, LispError>
```

Special forms: `quote`, `if`, `cond`, `when`, `unless`, `begin`, `define`, `set!`,
`lambda`, `let`, `let*`, `letrec`, named-`let`, `and`, `or`, `quasiquote`
(with `unquote`/`unquote-splicing`), `do` (desugars to named let).

Tail positions: `if` branches, last form of `begin`/`let`/`letrec`, last `cond` clause.

#### `src/lisp/builtins.rs` — ~55 standard functions

| Category | Functions |
|----------|-----------|
| Arithmetic | `+` `-` `*` `/` `quotient` `remainder` `modulo` `abs` `min` `max` `expt` |
| Comparison | `=` `<` `>` `<=` `>=` `equal?` `eqv?` `eq?` |
| Boolean | `not` `boolean?` |
| Pairs/Lists | `cons` `car` `cdr` `cadr` `caddr` `list` `null?` `pair?` `length` `append` `reverse` `list-ref` `iota` `assoc` |
| Higher-order | `map` `filter` `for-each` `apply` `fold-left` `fold-right` |
| Strings | `string?` `string-append` `string-length` `substring` `string->number` `number->string` `string-upcase` `string-downcase` `string=?` `string<?` |
| Symbols | `symbol?` `symbol->string` `string->symbol` |
| Type predicates | `number?` `procedure?` `list?` |
| I/O | `display` `newline` `error` |

#### `src/lisp/remora.rs` — Remora builtins + hook system

```rust
type HookMap = HashMap<String, Vec<Rc<dyn Fn() -> Result<(), LispError>>>>;

pub fn register_remora_builtins(env: &Env, hooks: Rc<RefCell<HookMap>>)
```

| Function | Returns |
|----------|---------|
| `(service name opts...)` | `Value::ServiceSpec` |
| `(network name opts...)` | `Value::NetworkSpec` |
| `(volume name)` | `Value::VolumeSpec` |
| `(compose items...)` | `Value::ComposeSpec` — flattens nested lists of specs |
| `(compose-up spec [project] [foreground?])` | Runs compose, fires hooks |
| `(on-ready "svc" lambda)` | Registers zero-arg hook closure |
| `(env "VAR")` | `Value::Str` or `Value::Nil` |
| `(log msg ...)` | `Value::Nil`; calls `log::info!` |

`on-ready` wraps the lambda value in a Rust closure `move || eval_apply(lambda, &[], env)`
and stores it in `HookMap` under the service name.

#### `src/lisp/mod.rs` — Interpreter

```rust
pub struct Interpreter {
    global_env: Env,
    hooks: Rc<RefCell<HookMap>>,
}
impl Interpreter {
    pub fn new() -> Self
    pub fn eval_file(&mut self, path: &Path) -> Result<Value, LispError>
    pub fn eval_str(&mut self, input: &str) -> Result<Value, LispError>
}
```

#### Hook integration in `src/cli/compose.rs`

Extract from `run_supervisor`:
```rust
pub fn run_compose_with_hooks(
    compose: &ComposeFile,
    compose_dir: &Path,
    project: &str,
    foreground: bool,
    on_ready: &HookMap,
) -> Result<(), Box<dyn std::error::Error>>
```

After `wait_for_dependency` passes and PID/IP recorded, fire hooks:
```rust
if let Some(hooks) = on_ready.get(svc_name) {
    for hook in hooks { hook()?; }
}
```

`.rem` path passes empty `HookMap` — zero behavioural change.

---

### Files To Create/Modify

| File | Change |
|------|--------|
| `src/lisp/mod.rs` | **NEW** |
| `src/lisp/value.rs` | **NEW** |
| `src/lisp/env.rs` | **NEW** |
| `src/lisp/eval.rs` | **NEW** |
| `src/lisp/builtins.rs` | **NEW** |
| `src/lisp/remora.rs` | **NEW** |
| `src/sexpr.rs` | Add `pub fn parse_all()` |
| `src/lib.rs` | Add `pub mod lisp;` |
| `src/cli/compose.rs` | `.reml` dispatch + `run_compose_with_hooks()` |
| `src/main.rs` | Default discovery: `compose.reml` before `compose.rem` |
| `tests/integration_tests.rs` | `test_lisp_compose_basic` |
| `docs/USER_GUIDE.md` | New `.reml` section |
| `docs/INTEGRATION_TESTS.md` | Document new test |

### Implementation Order

1. `src/sexpr.rs` — `parse_all()`
2. `src/lisp/value.rs` — Value, LispError, Params, NativeFn
3. `src/lisp/env.rs` — Env, EnvFrame
4. `src/lisp/eval.rs` — core evaluator, TCO, all special forms, quasiquote
5. `src/lisp/builtins.rs` — arithmetic, lists, strings, predicates
6. `src/lisp/mod.rs` — Interpreter, unit tests
7. **Checkpoint**: `cargo test --lib`
8. `src/lisp/remora.rs` — domain builtins + hook system
9. `src/cli/compose.rs` — extract `run_compose_with_hooks`, add `.reml` dispatch
10. `src/main.rs` — default file discovery
11. Tests + docs

### Verification

1. `cargo build` — clean
2. `cargo test --lib` — all pass
3. Manual: write `test.reml` using `define`/`lambda`/`map`, run `remora compose up -f test.reml`
4. Verify `(on-ready "db" ...)` fires at the right moment in logs
5. Verify existing `compose.rem` still works unchanged
6. Integration test: eval a `.reml` string, assert `ComposeSpec` structure

### Status

**COMPLETE.** All files created, `cargo build` + `cargo clippy -- -D warnings` + `cargo fmt`
+ `cargo test --lib` (205 tests) all pass. Two integration tests pass:
`test_lisp_compose_basic` and `test_lisp_evaluator_tco_and_higher_order`. Docs updated.

---

### Notes / Risks

- `Value` needs `Clone`; `Pair(Rc<...>)`, `Env(Rc<...>)`, `NativeFn(Rc<...>)` all clone cheaply.
- `unquote-splicing` into pair structure: build right-to-left with `cons`.
- `(compose ... services)` where `services` is a Lisp list of `ServiceSpec`: `compose` builtin flattens one level.
- Hooks survive fork (heap Rc closures in child process). Correct.
- `HookMap` pub-re-exported from `lisp::mod` so `cli::compose` doesn't need deep import path.
- All existing `.rem` compose tests unaffected — they use the old path exclusively.
