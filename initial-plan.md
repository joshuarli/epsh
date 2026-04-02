# epsh: Embeddable POSIX Shell

## Context

Build a non-interactive, embeddable POSIX shell in stdlib-only Rust, targeting use as a script executor for coding agents. Two reference implementations studied:
- **dash** (`~/d/dash-0.5.11+git20200708+dd9ef66`) — minimal, fast, well-tested POSIX shell (~12k LOC). Classic C design with union-tagged AST, setjmp/longjmp errors, embedded control chars in words, hand-rolled hash tables.
- **posh** (`~/d/posh-0.14.3`) — portable POSIX shell (~17k LOC, pdksh lineage). Notable for arena allocation, environment stacking with per-scope jbufs, SubType stack for nested expansion, explicit field-splitting state machine (IFS_WORD/IFS_WS/IFS_NWS), and migration to tree-based symbol lookup.

- **mrsh** (`~/d/mrsh`) — modern, clean POSIX shell (~12k LOC). Library-first design (libmrsh + binary). Explicit type-safe AST (no union tags), position tracking on every node, word-list nesting for quoting, split-fields metadata flag, task-based control flow (no setjmp), explicit context passing (no globals), modular builtins (one file each), optional job control.

We take the best ideas from all three and leverage Rust's type system for cleaner, safer code.

## What We Keep from Dash

- Full POSIX shell grammar (parser + lexer)
- AST node types for all compound commands, pipelines, redirections
- Word expansion pipeline: tilde, parameter, command substitution, arithmetic, field splitting, pathname expansion
- Variable management with export/readonly/local scoping
- Redirection and file descriptor management
- POSIX-required builtins
- Trap/signal handling (non-interactive subset)
- Here-documents

## What We Drop

- **Job control** (fg, bg, jobs, process groups, TIOCSPGRP) — not needed for script execution
- **Interactive features** (prompt, line editing, history, mail checking)
- **Aliases** — POSIX allows omitting in non-interactive mode
- **Login shell profiles** (~/.profile, /etc/profile)
- **mknodes.c code generator** — replaced by Rust enums

## Architecture

### Public API (`lib.rs`)

```rust
pub struct Shell { /* variables, functions, traps, options, exit status */ }

impl Shell {
    pub fn new() -> Shell;
    pub fn run_script(&mut self, source: &str) -> i32;  // returns exit status
    pub fn set_var(&mut self, name: &str, value: &str);
    pub fn get_var(&self, name: &str) -> Option<&str>;
    pub fn set_args(&mut self, args: &[&str]);           // $0, $1, ...
}
```

### Module Layout

```
src/
  lib.rs          — Public API, Shell struct
  error.rs        — ShellError enum, Result type alias
  ast.rs          — AST node types as Rust enums
  lexer.rs        — Tokenizer (character stream → tokens)
  parser.rs       — Recursive-descent parser (tokens → AST)
  eval.rs         — Tree-walking evaluator (AST → execution)
  expand.rs       — Word expansion pipeline
  exec.rs         — Command resolution + fork/exec
  redir.rs        — Redirection setup/teardown with FD save/restore
  var.rs          — Variable storage, scoping, environment export
  arith.rs        — Arithmetic expression evaluator ($((…)))
  trap.rs         — Signal trap management
  glob.rs         — Pathname expansion (custom, no libc glob)
  builtin/
    mod.rs        — Builtin dispatch table
    echo.rs       — echo
    printf.rs     — printf
    test.rs       — test / [
    cd.rs         — cd, pwd
    flow.rs       — break, continue, return, exit
    eval_cmd.rs   — eval, dot/source
    export.rs     — export, readonly, unset
    read.rs       — read
    set.rs        — set, shift
    trap_cmd.rs   — trap
    exec_cmd.rs   — exec
    wait.rs       — wait
    misc.rs       — true, false, colon, type, command, umask, getopts
```

### Key Design Decisions

**1. AST with source positions** (inspired by mrsh) — every node carries a `Span` for error reporting:
```rust
struct Span { offset: usize, line: u32, col: u32 }

enum Command {
    Simple { assigns: Vec<Assignment>, args: Vec<Word>, redirs: Vec<Redir>, span: Span },
    Pipeline { commands: Vec<Command>, bang: bool, span: Span },
    And(Box<Command>, Box<Command>),
    Or(Box<Command>, Box<Command>),
    Semi(Box<Command>, Box<Command>),
    Subshell { body: Box<Command>, redirs: Vec<Redir>, span: Span },
    If { cond: Box<Command>, then_part: Box<Command>, else_part: Option<Box<Command>>, span: Span },
    While { cond: Box<Command>, body: Box<Command>, span: Span },
    Until { cond: Box<Command>, body: Box<Command>, span: Span },
    For { var: String, words: Option<Vec<Word>>, body: Box<Command>, span: Span },
    Case { word: Word, arms: Vec<CaseArm>, span: Span },
    FuncDef { name: String, body: Box<Command>, span: Span },
    Not(Box<Command>),
    Background { cmd: Box<Command>, redirs: Vec<Redir> },
}
```

