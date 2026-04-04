# epsh — Embeddable POSIX Shell

Non-interactive, embeddable POSIX shell in Rust + libc. Script executor for coding agents.

167/167 mksh conformance on dash-passable tests. 10k lines, 376 tests, zero clippy warnings.

## Architecture

```
src/
  lib.rs          Public API, module declarations
  main.rs         CLI binary: epsh [-c cmd] [-e] [script.sh]
  ast.rs          AST types: Command, Word, WordPart, Redir, ParamExpr
  lexer.rs        Single-pass tokenizer with unified word-part builder
  parser.rs       Recursive-descent parser, heredoc body reading
  eval.rs         Shell struct, ShellBuilder, eval dispatch, pipelines, comsub
  builtins.rs     30 builtins: echo, cd, test, read, set, trap, printf, kill, ...
  expand.rs       Word expansion: tilde, param, arith, IFS split, glob, patterns
  arith.rs        $((…)) evaluator with short-circuit (noeval), int var cache
  var.rs          Variable storage with scope stack, integer cache
  glob.rs         Custom fnmatch + pathname expansion (cwd-aware)
  redirect.rs     FD save/restore for redirections
  signal.rs       Signal handler install/reset, atomic pending flags
  test_cmd.rs     POSIX test/[ recursive-descent evaluator
  error.rs        ShellError enum, ExitStatus newtype, Result alias
  encoding.rs     PUA-based byte preservation for non-UTF-8 data
  sys.rs          Thin libc wrappers
```

## Embedding API

```rust
use epsh::eval::Shell;
use epsh::parser::Parser;
use epsh::builtins::{is_builtin, BUILTIN_NAMES};

// Builder pattern — recommended for embedders
let mut shell = Shell::builder()
    .cwd(PathBuf::from("/project"))
    .errexit(true)
    .pipefail(true)
    .noglob(true)
    .noexec(false)
    .stdout_sink(stdout.clone())
    .stderr_sink(stderr.clone())
    .cancel_flag(cancel.clone())
    .timeout(Duration::from_secs(120))
    .env_clear()
    .build();

// Parse once, execute separately (for permission checking)
let program = Parser::new("echo hello").parse().unwrap();
let status = shell.run_program(&program);

// Custom process spawner (for sandboxing, job control, SSH proxy)
shell.set_external_handler(Box::new(|args, env| {
    // args[0] is command name, env is prefix assignments
    // redirections already applied to fds
    todo!("spawn the process your way")
}));

// Interactive shell primitives
let mut ish = Shell::builder()
    .interactive(true)           // enables tcsetpgrp + WUNTRACED
    .external_handler(handler)   // you own fork/exec/wait
    .build();
// ShellError::Stopped { pid, pgid } propagates when a process is stopped

// Builtin detection for permission systems / prompt coloring
assert!(is_builtin("echo"));
assert!(!is_builtin("rm"));
```

## Testing

```sh
cargo test                                              # 376 tests
cargo test --test api_stability                         # API surface regression
cargo test --test embedding                             # embedding API tests
cargo test --test integration                           # shell behavior tests
cargo build && perl check.pl -p ./target/debug/epsh \
  -s check-epsh.t                                       # 167/167 mksh conformance
sh tests/stress/run.sh ./target/debug/epsh dash         # performance vs dash
perl filter-tests.pl check.t > check-epsh.t             # regenerate filtered tests
```

## Performance vs dash

Fork-dominated operations (typical coding agent workload) are at parity:

| Operation | vs dash | Technique |
|-----------|---------|-----------|
| Builtin comsub | **3.6x faster** | Fork-free `$(echo ...)` |
| Heredocs | **0.8x** | Faster than dash |
| Pipelines | **1.1x** | posix_spawn, exec-direct |
| External commands | **1.1x** | At parity |
| Tight arith loops | ~28x | String allocation dominated |

