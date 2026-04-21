# OS / String Plan

## Goal

Make `epsh` correct for Unix shell data that is not valid UTF-8.

The target is not "use `OsString` everywhere". The target is:

- shell syntax stays as shell syntax
- shell data becomes byte-preserving
- OS boundaries use `OsStr` / `OsString` / `Path` on Rust APIs
- libc boundaries use raw bytes / `CString`

## Core Position

`OsString` is the right Rust boundary type for filesystem and process APIs on Unix.
It is not the right universal runtime type for a POSIX shell.

The shell's internal data model should distinguish:

- Shell syntax
  - reserved words
  - operators
  - variable names
  - function names
  - type: `String`
- Shell data
  - expanded words
  - variable values
  - positional parameters
  - command names
  - argv entries
  - pathnames
  - environment values
  - type: byte-preserving shell-owned type, e.g. `ShellBytes(Vec<u8>)`
- OS interface
  - `Path` / `PathBuf`
  - `OsStr` / `OsString`
  - `CString`

## Why The Current Model Is Wrong

Today `epsh` uses `String` for most shell data and a PUA encoding shim in
`src/encoding.rs` to preserve invalid bytes in some paths. That is a useful
bridge, but it is not a correct runtime model.

Current correctness gaps:

- `execvp` converts shell strings with `CString::new(a.as_bytes())`, which sends
  UTF-8 bytes for PUA codepoints instead of the original bytes.
  - `src/builtins.rs`
- external spawn uses `String` argv/env directly
  - `src/eval.rs`
- environment import uses `std::env::vars()`, which assumes UTF-8
  - `src/var.rs`
- CLI argv import uses `std::env::args()`, which assumes UTF-8
  - `src/main.rs`
- glob drops non-UTF-8 names with `to_str()`
  - `src/glob.rs`
- path-based builtins and tests operate on `&str` paths
  - `src/redirect.rs`
  - `src/builtins.rs`
  - `src/test_cmd.rs`
- `PWD` is written with `to_string_lossy()`, which is not semantically correct
  for shell state
  - `src/builtins.rs`

## Desired Invariants

These invariants define success.

1. Any non-NUL byte sequence accepted by the shell as data remains exact until an
   operation explicitly transforms it.
2. A pathname containing non-UTF-8 bytes can be:
   - passed as an argument
   - redirected to or from
   - globbed
   - used by `cd`
   - checked by `test` / `[`
   - sourced with `.`
3. Environment values with non-UTF-8 bytes survive:
   - import into the shell
   - export from shell variables
   - prefix assignment to an external command
   - exec/spawn to a child process
4. `argv` bytes seen by a child process exactly match the bytes produced by shell
   expansion.
5. `cwd` and path resolution remain byte-correct at the OS boundary.

## Type Design

Introduce a dedicated internal byte string type:

```rust
pub struct ShellBytes(Vec<u8>);
```

Minimum API:

```rust
impl ShellBytes {
    pub fn as_bytes(&self) -> &[u8];
    pub fn from_vec(bytes: Vec<u8>) -> Self;
    pub fn into_vec(self) -> Vec<u8>;
    pub fn from_str_lossless(s: &str) -> Self;
    pub fn to_os_string(&self) -> OsString;
    pub fn from_os_str(s: &OsStr) -> Self;
    pub fn to_cstring(&self) -> Result<CString, NulError>;
}
```

On Unix these conversions should use `std::os::unix::ffi::{OsStrExt, OsStringExt}`.

Do not route new code through the PUA encoding layer except as a temporary
compatibility bridge.

## Data Model Changes

Keep as `String`:

- variable names
- function names
- reserved words
- operator tokens
- parser control structures

Move to `ShellBytes`:

- `WordPart::Literal`
- `WordPart::SingleQuoted`
- heredoc literal bodies
- expanded fields
- command substitution output
- shell variable values
- positional parameters
- `$0`
- command names after expansion
- path arguments after expansion
- environment values

Likely split AST and runtime concerns:

- parser may continue to build syntax-oriented structures first
- expansion output should become byte-based as early as possible
- long term, lexer/parser should become byte-native instead of `Vec<char>`