**2. Word representation** — word-list nesting (inspired by mrsh) instead of dash's embedded control characters:
```rust
// A Word is a list of parts that concatenate to form a single shell word
struct Word { parts: Vec<WordPart>, span: Span }

enum WordPart {
    Literal(String),
    SingleQuoted(String),
    DoubleQuoted(Vec<WordPart>),  // parts inside "..."
    Param(ParamExpr),             // ${var}, ${var:-default}, ${#var}, etc.
    CmdSubst(Box<Command>),       // $(cmd)
    Backtick(Box<Command>),       // `cmd`
    Arith(Vec<WordPart>),         // $((expr))
    Tilde(String),                // ~user
}
// Glob patterns are detected during expansion, not at parse time
```

**3. Error handling** — `Result<T, ShellError>` instead of setjmp/longjmp (matching mrsh's approach). Errors carry source position for good diagnostics:
```rust
enum ShellError {
    Exit(i32),                          // exit N
    Return(i32),                        // return N
    Break(usize),                       // break N
    Continue(usize),                    // continue N
    CommandNotFound(String),
    Syntax { msg: String, span: Span }, // with position
    Io(std::io::Error),
}
```

**Split-fields tracking** (adopted from mrsh) — expanded words carry metadata about whether field splitting applies:
```rust
struct ExpandedWord {
    value: String,
    split_fields: bool,  // true for unquoted $var, $(cmd) — triggers IFS splitting
}
```
This elegantly handles the difference between `"$var"` (no split) and `$var` (split).

**4. Variable storage** — `HashMap<String, Var>` instead of dash's hand-rolled hash table:
```rust
struct Var {
    value: Option<String>,    // None = unset
    flags: VarFlags,          // export, readonly, local
}
```
Local scoping via a `Vec<HashMap<String, Option<Var>>>` scope stack pushed/popped on function calls.

**5. Environment stacking** (inspired by posh's `struct env`) — each function call, dot-script, or subshell pushes a new `Scope` that owns its local variables and saved FD state. On exit (normal or error), the scope's `Drop` impl restores FDs and pops variables. This replaces both dash's `struct localvar` chain and posh's `quitenv()` cleanup with Rust's RAII.
```rust
struct Scope {
    locals: HashMap<String, Option<Var>>,  // saved previous values
    saved_fds: Vec<(RawFd, Option<RawFd>)>,
}
```

**6. Field splitting state machine** (adopted from posh) — track IFS state as an enum rather than dash's multi-pass approach:
```rust
enum IfsState { Init, Word, IfsWhitespace, IfsNonWhitespace }
```
This handles edge cases correctly in a single pass: empty fields between non-WS IFS chars, leading/trailing WS IFS stripping, and no splitting inside quotes.

**7. No libc glob** — custom glob implementation in `glob.rs` using `std::fs::read_dir`. Avoids libc dependency and gives us full control.

**8. Arithmetic evaluator** — recursive-descent parser for C-like integer arithmetic in `arith.rs`, replacing dash's yacc-generated `arith_yacc.c`.

**9. Fork/exec** — use `std::process::Command` with `CommandExt::pre_exec` for simple commands. For pipelines needing precise FD routing, use `unsafe` with `std::os::unix` APIs. No direct libc dependency.

**10. Nested expansion via SubType stack** (adopted from posh) — when expanding `${x:-${y}}`, push a `SubType` frame tracking the outer expansion's state. This handles arbitrary nesting depth cleanly:
```rust
struct SubType {
    kind: ParamOp,        // Minus, Plus, Question, Assign, Trim*, Length
    base: Option<String>, // accumulated value before nested expansion
    quoted: bool,
}
```

### Execution Flow

```
Shell::run_script(source)
  → Lexer::new(source)
  → Parser::parse(&mut lexer) → Command (AST)
  → Eval::eval_command(&mut shell, &command) → i32 (exit status)
      → expand words (expand.rs)
      → setup redirections (redir.rs)
      → dispatch:
          builtin? → call builtin fn
          function? → push scope, eval body, pop scope
          external? → fork + exec
      → teardown redirections
```

## Implementation Order

Building bottom-up, testing each layer before moving on:

1. **Scaffold** — `Cargo.toml` (lib crate), module stubs, error types
2. **AST** — Node type definitions
3. **Lexer** — Tokenizer with all POSIX tokens, quoting, here-doc collection
4. **Parser** — Recursive-descent parser producing AST
5. **Variables** — Var storage, scoping, special variables ($?, $$, $#, etc.)
6. **Word Expansion** — Tilde, parameter expansion, field splitting, quote removal (no command subst yet)
7. **Glob** — Pathname expansion
8. **Redirections** — FD management, here-documents
9. **Eval Core** — Simple commands, compound commands, pipelines (fork/exec)
10. **Command Substitution** — $(…) and backticks wired into expansion
11. **Arithmetic** — $((…)) evaluator
12. **Builtins** — All POSIX-required builtins
13. **Traps** — Signal handling, trap builtin
14. **Integration** — Wire everything together, end-to-end script execution

## Verification

- Unit tests per module (parser round-trips, expansion correctness, arithmetic edge cases)
- Integration tests running actual shell scripts and comparing output/exit status against dash
- Key test cases: quoting edge cases, nested expansions, pipelines, here-docs, subshells, function scoping, trap handling, `set -e` / `set -o pipefail` semantics
