# epsh — Embeddable POSIX Shell

Non-interactive, embeddable POSIX shell in Rust + libc. Script executor for coding agents.

156/167 (93%) mksh conformance on dash-passable tests. 9.4k lines, 136 unit tests.

## Architecture

```
src/
  lib.rs          Public API
  main.rs         CLI binary: epsh [-c cmd] [-e] [script.sh]
  ast.rs          (205) AST types: Command, Word, WordPart, Redir, ParamExpr
  lexer.rs        (1912) Single-pass tokenizer with syntax-context tracking
  parser.rs       (1924) Recursive-descent parser, heredoc body reading
  eval.rs         (1076) Shell struct, eval dispatch, pipelines, subshells, comsub
  builtins.rs     (1019) 25+ builtins: echo, cd, test, read, set, trap, printf, ...
  expand.rs       (1084) Word expansion: tilde, param, arith, IFS split, glob, patterns
  arith.rs        (809)  $((…)) evaluator with short-circuit (noeval)
  var.rs          (315)  Variable storage with scope stack
  glob.rs         (289)  Custom fnmatch + pathname expansion
  redirect.rs     (151)  FD save/restore for redirections
  test_cmd.rs     (398)  POSIX test/[ recursive-descent evaluator
  error.rs        (120)  ShellError enum, ExitStatus newtype, Result alias
  sys.rs          (33)   Thin libc wrappers
```

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

## What's Left on the Table

Things we know about but haven't implemented:

- **Signal trap execution**: `trap` stores handlers and EXIT traps fire, but we
  don't install actual signal handlers (SIGINT, SIGTERM, etc. use default behavior).
  A coding agent executor might want Ctrl-C handling.

- **set -x (xtrace)**: The flag is stored but no trace output is produced.
  Useful for debugging agent-generated scripts.

- **Non-UTF-8 input**: Rust strings are UTF-8. Dash handles arbitrary bytes.
  Unlikely to matter for coding agents but technically non-conformant.

- **Heredoc in nested function definitions**: The heredoc body lifecycle doesn't
  survive through function AST storage and re-execution. Niche edge case.

## Testing

```sh
cargo test                                              # 136 unit tests
cargo build && perl check.pl -p ./target/debug/epsh \
  -s check-epsh.t                                       # 156/167 mksh conformance
perl filter-tests.pl check.t > check-epsh.t             # regenerate filtered tests
```

## What's Not Implemented (by design)

- Job control (fg, bg, jobs, process groups)
- Interactive features (prompt, history, line editing)
- Aliases
- Here-strings (`<<<`)
- Extended globs (`@(...)`, `+(...)`, etc.)
- Arrays, typeset, select (ksh extensions)
