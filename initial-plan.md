# epsh: Embeddable POSIX Shell

## Context

Build a non-interactive, embeddable POSIX shell in stdlib-only Rust, targeting use as a script executor for coding agents. The dash shell (`~/d/dash-0.5.11+git20200708+dd9ef66`) serves as the reference implementation. We port its semantics faithfully but leverage Rust's type system and memory safety for cleaner, more maintainable code.

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

**1. AST as Rust enums** (vs dash's `union node` + type tag)
```rust
enum Command {
    Simple { assigns: Vec<Assignment>, args: Vec<Word>, redirs: Vec<Redir> },
    Pipeline { commands: Vec<Command>, bang: bool },
    And(Box<Command>, Box<Command>),
    Or(Box<Command>, Box<Command>),
    Semi(Box<Command>, Box<Command>),
    Subshell { body: Box<Command>, redirs: Vec<Redir> },
    If { cond: Box<Command>, then_part: Box<Command>, else_part: Option<Box<Command>> },
    While { cond: Box<Command>, body: Box<Command> },
    Until { cond: Box<Command>, body: Box<Command> },
    For { var: String, words: Option<Vec<Word>>, body: Box<Command> },
    Case { word: Word, arms: Vec<CaseArm> },
    FuncDef { name: String, body: Box<Command> },
    Not(Box<Command>),
    Background { cmd: Box<Command>, redirs: Vec<Redir> },
}
```

**2. Word representation** — instead of dash's embedded control characters (CTLVAR, CTLBACKQ, etc.), use a typed representation:
```rust
enum WordPart {
    Literal(String),
    SingleQuoted(String),
    DoubleQuoted(Vec<WordPart>),
    Param(ParamExpr),         // ${var}, ${var:-default}, ${#var}, etc.
    CmdSubst(Box<Command>),   // $(cmd)
    Backtick(Box<Command>),   // `cmd`
    Arith(Vec<WordPart>),     // $((expr))
    Tilde(String),            // ~user
    Glob(String),             // deferred to expansion phase
}
```

**3. Error handling** — `Result<T, ShellError>` instead of setjmp/longjmp. Use an enum for control flow:
```rust
enum ShellError {
    ExitShell(i32),           // exit N
    Return(i32),              // return N
    Break(usize),             // break N
    Continue(usize),          // continue N
    CommandNotFound(String),
    SyntaxError(String, usize), // message + line number
    IoError(std::io::Error),
    // ...
}
```

**4. Variable storage** — `HashMap<String, Var>` instead of dash's hand-rolled hash table:
```rust
struct Var {
    value: Option<String>,    // None = unset
    flags: VarFlags,          // export, readonly, local
}
```
Local scoping via a `Vec<HashMap<String, Option<Var>>>` scope stack pushed/popped on function calls.

**5. Redirection state** — stack of saved FDs, restored on scope exit:
```rust
struct RedirState {
    saved: Vec<(RawFd, Option<RawFd>)>,  // (fd, saved_copy)
}
```

**6. No libc glob** — custom glob implementation in `glob.rs` using `std::fs::read_dir`. Avoids libc dependency and gives us full control.

**7. Arithmetic evaluator** — recursive-descent parser for C-like integer arithmetic in `arith.rs`, replacing dash's yacc-generated `arith_yacc.c`.

**8. Fork/exec** — use `std::process::Command` where possible, fall back to raw `libc::fork`/`libc::execve` via `std::os::unix` for pipelines and complex FD routing. Actually — since we're stdlib-only and need precise FD control for pipelines/redirections, we'll use `unsafe` blocks with the unix-specific stdlib APIs (`std::os::unix::process::CommandExt` for `pre_exec`, plus `std::os::unix::io`).

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
