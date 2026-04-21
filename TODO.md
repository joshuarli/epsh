# TODO

Detailed backlog for shell correctness work that is important, but deferred.

This file is intentionally biased toward semantic and OS-observable correctness,
not feature growth.

## Priority Order

1. Expansion and quoting exactness
2. `errexit` / special builtin semantics
3. Redirection / fd / process hygiene
4. Signal, trap, and wait behavior
5. `PWD` and path semantics
6. PATH / env merge semantics
7. Command substitution edge cases
8. Glob / pattern exactness
9. NUL-byte policy and API consistency
10. Targeted regression tests and capability matrices

## 1. Expansion And Quoting Exactness

Goal: prove and preserve the exact expansion pipeline and quoting semantics.

### Expansion pipeline audit

- Audit every expansion path against POSIX and current dash-like intent:
  `tilde -> param -> command substitution -> arithmetic -> field splitting -> glob -> quote removal`.
- Verify the code does not accidentally perform quote removal too early.
- Verify quoted regions suppress field splitting and globbing exactly where intended.
- Verify `${...}` forms preserve the correct quoting context in nested cases.
- Verify trim operators (`%`, `%%`, `#`, `##`) use pattern semantics, not plain substring semantics.
- Verify `${var+word}`, `${var-word}`, `${var=word}`, `${var?word}` respect quoted/unquoted context for `word`.
- Verify `$@` and `"$@"` preserve field boundaries exactly.
- Verify `$*` and `"$*"` differ correctly with current `IFS`.
- Verify empty expansions in quoted and unquoted contexts behave correctly.

### Quoting edge cases

- Add targeted tests for interactions among:
  - single quotes
  - double quotes
  - backslash escaping
  - command substitution
  - parameter expansion
  - arithmetic expansion
- Test nested quoting inside `"${...}"`.
- Test escaped glob metacharacters surviving until the right phase.
- Test escaped newline continuation rules in all contexts that support them.
- Test heredoc quoting modes carefully:
  - quoted delimiter
  - unquoted delimiter
  - tab-stripping with `<<-`
  - backslash-newline continuation in heredoc bodies

### Regression matrix

- Build a table of representative cases for:
  - unquoted
  - double-quoted
  - single-quoted
  - assignment context
  - command substitution context
  - pattern context
- Keep these as integration tests instead of only unit tests.

## 2. `errexit` / Special Builtin Semantics

Goal: make failure behavior boringly predictable and script-compatible.

### `set -e` / `errexit`

- Expand integration coverage for `set -e` in:
  - simple commands
  - pipelines
  - `if` conditions
  - `while` / `until` conditions
  - `&&` left-hand side
  - `||` left-hand side
  - subshells
  - functions
  - command substitutions
  - grouped commands
- Verify only leaf execution points trigger exit, not compound syntax nodes.
- Verify tested contexts suppress `errexit` exactly where intended.
- Verify `! cmd` inverts status without producing accidental `errexit`.
- Verify pipeline + `pipefail` interactions under `set -e`.

### Special builtins

- Audit which builtins are treated as POSIX special builtins versus normal builtins.
- Verify assignment prefixes persist for special builtins and do not persist for normal builtins/external commands.
- Verify redirection failure on a special builtin is a shell error with the correct status.
- Verify parse/runtime errors around special builtins affect shell state correctly.
- Add tests for:
  - `export`
  - `readonly`
  - `unset`
  - `eval`
  - `.`
  - `exec`
  - `set`
  - `trap`

### Exit statuses

- Audit every shell-generated error path to ensure statuses are intentional and consistent.
- Verify usage errors vs runtime errors vs command-not-found vs not-executable are distinct where expected.
- Verify builtin misuse returns the intended status.

## 3. Redirection / FD / Process Hygiene

Goal: ensure file descriptor behavior is exact, leak-free, and race-resistant.

### Redirection ordering