## Environment Model

Do not treat the inherited process environment as if every entry were valid shell
syntax and valid UTF-8.

Use two concepts:

- shell variables
  - keyed by valid shell identifier
  - values are `ShellBytes`
- inherited external environment
  - raw `name=value` byte entries or equivalent structured form
  - may contain names or values not representable as shell identifiers

Spawn/exec should merge:

- exported shell variables
- preserved inherited environment entries not shadowed by shell exports
- command prefix assignments

This avoids losing environment state during import/export cycles.

## Migration Phases

### Phase 1: Boundary Helpers

Add a small module for Unix conversions between:

- `ShellBytes` and `OsStr`
- `ShellBytes` and `CString`
- `ShellBytes` and current PUA `String` bridge

This phase should not change behavior yet. It creates the conversion seams.

### Phase 2: Runtime Values

Convert runtime-expanded values from `String` to `ShellBytes`:

- expansion results
- variable values
- positional parameters
- command substitution output

Keep parser-facing syntax names as `String`.

### Phase 3: Process Boundaries

Fix argv/env correctness for external commands:

- replace `std::env::args()` with `args_os()` in `src/main.rs`
- replace `std::env::vars()` with `vars_os()` or raw Unix env handling in `src/var.rs`
- feed `Command::new`, `.args`, and `.env` with `OsStr` / `OsString`
- build `CString` for direct `execvp` from raw shell bytes, not UTF-8 string bytes

### Phase 4: Path Boundaries

Change path resolution and path-using builtins to accept byte-preserving values:

- `resolve_path`
- redirections
- `.`
- `cd`
- `test` / `[`
- `which`

`cwd` may remain `PathBuf`, but conversion into it must be byte-correct.

### Phase 5: Glob

Rewrite glob to operate on Unix directory entry bytes.

Requirements:

- do not drop non-UTF-8 names
- preserve exact entry bytes in matches
- keep existing wildcard semantics
- preserve current behavior for dotfiles and escaped glob metacharacters

This is one of the highest-value correctness changes because it materially affects
what filenames the shell can see.

### Phase 6: Public API Review

Review public API surface in `eval.rs` and tests:

- `set_var`
- `get_var`
- `set_args`
- `run_script`
- `resolve_path`

Possible approach:

- keep current UTF-8 convenience APIs
- add byte-safe Unix-specific APIs alongside them
- avoid breaking embedders unnecessarily until the byte model is stable

### Phase 7: Parser / Lexer Rewrite

Move the shell core from `String` / `char` parsing to byte-native parsing.

This is the end-state cleanup phase. It removes the remaining conceptual mismatch
between shell semantics and Unicode-centric tokenization.

## Correctness Measurement

The improvement should be measured at OS-observable boundaries, not only with
unit tests over internal helpers.

The question is not "did conversion code run".
The question is "can `epsh` now preserve bytes that the current build loses".

### Test Philosophy

Each new test should compare old behavior and new behavior in a way that would
have failed materially before the change.

Good tests:

- assert exact child `argv` bytes
- assert exact child env bytes
- assert a non-UTF-8 filename is matched by glob
- assert redirection opens the intended non-UTF-8 filename
- assert `cd` and `test -e` succeed on non-UTF-8 paths
- fail on current `main` or would have failed before the migration step that
  fixes them

Weak tests:

- only checking helper conversion functions
- only round-tripping through PUA strings
- only testing Unicode-valid filenames

## Recommended Test Harness

Add small helper executables for integration tests that print raw bytes in a
stable representation, preferably hex.

Suggested fixtures:

- `tests/fixtures/show_argv.rs`
  - prints each argument as lowercase hex, one line per arg
- `tests/fixtures/show_env.rs`
  - prints selected environment values as lowercase hex
- `tests/fixtures/stat_path.rs`
  - checks whether the path passed in argv exists and reports the raw bytes it saw

If keeping fixtures as Rust binaries is awkward, a small Perl helper is also fine
because Perl handles raw Unix bytes well and the repo already uses Perl in the
conformance flow.

## Tests That Materially Show Improvement

