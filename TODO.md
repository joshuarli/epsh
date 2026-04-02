# TODO

## Current Status

- 136 internal tests passing
- mksh conformance: **113/167** (68%) — dash passes 167/167
- 54 remaining failures, all tests that dash passes

## Remaining Failures by Category

### IFS field splitting (8 tests)
`IFS-null-1` `IFS-subst-1` `IFS-subst-3-arr` `IFS-subst-3-ass` `IFS-subst-3-lcl` `IFS-subst-6` `IFS-subst-7` `IFS-subst-10`

Root cause: `$*` with empty IFS should concatenate without separator. `${var-$*}` and `${var=$*}` don't apply IFS correctly to the word. `IFS-subst-6`: `${x#$*}` with `IFS=` should concatenate `$*` as `ab`, trim `abc` → `c`. Need to handle `$*` inside parameter expansion words with correct IFS context.

### Heredoc processing (8 tests)
`heredoc-3` `heredoc-5` `heredoc-comsub-5` `heredoc-subshell-2` `heredoc-weird-1` `heredoc-weird-4` `heredoc-weird-5` `heredoc-quoting-subst`

Root causes:
- `heredoc-3`: EOF without final newline after delimiter — parser doesn't handle this
- `heredoc-5`: `\<newline>` in unquoted heredoc body should be line continuation
- `heredoc-weird-*`: Backslash handling in heredoc bodies (continuation, literal `\`)
- `heredoc-quoting-subst`: `\"` in unquoted heredoc should produce literal `"`
- `heredoc-subshell-2`: Heredoc inside `(...)` subshell — delimiter not found
- `heredoc-comsub-5`: Complex heredoc+comsub nesting

### Single quotes in `${}` double-quoted context (6 tests)
`single-quotes-in-quoted-braces` `single-quotes-in-brace-pattern` `single-quotes-in-heredoc-braces` `single-quotes-in-nested-quoted-braces` `single-quotes-in-nested-brace-pattern` `single-quotes-in-heredoc-nested-braces`

Root cause: Inside `"${var+...}"`, single quotes should be literal (not quoting). The `read_brace_word` parser function needs to track double-quote context and pass it to `parse_word_parts`. Currently `parse_word_parts` always treats `'...'` as quoting. Fix requires either: (a) producing WordParts directly in `read_brace_word` instead of raw text, or (b) adding a `literal_single_quotes` flag to `parse_word_parts`.

Reference: dash uses a syntax stack (`synstack`) that pushes DQSYNTAX vs BASESYNTAX when entering `${...}`. See `parser.c:1476`.

### Backslash-newline continuation (5 tests)
`bksl-nl-1` `bksl-nl-2` `bksl-nl-4` `bksl-nl-7` `bksl-nl-8`

Root cause: `\<newline>` not collapsed in several contexts:
- Inside `${...}` variable names (bksl-nl-1, bksl-nl-2)
- Inside `$((...))` arithmetic (bksl-nl-4 — also causes subtract-overflow panic)
- In heredoc delimiter words (bksl-nl-7)
- In multi-char operators `&&`, `||`, `<<`, `;;`, case `|` (bksl-nl-8)

Reference: dash's `pgetc_eatbnl()` in `parser.c:852` handles this — it's called in most contexts except inside single quotes.

### Backtick quoting (2 tests)
`regression-12` `regression-22`

Root cause: Backslash-escaped quotes inside backticks not handled. `` `echo \"$x\"` `` should preserve the escaped quotes. Nested backticks `` `echo \`echo there\` ` `` need recursive parsing with backslash-backtick unescaping.

### Regression / misc (13 tests)
- `regression-6`: `$(echo \( )` — backslash-escaped parens in comsub
- `regression-13`: Subshell fd leak — `(: ; cat) | sleep 1`
- `regression-14`: `(cmd) 2>/dev/null` — stderr redirect not applied to subshell errors
- `regression-21`: `case -x in -\?) ...` — backslash in case pattern not quoting the `?`
- `regression-22`: Nested backticks (see above)
- `regression-30`: `${a{b}}` should be syntax error — BadSubst detected but exit code not propagated
- `regression-31`: `read` on partial line (no trailing newline) — `script:` tag handling
- `regression-35`: Heredoc in function definition — temp file lifecycle
- `regression-39`: `set -e; echo \`false\`` should not exit
- `regression-61`: EXIT trap in subshell not executed

### Other (6 tests)
- `arith-lazy-3`: Ternary `0 ? x=1 : 2` evaluates assignment on non-taken branch
- `comsub-2`: Comment ending with `\<newline>` inside `$(...)` breaks parsing
- `exit-eval-1`: `eval ' $(false)'` should set `$?` to 1
- `exit-trap-3`: EXIT traps in comsub/subshell not executed
- `expand-unglob-dblq`/`expand-unglob-unq`: Complex `${v+...}` with nested braces/quotes — parse error on brace depth tracking
- `expand-weird-1`: `${#}` confused with `${#var}` length operator
- `glob-bad-2`: Glob through symlink directory not followed
- `glob-range-1`/`glob-range-6`: Character class range edge cases
- `oksh-input-comsub`: `$(<file)` syntax not implemented
- `utilities-getopts-1`: Leading `:` in optstring should suppress errors
- `varexpand-null-3`: `""$@` field counting wrong
- `xxx-clean-chars-1`: Non-UTF-8 bytes in input rejected
- `xxx-quoted-newline-1`: `\<newline>` inside `${}` not collapsed
- `xxx-variable-syntax-4`: `${*:+ }` with `IFS=` should produce empty

## What Was Done

### Modules implemented
1. **lexer** — Full tokenizer with quoting, operators, heredoc delimiters, backslash-newline (partial)
2. **parser** — Recursive-descent for full POSIX grammar, compound_list vs complete_command separation, heredoc body reading, recursive command substitution parsing
3. **ast** — Typed enums with Span on every node, Word/WordPart nesting
4. **var** — HashMap storage, scope stack, export/readonly, special params
5. **expand** — All 10 POSIX param ops, `$@`/`$*` with word_break, IFS field splitting, tilde, glob, command substitution wiring, arithmetic evaluation
6. **arith** — Full C-like integer arithmetic with all operators, precedence, assignment, hex/octal
7. **glob** — Custom fnmatch + glob using std::fs, no libc dependency
8. **eval** — Simple commands, pipelines (fork+pipe), subshells, compound commands, 25+ builtins, redirections with fd save/restore, function calls with scope, errexit (EV_TESTED model), EXIT trap execution
9. **sys** — Minimal POSIX wrappers: fork, pipe, dup2, waitpid, read, write, close, execvp, umask, getuid, isatty, fcntl

### Key fixes applied during mksh test iteration
- `$@`/`$*` expansion: separate word-break fields, IFS[0] joining
- errexit: leaf-only check, EV_TESTED propagation for if/while/&&/||/!
- `eval` builtin: propagates break/continue/return/exit
- `.` (dot): fatal error on missing file, propagates control flow
- `test`/`[`: has_operand guard prevents `-e` etc. consuming `)` as file arg
- `break`/`continue`: Illegal number error, clamp to loop depth
- `command`: now finds builtins (not just external commands)
- `read`: char-by-char from fd 0 (works with redirections), -r flag, multi-var IFS splitting
- Redirections: applied before builtin execution (was setup+teardown with no effect)
- EXIT trap: fires on exit/script-end
- echo/printf: use raw `write(2)` to bypass Rust stdout buffering
- Parser: `parse_compound_list` allows newlines as separators inside compound commands
- Parser: Assignment tokens after command name treated as word args (`local X=val`)
- Parser: `parse_cmdsubst_content` recursively parses `$(...)` content
- Param expansion: `expand_param_to_fragments` preserves quoting through `${var+word}`