- Add tests for left-to-right redirection semantics:
  - `cmd >out 2>&1`
  - `cmd 2>&1 >out`
  - `cmd <in >out`
  - multiple redirections to same fd
- Verify duplication and closure ordering:
  - `n>&m`
  - `n<&m`
  - `n>&-`
  - `n<&-`
- Verify redirections apply correctly to:
  - builtins
  - functions
  - external commands
  - `exec`

### Heredoc plumbing

- Audit heredoc stdin ownership and lifetime.
- Add tests for:
  - multiple heredocs on one command
  - heredoc with builtins
  - heredoc in pipelines
  - heredoc with command substitution in body
- Verify no accidental fd reuse or premature close.

### FD leaks

- Add regression tests that children do not inherit unrelated descriptors.
- Add explicit `CLOEXEC` coverage where feasible.
- Audit all pipe, dup, open, and tempfile-like paths for leak resistance.
- Verify saved redirection fds are always restored or intentionally consumed.

### Process spawning

- Verify direct `exec` and spawned child process setup stay equivalent for:
  - env
  - cwd
  - redirections
  - signal dispositions
- Verify failure paths restore shell fds/state correctly.

## 4. Signal, Trap, And Wait Behavior

Goal: make process control and traps behave like a real shell, especially during waits.

### Trap timing

- Revisit the current known limitation:
  traps fire between commands but not during blocking waits.
- Define the intended model explicitly.
- Add tests for traps arriving during:
  - foreground external command wait
  - pipeline wait
  - command substitution wait
  - builtin-heavy loops

### Signal disposition and propagation

- Verify children get default signal dispositions where expected.
- Verify shell-installed handlers do not leak into exec'd children.
- Verify signal interruption of builtins and waits yields intended statuses.
- Verify `SIGINT`, `SIGPIPE`, `SIGTSTP`, and `SIGCHLD` handling semantics.

### Job-control-adjacent behavior

- Even if full job control is not a goal, verify:
  - process groups for foreground pipelines
  - stopped process propagation
  - `WUNTRACED` handling in interactive mode
  - terminal handoff/restore correctness

## 5. `PWD` And Path Semantics

Goal: eliminate ambiguity around logical vs physical cwd behavior.

### Define shell policy

- Decide explicitly whether `epsh` is:
  - logical by default (`PWD` tracks user path)
  - physical by default (`PWD` tracks kernel-resolved path)
  - mixed, with documented command-specific behavior
- Document how `cd`, `pwd`, and `$PWD` relate.

### Consistency audit

- Verify `cd` updates shell cwd state, process cwd state, and `PWD` consistently.
- Verify `pwd` output matches the chosen logical/physical policy.
- Verify relative path resolution for:
  - redirections
  - `.`
  - glob
  - PATH search helpers
  - `test`
  - command substitution work dirs
- Audit every use of `canonicalize`, `current_dir`, and `to_path_buf` for policy violations.

### Symlink coverage

- Add tests around symlinked directories:
  - `cd symlink && pwd`
  - `cd symlink/..`
  - relative opens after logical `cd`
  - interaction with `.` and redirections

## 6. PATH / Env Merge Semantics

Goal: make env assembly and PATH lookup semantics explicit and testable.

### Child environment assembly

- Audit precedence rules among:
  - inherited foreign env entries
  - exported shell vars
  - prefix assignments
- Verify overwrites are deterministic and byte-correct.
- Verify unexported shell vars never leak to children.
- Verify `exec` uses the same merge rules as normal spawn.

### PATH lookup

- Add more coverage for:
  - empty PATH segments
  - relative PATH entries
  - PATH updates during shell lifetime
  - command hashing assumptions, if any are introduced later
- Verify `command -v`, `type`, external exec, and builtin `which`-style paths stay aligned.

### Environment corner cases

- Add tests for inherited non-shell env names that should be preserved to children.
- Verify shell identifier rules are separate from foreign env preservation rules.
- Audit environment import/export behavior under `env_clear` builder mode.

## 7. Command Substitution Edge Cases