### 1. External argv byte round-trip

Run `epsh -c` with an argument containing bytes like `0x80`, `0xff`, or mixed
valid UTF-8 plus invalid bytes, then invoke the argv-dump helper.

Expected:

- child sees exact original bytes

Current behavior likely fails because direct exec/spawn goes through UTF-8
`String` paths.

### 2. External env byte round-trip

Set a shell variable to non-UTF-8 bytes, export it, and run the env-dump helper.

Expected:

- child sees exact raw bytes for the exported variable

Add a second case for prefix assignment:

- `X=<bytes> helper`

### 3. Inherited env preservation

Launch `epsh` from the integration test with a parent env value containing
non-UTF-8 bytes.

Expected:

- child command run by `epsh` still sees those exact bytes unless explicitly
  overwritten

This exposes the current `std::env::vars()` UTF-8 loss.

### 4. Non-UTF-8 script path

Execute a script file whose pathname contains invalid UTF-8 bytes.

Expected:

- `epsh script-path` works

This forces `main.rs` argv import and file open paths to be correct.

### 5. Redirection to non-UTF-8 filename

Use `printf` or `echo` with output redirection into a filename containing invalid
UTF-8 bytes, then verify the file exists and contains the expected bytes.

Expected:

- redirect succeeds
- opened file is the exact intended path

### 6. Input redirection from non-UTF-8 filename

Create a file with a non-UTF-8 pathname and read it with `< file`.

Expected:

- command receives the file contents

### 7. `cd` into non-UTF-8 directory

Create a directory with invalid UTF-8 bytes and run:

```sh
cd <dir> && pwd
```

Expected:

- `cd` succeeds
- `pwd` and shell state remain consistent

Prefer checking shell behavior over exact display formatting if output escaping is
still under transition.

### 8. `test -e`, `-f`, `-d`, `-L` on non-UTF-8 paths

These should succeed for matching file kinds.

This directly measures fixes in `src/test_cmd.rs`.

### 9. `.` on non-UTF-8 path

Source a script whose filename contains invalid UTF-8 bytes.

Expected:

- script executes successfully

### 10. `which` / PATH search with non-UTF-8 directory entries

Set `PATH` to include a directory whose name contains invalid UTF-8 bytes and
place an executable inside it.

Expected:

- `command -v`
- `type`
- external execution by bare command name

all find the executable.

### 11. Glob matches non-UTF-8 filenames

Create files whose names include invalid UTF-8 bytes and run a glob that should
match them.

Expected:

- matches are returned
- unmatched files are not returned
- ordering remains stable

This is one of the clearest before/after tests because current glob explicitly
drops non-UTF-8 names.

### 12. Command substitution preserves non-UTF-8 bytes

Have a helper emit raw bytes to stdout, capture them with `$(...)`, and pass the
result to another helper that prints argv bytes.

Expected:

- bytes survive command substitution unchanged except for POSIX newline trimming

### 13. Here-doc and variable expansion with raw bytes

Where supported by the parser and runtime model, verify that raw bytes stored in
variables survive expansion into:

- argv
- redirection targets
- environment values

This guards against regressions once runtime values become byte-based.

## Metrics

Track progress with a simple matrix of capabilities.

Rows:

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

Columns:

- ASCII
- valid UTF-8 non-ASCII
- invalid UTF-8 bytes

Expected progression:

- ASCII stays green throughout
- valid UTF-8 stays green throughout
- invalid UTF-8 starts red in many rows and turns green phase by phase

The main success metric is the count of green cells in the `invalid UTF-8 bytes`
column. That is the part of the matrix where correctness is currently missing.

## Minimum Acceptance Bar

The migration is not complete until:

1. child `argv` and env tests prove exact byte preservation
2. glob sees and matches non-UTF-8 filenames
3. redirections and path-based builtins work on non-UTF-8 paths
4. inherited environment is not silently dropped or rewritten because of UTF-8
   assumptions

## Non-Goals

- making shell syntax identifiers support arbitrary bytes
- changing user-facing display/escaping policy before byte correctness is in place
- cross-platform abstraction beyond Unix semantics for this migration