The arith loop gap requires integer variable representation (dash's approach).

## Design Lineage — What We Actually Took

The original plan drew from three reference shells. Here's what we ended up
using, what we adapted, and where we diverged.

### From dash (primary — semantics and behavior)

dash is the conformance target. We ported these mechanisms directly:

- **EV_TESTED errexit model** (`eval.rs`): Only leaf nodes (Simple, Pipeline, Subshell)
  trigger `set -e` exit. Compound nodes propagate status but never exit directly.
  The `tested` flag suppresses errexit in if/while/until conditions and `&&`/`||`
  left operands. Matches `eval.c` lines 246-330.

- **pgetc_eatbnl** (`lexer.rs`): `peek()` and `advance()` transparently consume
  `\<newline>` continuations. `peek_raw()`/`advance_raw()` for single-quote and
  heredoc body contexts. Comments use raw reading.

- **parsesub syntax context** (`lexer.rs`): `read_brace_param` passes `in_dquote`
  to `read_brace_word_parts`. Trim ops (`%`, `#`) force BASESYNTAX (single quotes
  are quoting). Default/assign/alt (`-`, `+`, `=`, `?`) inherit context. Inner `"`
  in dquote `${…}` toggles context (dash's `innerdq`).

- **Heredoc reading at newlines** (`parser.rs`): `read_pending_heredocs` called
  after newline consumption in both `parse_program` and `parse_command_list`.
  Matches dash's `parseheredoc()` in `readtoken()`.

- **in_forked_child optimization** (`eval.rs`): Subshells inside pipeline children
  execute directly without double-forking. Prevents pipe fd leaks. Mirrors dash's
  `EV_EXIT` fast-path in `evalsubshell`.

- **IO_NUMBER detection** (`parser.rs`): Digit-only unquoted words before `<`/`>`
  parsed as fd numbers, not arguments.

- **Arithmetic noeval** (`arith.rs`): Ternary `?:` and `&&`/`||` suppress side
  effects (assignments) on non-taken branches via `noeval` flag. Matches dash's
  `arith_yacc.c`.

- **preglob/RMESCAPE_GLOB** (`expand.rs`): `expand_pattern()` escapes glob
  metacharacters from quoted regions for fnmatch. Used by trim ops and case patterns.

- **Heredoc body rules** (`parser.rs`, `lexer.rs`): Unquoted heredocs use DQSYNTAX
  (single quotes literal, `"` literal, only `\$` `\`` `\\` `\<nl>` are special
  backslash escapes). `\<newline>` continuation with odd/even backslash counting.

### From posh (IFS and expansion)

- **IFS field-splitting state machine** (`expand.rs`): Single-pass `field_split`
  distinguishes IFS whitespace vs non-whitespace. Handles empty fields between
  non-WS delimiters, leading/trailing WS stripping. Adopted as designed.

- **in_param_word flag** (`expand.rs`): Literals inside `${var+word}` etc. are
  marked `split_fields: true` when in unquoted context. This was our adaptation
  of posh's SubType stack concept — we don't have a stack, but the flag achieves
  the same per-context splitting behavior.

- **Variable scope stack** (`var.rs`): `push_scope()`/`pop_scope()` with saved
  values for `local`. Inspired by posh's environment stacking, implemented with
  Rust's RAII-friendly design.

### From mrsh (AST design and API)

- **Span on every AST node** (`ast.rs`): Source position tracking for error
  reporting. Used in ShellError::Syntax and ShellError::Runtime.

- **word_break metadata** (`expand.rs`): `ExpandedWord` carries `word_break: bool`
  to mark `"$@"` field boundaries, keeping arguments separate through field splitting.

- **Library-first architecture**: `Shell` struct with `run_script`, `set_var`,
  `get_var` API. The binary is a thin CLI wrapper.

### Where We Diverged

- **Single-pass WordPart building** — we started with mrsh's two-phase approach
  (lexer collects raw text, parser re-parses into WordParts) but hit a wall of
  quoting-context bugs at ~74% conformance. We restructured to build WordParts
  directly during lexing (matching dash's single-pass model) while keeping
  mrsh's typed enum representation instead of dash's control-character-in-string
  approach. This was the highest-impact architectural change.

- **ExitStatus newtype** — neither dash, posh, nor mrsh distinguish exit codes
  at the type level. Our `ExitStatus(i32)` with named constants (`SUCCESS`,
  `FAILURE`, `MISUSE`, `NOT_FOUND`, `NOT_EXECUTABLE`) and `from_wait()` prevents
  integer confusion throughout the codebase.

- **Expansion returns Result** — dash uses setjmp/longjmp, posh uses longjmp,
  mrsh uses return codes. We use `Result<T, ShellError>` with the `?` operator,
  which gives us dash-equivalent error propagation with Rust's type safety.
  BadSubst, `${var?msg}`, and arithmetic errors all propagate cleanly.

- **`$(<file)` optimization** — reads the file directly without forking, since
  we can detect the pattern (Simple command with no args/assigns, one input
  redirect) before entering the fork path.

## Interactive Shell Support

epsh doesn't implement interactive features directly, but provides the minimal
primitives for an interactive shell to be built on top:

- **`ExternalHandler`**: callback replacing fork+exec for external commands.
  The embedder owns process creation for job control and terminal management.
- **`interactive` mode**: `tcsetpgrp` gives pipeline foreground, `WUNTRACED`
  detects stopped processes, `ShellError::Stopped { pid, pgid }` carries
  the pipeline PGID for job resume.
- **`BUILTIN_NAMES`**: for prompt coloring and completion.

Everything else (prompt, history, line editing, `fg`/`bg`/`jobs`) is the
interactive shell's responsibility.

## Known Limitations

- Signal traps fire between commands but not during blocking waits
- `~user` expansion requires getpwnam (not available stdlib-only)
- Arith loops ~28x slower than dash (string allocation; fixable with integer var repr)

## What's Not Implemented (by design)

- Job control builtins (fg, bg, jobs) — use `external_handler` + `Stopped`
- Interactive features (prompt, history, line editing) — use rustyline/reedline
- Aliases
- Here-strings (`<<<`)
- Extended globs (`@(...)`, `+(...)`, etc.)
- Arrays, typeset, select (ksh extensions)
