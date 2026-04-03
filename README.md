# epsh

Embeddable POSIX shell for Rust. Built for coding agents.

epsh is a non-interactive shell that runs scripts and commands. It exists so
that Rust-based coding agents can execute shell commands in-process rather
than shelling out to bash. It's a library first, binary second.

## Why not just use bash?

Spawning bash works, but it gives you no control. You can't cancel a
runaway command without killing the process group yourself. You can't
capture output without pipe gymnastics. You can't run two commands with
different working directories in the same process. You can't inspect the
AST before execution for permission checking.

epsh gives you all of that as a Rust API.

## Why not an existing shell library?

Existing embeddable shells either target interactive use (and carry the
complexity of job control, line editing, history, SIGTSTP handling) or are
POSIX-incomplete. epsh takes the opposite approach: **strip out everything
interactive and get the scripting semantics right**.

No job control. No prompt. No aliases. No history. No terminal handling.
This eliminates entire categories of bugs and reduces the surface to what
a coding agent actually needs: run a script, get the output, check the
exit code.

## What you get

- **96% POSIX conformance** on the mksh test suite (161/167 dash-passable tests)
- **Builder API** — configure shell options, sinks, timeouts, cancellation in one chain
- **Per-shell working directory** — no process-global state, safe for concurrent use
- **Cancellation + timeout** — kills child process groups within milliseconds
- **Output capture** via sinks — no pipe wrangling
- **Parse-then-execute** — inspect the AST between parsing and execution
- **Process group isolation** — every child gets `setpgid`; cancellation kills the tree
- **Fork-free command substitution** — `$(echo ...)` runs in-process, 3.6x faster than dash
- **External command handler** — plug in your own process spawner for sandboxing or job control
- **Interactive shell primitives** — `tcsetpgrp`, `WUNTRACED`, `Stopped` for building shells on top
- **10k lines of Rust + libc** — no other dependencies
- 314 tests, zero clippy warnings

## Usage

```rust
use epsh::eval::Shell;
use epsh::parser::Parser;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use std::path::PathBuf;
use std::time::Duration;

// Builder API — recommended for embedders
let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
let cancel = Arc::new(AtomicBool::new(false));
let mut shell = Shell::builder()
    .cwd(PathBuf::from("/some/project"))
    .errexit(true)
    .pipefail(true)
    .stdout_sink(stdout.clone())
    .cancel_flag(cancel.clone())
    .timeout(Duration::from_secs(120))
    .build();

// One-shot execution
let exit_code = shell.run_script("cargo test 2>&1");

// Parse once, execute later (for permission checking / AST inspection)
let program = Parser::new("rm -rf /tmp/build").parse().unwrap();
// ... inspect program.commands, check permissions ...
let status = shell.run_program(&program);

// Output capture
let output = String::from_utf8_lossy(&stdout.lock().unwrap());

// Builtin detection (for permission systems)
use epsh::builtins::{is_builtin, BUILTIN_NAMES};
assert!(is_builtin("echo"));
assert!(!is_builtin("rm"));
```

### Builder options

| Method | Description |
|--------|-------------|
| `.cwd(PathBuf)` | Working directory (default: process cwd) |
| `.errexit(bool)` | Exit on error (`set -e`) |
| `.nounset(bool)` | Error on unset variables (`set -u`) |
| `.xtrace(bool)` | Print commands before execution (`set -x`) |
| `.pipefail(bool)` | Return highest nonzero pipeline status |
| `.interactive(bool)` | Enable tcsetpgrp/WUNTRACED for job control |
| `.stdout_sink(Arc<Mutex<dyn Write + Send>>)` | Capture stdout |
| `.stderr_sink(Arc<Mutex<dyn Write + Send>>)` | Capture stderr |
| `.cancel_flag(Arc<AtomicBool>)` | Cancellation flag |
| `.timeout(Duration)` | Execution deadline |
| `.env_clear()` | Don't inherit process environment |
| `.external_handler(ExternalHandler)` | Custom process spawner |

### Building an interactive shell on epsh

epsh deliberately excludes interactive features (prompt, history, line
editing, job control builtins). However, it provides the minimal primitives
needed for an interactive shell to be built on top:

- **`external_handler`**: Replace the default fork+exec with your own process
  spawner. Your handler receives expanded args and env pairs with redirections
  already applied to fds. This lets you own the fork/exec/wait cycle for
  terminal and job control.

- **`interactive` mode**: When enabled, pipelines call `tcsetpgrp` to give the
  foreground process group the terminal, and `waitpid` uses `WUNTRACED` to
  detect stopped processes. `ShellError::Stopped { pid, pgid }` propagates up
  with the pipeline's process group ID, so you can save the job and resume it
  later with `kill(pgid, SIGCONT)` + `tcsetpgrp`.

- **`BUILTIN_NAMES`** / **`is_builtin()`**: For command-word coloring and
  completion in the prompt.

Everything else — prompt rendering, line editing, history, `fg`/`bg`/`jobs`
builtins, signal mask management — is the interactive shell's responsibility.
epsh handles parsing, expansion, control flow, builtins, and redirections.

## CLI

```sh
epsh -c 'echo hello'          # run a command string
epsh script.sh                 # run a script file
echo 'echo hello' | epsh      # read from stdin (pipe)
epsh                           # no args + tty → prints usage
```

## Performance

Fork-dominated operations (the typical coding agent workload) are at parity
with dash:

| Operation | vs dash | Notes |
|-----------|---------|-------|
| Builtin command substitution | **3.6x faster** | Fork-free `$(echo ...)` |
| Heredocs | **0.8x** | Faster than dash |
| Pipelines | **1.1x** | posix_spawn, exec-direct |
| External commands | **1.1x** | At parity |
| Tight arithmetic loops | **~28x** | String allocation dominated |

## Design

epsh's conformance target is dash, the Debian default `/bin/sh`. The
implementation was built by studying dash's source directly — not by
guessing at POSIX spec wording. Key mechanisms ported from dash:

- Single-pass word tokenization with syntax-context tracking
- `EV_TESTED` errexit suppression in conditionals and `&&`/`||`
- `in_forked_child` / `ev_exit` to prevent double-fork and enable exec-direct
- Heredoc body reading at newlines (not at parse time)
- `$((..))` arithmetic with short-circuit `noeval` for ternary/logical ops
- CTLESC escape marker for preserving backslash semantics through expansion
- PUA-based byte encoding for non-UTF-8 data preservation

Where we diverged from dash: typed AST instead of control-character strings,
`Result<T, ShellError>` instead of setjmp/longjmp, `ExitStatus` newtype
with private field and type-safe methods.

## Not implemented (by design)

These live in the interactive shell layer, not in epsh:

- Job control builtins (`fg`, `bg`, `jobs`) — use `external_handler` + `Stopped`
- Prompt, history, line editing — use rustyline/reedline/etc.
- Aliases
- Here-strings (`<<<`), extended globs, arrays, `typeset`, `select`

## Testing

```sh
cargo test                      # 314 tests
cargo test --test api_stability # API surface regression tests
cargo test --test embedding     # embedding API tests (31)
cargo test --test integration   # shell behavior tests (122)
cargo build && perl check.pl \
  -p ./target/debug/epsh \
  -s check-epsh.t              # 161/167 mksh conformance
```

## License

MIT
