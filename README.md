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

- **95% POSIX conformance** on the mksh test suite (158/167 dash-passable tests)
- **Per-shell working directory** (`Shell.cwd`) — no process-global state, safe for concurrent use
- **Cancellation** via `Arc<AtomicBool>` — kills child process groups within milliseconds
- **Output capture** via `Arc<Mutex<dyn Write + Send>>` sinks — no pipe wrangling
- **Parse-then-execute** — inspect the AST between parsing and execution
- **Process group isolation** — every child gets `setpgid`; cancellation kills the tree
- **~10k lines of Rust + libc** — no other dependencies
- 237 tests (unit + integration), zero clippy warnings

## Usage

```rust
use epsh::eval::Shell;
use epsh::parser::Parser;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}};
use std::path::PathBuf;

let mut shell = Shell::new();
shell.set_cwd(PathBuf::from("/some/project"));

// One-shot execution
let exit_code = shell.run_script("cargo test 2>&1");

// Parse once, execute later (for permission checking / AST inspection)
let program = Parser::new("rm -rf /tmp/build").parse().unwrap();
// ... inspect program.commands, check permissions ...
let status = shell.run_program(&program);

// Cancellation
let cancel = Arc::new(AtomicBool::new(false));
shell.set_cancel_flag(cancel.clone());
// From another thread: cancel.store(true, Ordering::Relaxed);

// Output capture
let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
shell.set_stdout_sink(stdout.clone());
shell.run_script("echo hello");
let output = String::from_utf8_lossy(&stdout.lock().unwrap());
```

## CLI

```sh
epsh -c 'echo hello'          # run a command string
epsh script.sh                 # run a script file
echo 'echo hello' | epsh      # read from stdin (pipe)
epsh                           # no args + tty → prints usage
```

## Design

epsh's conformance target is dash, the Debian default `/bin/sh`. The
implementation was built by studying dash's source directly — not by
guessing at POSIX spec wording. Key mechanisms ported from dash:

- Single-pass word tokenization with syntax-context tracking
- `EV_TESTED` errexit suppression in conditionals and `&&`/`||`
- `in_forked_child` to prevent double-fork pipe fd leaks
- Heredoc body reading at newlines (not at parse time)
- `$((..))` arithmetic with short-circuit `noeval` for ternary/logical ops

Where we diverged from dash: typed AST instead of control-character strings,
`Result<T, ShellError>` instead of setjmp/longjmp, `ExitStatus` newtype
instead of bare integers.

## Not implemented (by design)

- Job control (`fg`, `bg`, `jobs`, process groups for interactive use)
- Interactive features (prompt, history, line editing)
- Aliases
- Here-strings (`<<<`), extended globs, arrays, `typeset`, `select`

These are ksh/bash extensions or interactive features. A coding agent
doesn't need them.

## Testing

```sh
cargo test                      # 237 tests (136 unit + 101 integration)
cargo build && perl check.pl \
  -p ./target/debug/epsh \
  -s check-epsh.t              # 158/167 mksh conformance
```

## License

MIT
