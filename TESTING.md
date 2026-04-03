# Testing Strategy

## Current Tests

314 unit/integration tests across 9 modules + integration/embedding/API suites:

- **lexer** (19): tokenization, quoting, operators, reserved words, comments, escapes
- **parser** (34): full grammar coverage — simple commands, pipelines, compound commands, case, for, functions, word parts, parameter expansion syntax
- **arith** (14): arithmetic evaluator — precedence, operators, variables, assignment, hex/octal, division by zero
- **expand** (20): parameter expansion (all 10 POSIX ops), field splitting, tilde, globbing, quoting
- **glob** (9): fnmatch pattern matching — wildcards, character classes, ranges, negation, escapes
- **var** (8): set/get, unset, readonly, scope push/pop, positional params, special params
- **eval** (32): end-to-end script execution — commands, pipelines, command substitution, here-docs, if/while/for/case, functions, arithmetic, subshells, set -e, test builtin, local vars, trap
- **integration** (122): oils-spec and oils-posix conformance, builtins, redirections, control flow, xtrace, assignments, encoding
- **embedding** (31): builder, sinks, cancellation, timeout, external handler, cwd isolation
- **api_stability** (16): public API surface checks

## POSIX Conformance Testing

### mksh test suite

mksh's `check.t` (~4500 tests) is our primary conformance target. Located at `check.t` with runner `check.pl`.

#### Applicable categories for epsh

These test categories exercise features epsh implements:

- **basic**: simple command execution, exit status
- **quoting**: single/double quotes, backslash, escaping edge cases
- **expansion**: parameter expansion (${var:-...} etc.), field splitting, tilde
- **arith**: arithmetic $((...)) — operators, precedence, assignment
- **pattern**: glob/fnmatch pattern matching, case patterns
- **redirect**: file redirections, here-documents, fd duplication
- **pipeline**: pipe execution, exit status propagation
- **compound**: if/while/until/for/case syntax and semantics
- **function**: function definition, local variables, return
- **builtin**: test/[, echo, printf, read, set, shift, trap, etc.
- **errexit**: set -e behavior in various contexts

#### Categories to skip

These require features epsh intentionally omits:

- **interactive**: line editing, history, prompts, mail checking
- **job-control**: fg, bg, jobs, process groups, TIOCSPGRP
- **alias**: alias expansion (POSIX allows omitting in non-interactive mode)
- **emacs/vi**: editor modes
- **coprocess**: |& syntax (ksh extension)
- **select**: select loop (ksh extension)
- **arrays**: array variables (ksh extension)
- **typeset**: typeset/declare (ksh extension)
- **heredoc-tmpfile**: tests that depend on heredoc implementation via temp files

#### Running mksh tests

```sh
cargo build
perl check.pl -s ./target/debug/epsh -c check.t
```

#### Per-test filtering

Individual tests in `check.t` may need skipping even within applicable categories if they use ksh extensions. Look for tests that use:
- `typeset`, `nameref`, `integer` keywords
- `[[ ]]` extended test syntax
- `${var/pat/rep}` substitution (ksh extension, not POSIX)
- `$RANDOM`, `$SECONDS` (ksh special variables)
- `print -r` (ksh print builtin)
- `function name {` syntax (ksh function definition)

### Comparison testing against dash

```sh
for f in tests/*.sh; do
    diff <(dash "$f" 2>&1; echo "exit:$?") \
         <(./target/debug/epsh "$f" 2>&1; echo "exit:$?")
done
```

## Remaining Known Gaps

- Signal handling: `trap` stores handlers but doesn't install signal handlers
- `~user` expansion: requires getpwnam, not available stdlib-only
