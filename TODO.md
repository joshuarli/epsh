# TODO

## Current Status

- **161/167 mksh conformance tests passing (96%)**
- 290 tests (141 unit + 27 embedding + 122 integration)
- Zero clippy warnings
- 10k lines of Rust across 16 modules

## Remaining 6 Failures

### Test runner limitation (1)
- `regression-31` — uses `script:` tag (writes script to file, not stdin)

### Edge-case IFS / expansion semantics (4)
- `IFS-subst-10` — `${var=$*}` with IFS="" should join in scalar context
- `IFS-subst-6` — `${x#$*}` with IFS="" trim: `$*` should concatenate
- `xxx-variable-syntax-4` — `${*:+ }` with various IFS values
- `varexpand-null-3` — `""$@` empty-string argument counting

### Requires missing features (1)
- `regression-35` — heredoc in nested function definition (heredoc body lifecycle)

## Performance vs dash

Fork-dominated operations (typical coding agent workload) are at parity:

| Operation | vs dash | Technique |
|-----------|---------|-----------|
| Builtin comsub | **3.6x faster** | Fork-free `$(echo ...)` |
| Heredocs | **0.8x** | Faster than dash |
| Pipelines | **1.1x** | posix_spawn, exec-direct |
| External commands | **1.1x** | At parity |
| Tight arith loops | ~28x | String allocation dominated |

The loop gap requires integer variable representation (dash's approach).

## Architecture Notes

### Completed structural improvements
1. **Single-pass word tokenization** — lexer builds WordPart directly (no re-parsing)
2. **ExitStatus newtype** — private field, type-safe methods (.code(), from_bool(), inverted())
3. **Expansion returns Result** — errors propagate cleanly
4. **Module split** — eval.rs, builtins.rs, redirect.rs, test_cmd.rs, encoding.rs
5. **expand_pattern** — separate function for fnmatch-ready pattern expansion
6. **Heredoc body reading at newlines** — matches dash's parseheredoc
7. **Unified word-part builder** — lexer's read_word_parts handles Word and Brace contexts
8. **Per-shell cwd** — no process-global set_current_dir, all paths resolve via Shell.cwd
9. **Output sinks** — stdout/stderr captured via Arc<Mutex<dyn Write + Send>>
10. **Cancellation + timeout** — Arc<AtomicBool> flag + Instant deadline, same check points
11. **Process group isolation** — setpgid in all fork children, shared pipeline PGID
12. **pipefail** — POSIX.1-2024 set -o pipefail support
13. **CTLESC** — escape marker for preserving backslash semantics through expansion
14. **PUA byte encoding** — non-UTF-8 bytes preserved through the full pipeline
15. **Fork-free comsub** — pure builtins run in-process with output capture
16. **exec-direct** — pipeline children exec external commands without double-fork
17. **posix_spawn** — no pre_exec allows Rust to use fast spawn path
18. **Integer var cache** — avoids string→i64 parse in arithmetic hot path
19. **Shell::builder()** — fluent API for embedder configuration
20. **BUILTIN_NAMES** — public constant for permission systems

### Key dash-compatible mechanisms
- **EV_TESTED** errexit suppression for conditionals and `&&`/`||`/`!`
- **in_forked_child** prevents double-fork pipe fd leaks
- **ev_exit** enables exec-direct in pipeline children
- **IO_NUMBER** detection for `2>`, `1<` etc.
- **Backslash-newline** eating via peek()/advance() (like dash's pgetc_eatbnl)
- **Syntax context tracking** for `${}` words (trim vs default/assign ops)
- **EXIT trap** execution in subshells and command substitutions
- **Arithmetic noeval** for ternary/logical short-circuit
- **set -x (xtrace)** with $PS4 prefix
