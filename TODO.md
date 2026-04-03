# TODO

## Current Status

- **156/167 mksh conformance tests passing (93%)**
- 230 tests (136 unit + 94 integration)
- Zero clippy warnings
- ~9.6k lines of Rust across 15 modules

## Remaining 13 Failures

### Test runner limitations (3) — dash passes, epsh matches
- `glob-bad-2` — needs file-setup with symlinks (test creates files)
- `glob-range-6` — needs file-setup for character class testing
- `regression-31` — uses `script:` tag (writes script to file, not stdin)

### Worth fixing (2)
- `comsub-2` — comment ending with `\<newline>` inside `$(...)` breaks parsing
- `exit-eval-1` — some eval + command substitution exit status sub-cases

### Edge-case IFS / expansion semantics (4)
- `IFS-subst-10` — `${var=$*}` with IFS="" should join in scalar context
- `IFS-subst-6` — `${x#$*}` with IFS="" trim: `$*` should concatenate
- `xxx-variable-syntax-4` — `${*:+ }` with various IFS values
- `varexpand-null-3` — `""$@` empty-string argument counting

### Requires missing features (3)
- `IFS-subst-3-lcl` — needs `set -x` (xtrace) output
- `xxx-clean-chars-1` — non-UTF-8 byte handling (Rust strings are UTF-8)
- `regression-35` — heredoc in nested function definition (heredoc body lifecycle)

### Pattern matching edge cases (1)
- `glob-range-1` — character class range edge cases (`[!-ab]*` etc.)

## Embedding API follow-ups

### Cancellation is passive, not preemptive
`check_cancel` only fires between commands and after `waitpid` returns. A
long-running `sleep 1000` blocks in `child.wait()` and cancellation can't
interrupt it. For true responsiveness, `eval_external` needs a non-blocking
wait loop that polls both the child and the cancel flag (similar to nerv's
bash tool pattern). Pipeline waitpid should also check cancel between stages
and kill remaining children if triggered mid-pipeline.

### Sink relay threads aren't joined
In `eval_external` with sinks, relay threads are spawned for stdout/stderr
but the `JoinHandle`s are dropped. There's a race: `child.wait()` returns
but the relay thread might not have flushed the last pipe buffer. Fix: store
the handles and join them after `wait()`.

### Pipeline stages get separate process groups
`setpgid(0, 0)` in each pipeline child puts each stage in its own group.
Killing "the pipeline" requires iterating all stage PIDs. A shared pipeline
group (first child's PID as PGID, others join it) would allow a single
`kill(-pgid, SIGKILL)`. Low priority — current approach works, just verbose.

## Architecture Notes

### Completed structural improvements
1. **Single-pass word tokenization** — lexer builds WordPart directly (no re-parsing)
2. **ExitStatus newtype** — private field, type-safe methods (.code(), from_bool(), inverted())
3. **Expansion returns Result** — errors propagate cleanly
4. **Module split** — eval.rs, builtins.rs, redirect.rs, test_cmd.rs
5. **expand_pattern** — separate function for fnmatch-ready pattern expansion
6. **Heredoc body reading at newlines** — matches dash's parseheredoc
7. **Unified word-part builder** — lexer's read_word_parts handles Word and Brace contexts
8. **Per-shell cwd** — no process-global set_current_dir, all paths resolve via Shell.cwd
9. **Output sinks** — stdout/stderr captured via Arc<Mutex<dyn Write + Send>>
10. **Cancellation** — Arc<AtomicBool> flag with check points throughout eval
11. **Process group isolation** — setpgid(0,0) in all fork children
12. **pipefail** — POSIX.1-2024 set -o pipefail support

### Key dash-compatible mechanisms
- **EV_TESTED** errexit suppression for conditionals and `&&`/`||`
- **in_forked_child** prevents double-fork pipe fd leaks
- **IO_NUMBER** detection for `2>`, `1<` etc.
- **Backslash-newline** eating via peek()/advance() (like dash's pgetc_eatbnl)
- **Syntax context tracking** for `${}` words (trim vs default/assign ops)
- **EXIT trap** execution in subshells and command substitutions
- **Arithmetic noeval** for ternary/logical short-circuit
