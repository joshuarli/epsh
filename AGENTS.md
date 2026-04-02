# epsh — Embeddable POSIX Shell

Non-interactive, embeddable POSIX shell written in stdlib-only Rust. Designed as a script executor for coding agents.

## Architecture

```
src/
  lib.rs        Public API (re-exports modules)
  main.rs       CLI binary: epsh [-c cmd] [-e] [script.sh]
  ast.rs        AST node types (Command, Word, WordPart, Redir, etc.)
  lexer.rs      Tokenizer: source text → Token stream
  parser.rs     Recursive-descent parser: tokens → AST
  eval.rs       Tree-walking evaluator + builtins (Shell struct)
  expand.rs     Word expansion: tilde, param, cmdsub, arith, IFS split, glob
  arith.rs      Arithmetic expression evaluator for $((…))
  var.rs        Variable storage with scope stack
  glob.rs       Custom fnmatch + pathname expansion (no libc glob)
  sys.rs        Minimal POSIX syscall wrappers (fork, pipe, dup2, etc.)
```

## Design Lineage

Three reference shells inform the design:

- **dash** (primary): POSIX semantics and behavior target. The evaluator's errexit logic, heredoc handling, and `$@`/`$*` expansion follow dash's patterns directly. dash's `eval.c` EV_TESTED flag model is mirrored in our `tested` field.

- **posh**: IFS field-splitting state machine (single-pass with IFS_WS/IFS_NWS states). SubType stack concept for nested `${var:-${other}}` expansion. Environment stacking model mapped to Rust's RAII scopes.

- **mrsh**: AST design — every node carries a `Span` for error reporting. Word-list nesting for quoting (instead of embedded control characters). Split-fields metadata (`word_break` flag on `ExpandedWord`). Library-first architecture.

## Key Data Flow

```
Shell::run_script(source)
  → Parser::new(source)
  → parser.parse() → Program { commands: Vec<Command> }
  → for each command: shell.eval_command(cmd)
      → expand words via shell.expand_fields/expand_string
          → expand_word_parts (tilde, param, cmdsub, arith)
          → field_split (IFS splitting with word_break boundaries)
          → glob (pathname expansion)
      → setup_redirections (dup2 with saved fd stack)
      → dispatch: builtin / function / external (fork+exec)
      → restore_redirections
```

## Important Implementation Details

**stdout writes**: All builtins use `write_stdout()` / raw `sys::write(1, ...)` instead of `println!()`. This is required because `_exit()` doesn't flush Rust's `BufWriter`, which breaks command substitution (where fd 1 is a pipe).

**Command substitution**: Uses `fork()` + pipe. The parent reads output; the child evaluates the command with stdout redirected to the pipe. The `expand_fields`/`expand_string` methods on Shell use a raw pointer trick (`self as *mut Shell`) to create the cmd_subst closure that bypasses borrow checker constraints — safe because fork gives the child its own address space.

**errexit (`set -e`)**: Mirrors dash's EV_TESTED model. Only "leaf" nodes (Simple, Pipeline, Subshell) trigger errexit. Compound nodes (if, while, for, case, &&, ||) propagate status but don't exit. The `tested` flag suppresses errexit in if/while/until conditions and `&&`/`||` left operands.

**`$@` vs `$*`**: `"$@"` produces separate `ExpandedWord` fragments with `word_break: true`. `"$*"` joins with IFS[0]. Unquoted `$@` and `$*` both produce separate word-break fields. The `field_split` function respects `word_break` boundaries to keep `"$@"` arguments separate.

**Variable scoping**: `Variables` has a scope stack (`Vec<Scope>`). `push_scope()` on function entry, `pop_scope()` on exit restores saved values. `make_local()` saves the current value before local modification.

## Testing

- 136 internal unit/integration tests (`cargo test`)
- mksh conformance: 113/167 filtered tests pass (dash passes 167/167)
- Test filter: `filter-tests.pl` extracts POSIX-relevant tests from mksh's `check.t`
- Run: `perl check.pl -p ./target/debug/epsh -s check-epsh.t`

## What's Not Implemented

- Job control (fg, bg, jobs, process groups)
- Interactive features (prompt, history, line editing)
- Aliases
- Signal trap execution (trap stores handlers but doesn't install signal handlers)
- `$(<file)` input redirection shorthand
- `<<<` here-strings
- Extended globs (`@(...)`, `+(...)`, etc.)