Goal: make `$(...)` and backticks reliable under nesting and failure.

### Output semantics

- Verify trailing newline trimming exactly matches intended shell behavior.
- Verify interior newlines are preserved.
- Verify invalid UTF-8 bytes survive unchanged except for required newline trimming.

### Execution semantics

- Test nested command substitutions.
- Test command substitution inside double quotes and unquoted context.
- Test command substitution failures under `set -e`.
- Test variable assignments and side effects inside command substitutions.
- Verify stderr routing and redirection behavior.

### Resource behavior

- Audit pipe/fork setup in command substitution to avoid leaks and deadlocks.
- Verify builtins optimized into command substitution still match external behavior where required.

## 8. Glob / Pattern Exactness

Goal: ensure pathname expansion and pattern matching are stable and compatible.

### Pathname expansion

- Add tests for:
  - ordering guarantees
  - dotfile matching rules
  - nested directories
  - escaped metacharacters
  - bracket expressions
  - ranges and negated classes
  - dangling symlinks
  - mixed UTF-8 and invalid-byte filenames
- Verify unmatched globs stay literal when intended.
- Verify `set -f` / `noglob` disables pathname expansion everywhere it should.

### Pattern matching helpers

- Audit `fnmatch` behavior used by:
  - parameter trim operators
  - `case`
  - glob
- Add shared regression cases so these contexts do not drift semantically.

## 9. NUL-Byte Policy And API Consistency

Goal: be explicit about the one byte Unix process/file APIs do not permit in strings.

### Policy

- Document clearly:
  - shell data may contain arbitrary bytes except NUL where OS APIs forbid it
  - errors on NUL are expected boundary errors, not encoding bugs
- Ensure all public APIs fail consistently on embedded NUL where required.

### API review

- Audit byte-safe APIs for consistent naming and behavior:
  - `set_var_bytes`
  - `get_var_bytes`
  - `set_args_bytes`
  - path resolution helpers
  - exported env helpers
- Decide which UTF-8 convenience APIs remain and how they should document lossy assumptions.

### Tests

- Add targeted tests that NUL-containing data:
  - is rejected cleanly at exec boundaries
  - does not cause panics
  - does not partially mutate shell state before failing

## 10. Regression Tests And Capability Matrices

Goal: keep future changes honest with OS-observable tests, not just unit coverage.

### Byte-correctness matrix

- Maintain the capability matrix from `OSSTRING.md` with rows:
  - argv to child
  - env to child
  - inherited env
  - script path
  - redirection output
  - redirection input
  - `cd`
  - `test`
  - `.`
  - PATH search
  - glob
  - command substitution
- Keep columns for:
  - ASCII
  - valid UTF-8 non-ASCII
  - invalid UTF-8 bytes

### Fixture strategy

- Keep test fixtures as repo-native Rust binaries.
- Prefer helpers that expose raw OS observations:
  - argv bytes
  - env bytes
  - path bytes + stat result
- Avoid helper layers that reintroduce UTF-8 assumptions.

### Cross-environment realism

- Continue skipping raw-path tests on hosts/filesystems that reject invalid path bytes.
- Add comments to each such test explaining what condition caused the skip.
- Keep byte-oriented env/argv tests unconditional because they do not depend on filesystem acceptance.

### Test organization

- Group high-value shell-correctness tests by semantic area instead of only by implementation file.
- Prefer integration tests over unit tests when behavior crosses parser/expander/evaluator/OS boundaries.
- When fixing a semantic bug, add:
  - a narrow regression test for the exact bug
  - at least one nearby matrix-style test to guard the broader behavior

## Deferred Nice-To-Haves

These matter less than the items above, but are worth tracking.

- More exhaustive mksh/dash differential coverage for edge semantics.
- Dedicated stress tests for fd churn and nested substitutions.
- Optional property-style tests for pattern matching and field splitting.
- A short design note documenting which dash/posh/mrsh behaviors are intentionally followed and where `epsh` diverges.
