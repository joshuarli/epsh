Remaining libc:

 1. fork — `rustix::runtime::kernel_fork` is Linux-raw-only, hidden, and
    experimental; it bypasses libc pthread-atfork handling and is intended for
    libc-like runtimes, not general embeddable applications.
 2. execvp — Rustix has raw `execve` only in its experimental runtime API.
    `execvp` additionally implements PATH search and POSIX error/fallback
    behavior, so this belongs in an epsh-local or higher-level helper. In the
    ish embedding, ordinary external commands use ish's external handler and
    do not normally reach epsh's `execvp`; the direct use remains relevant to
    the `exec` builtin and handler-less embedders.
 3. _exit — `rustix::runtime::exit_group` is Linux-raw-only; retain libc on
    other Unix targets.
 4. sigaction/sigemptyset — Linux handler installation still uses libc because
    rustix's kernel_sigaction trampoline crashes on static musl (same pattern
    as ish); macOS uses a local libc FFI declaration.

## Fork

### What rustix provides

Rustix does provide a fork primitive: `rustix::runtime::kernel_fork`.

It is available only with the `runtime` feature and the `linux_raw` backend.
The `runtime` module is hidden from normal documentation and is explicitly
experimental. The API returns `Fork::ParentOf(Pid)` in the parent and
`Fork::Child(Pid)` in the child.

The implementation is a direct Linux `clone` syscall using
`CLONE_CHILD_SETTID`, rather than the platform libc `fork` wrapper. It does
not run `pthread_atfork` handlers and does not repair libc, allocator, or
thread-runtime state in the child.

This API exists for libc-like runtimes and low-level fork-then-exec users. It
is not intended to be a generally interchangeable, portable replacement for
`libc::fork`.

### PR #138

[bytecodealliance/rustix#138](https://github.com/bytecodealliance/rustix/pull/138)
is the relevant design discussion. Its central change was to make the
fork/exec boundary explicitly unsafe: `execve` should be a raw,
non-allocating operation suitable for the child of a fork, rather than a safe
or ergonomic process-launching API.

The PR documents the hazards of the child between fork and exec. In
particular, child code must avoid:

 - allocating through the global allocator;
 - acquiring locks that may have been held by another parent thread;
 - accessing thread-runtime state;
 - calling C functions that are not async-signal-safe;
 - using inherited shared memory or external state in ways that depend on the
   parent process;
 - relying on libc or runtime initialization that the raw fork did not perform.

The intended safe shape is therefore:

```text
prepare argv/env/fds in the parent
        |
        v
rustix kernel_fork
        |
        +-- parent: wait
        |
        +-- child: raw fd setup -> raw execve -> raw exit
```

### What epsh actually does

Epsh is not generally a fork-and-exec implementation for external commands.
Its normal external-command behavior depends on whether an external handler
has been installed:

 - with an external handler, epsh expands the command and delegates process
   creation to the handler;
 - without a handler, epsh uses `std::process::Command` for ordinary external
   commands and has a direct `execvp` fast path in an already-forked child;
 - the `exec` builtin directly replaces the current process with `execvp`.

Epsh does, however, use fork to create shell execution contexts. After one of
those forks, a child can:

 - evaluate a complete pipeline stage;
 - run a builtin in a pipeline;
 - execute a subshell or background command;
 - evaluate command substitutions;
 - allocate Rust `Vec`, `String`, and AST/expansion state;
 - manipulate `std::env` and the working directory;
 - run traps and other shell bookkeeping;
 - invoke an external handler, which may construct `std::process::Command`.

Those operations do not satisfy rustix's raw-fork child contract. Replacing
`sys::fork` with `kernel_fork` would therefore be an unsafe semantic change,
not merely a libc-to-rustix wrapper migration.

The distinction is especially important for ish. A normal top-level external
command follows this path:

```text
ish main loop -> epsh external handler -> std::process::Command
```

A pipeline stage follows a different path:

```text
ish main loop -> epsh fork -> epsh child evaluator
                         -> ish external handler or epsh builtin
                         -> external spawn or in-process shell code
```

Thus epsh commonly forks before delegating an external spawn, but it does not
usually perform the final external exec itself in the ish embedding. The
forked child is still running Rust evaluator code, which is the relevant
constraint for `kernel_fork`.

The current libc fork path is more compatible with a normal Unix application
because libc can run its registered `pthread_atfork` handlers and maintain its
own process/runtime invariants. It still cannot make arbitrary Rust execution
after fork universally safe in a multithreaded host, but it is the expected
platform integration boundary for this kind of general evaluator.

### What the ish embedding changes

Ish is currently the only epsh consumer and is deliberately designed around a
single-threaded interactive shell loop. That narrows the practical scope, but
does not make the process literally single-threaded in every state:

 - the file picker starts a background finder thread;
 - dropping a `FinderHandle` sets a stop flag but does not join the worker;
 - denv temporarily starts reader threads, though those are joined before the
   operation returns;
 - ordinary top-level external commands are handled by ish's external handler,
   which uses `std::process::Command` and job-control setup;
 - epsh still creates its own children for pipelines, command substitutions,
   subshells, and other shell semantics.

In particular, an epsh pipeline child can call ish's external handler after
the epsh fork. The handler then constructs and spawns a
`std::process::Command` from that child. That is not the fork-then-raw-exec
model from PR #138. The handler-less epsh path and the `exec` builtin are the
places where epsh itself performs the final exec operation.

The single-consumer fact does make a Linux-specific, deliberately unsafe
contract possible if ish guarantees that no helper threads are alive before
`run_script`, and if epsh documents that contract. It does not remove the
need to address the Rust work performed after fork.

### Viable migration options

1. Keep libc fork for evaluator children. This preserves the current
   cross-platform and embedding behavior. In the ish embedding, continue to
   let the external handler own ordinary external process creation.

2. Add a Linux-only pure-external fast path. Expand and prepare argv,
   environment, redirections, and process-group information in the parent;
   use `kernel_fork` in the child only for raw fd/process setup, raw `execve`,
   and `exit_group`. This requires moving external pipeline-stage ownership
   out of the current post-fork evaluator/handler path. Builtins and evaluator
   children would continue using the existing path.

3. Make all epsh fork children raw-exec-only. This would require a larger
   redesign of pipeline, subshell, builtin, and command-substitution
   execution. It would no longer be a general Rust evaluator in the child.

4. Use `kernel_fork` everywhere under an explicit ish-only safety contract.
   This is technically possible, but it accepts the documented rustix hazards
   for the current evaluator and should be treated as a conscious project
   policy, not as a sound portable migration.

The existence of ish as the only consumer makes option 2 worth considering,
but it is not a direct fork wrapper migration. The immediate safe direction is
option 1. A rustix `kernel_fork` migration should wait until external pipeline
process creation can be prepared in the parent and the child can use only raw
operations. Otherwise it would be an intentional ish-specific acceptance of
the raw-fork hazards described above.
