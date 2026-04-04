use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::io::{FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::ast::*;

/// Builtins safe to run in-process for command substitution.
/// Either pure (no shell state modification) or delegates to pure commands.
/// `command` is included because it dispatches to builtins/externals which
/// work correctly with the output sink capture mechanism.
/// Spawn a thread that relays data from a readable stream to a sink.
fn spawn_relay(
    stream: impl std::io::Read + Send + 'static,
    sink: Arc<Mutex<dyn Write + Send>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let mut reader = std::io::BufReader::new(stream);
        loop {
            match std::io::Read::read(&mut reader, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut w) = sink.lock() {
                        let _ = w.write_all(&buf[..n]);
                    }
                }
            }
        }
    })
}

fn is_pure_builtin(name: &str) -> bool {
    matches!(
        name,
        "echo" | "printf" | "true" | "false" | ":" | "pwd" | "type" | "test" | "[" | "command"
    )
}
use crate::error::{ExitStatus, ShellError, Span};
use crate::expand;
use crate::glob;
use crate::parser::Parser;
use crate::sys;
use crate::var::Variables;

/// Callback for external command execution.
///
/// Receives expanded args (args[0] is the command name) and prefix assignment
/// environment pairs. Redirections are already applied to fds before the
/// handler is called. Return the exit status of the command.
pub type ExternalHandler =
    Box<dyn FnMut(&[String], &[(String, String)]) -> crate::error::Result<ExitStatus> + Send>;

/// A POSIX shell interpreter instance.
///
/// Each `Shell` maintains its own variable scope, working directory, function
/// definitions, and options — safe for concurrent use across threads (one Shell
/// per thread; Shell itself is not Sync).
///
/// # Quick start
/// ```no_run
/// use epsh::eval::Shell;
/// let mut shell = Shell::new();
/// let exit_code = shell.run_script("echo hello");
/// ```
///
/// # Builder pattern
/// ```no_run
/// use epsh::eval::Shell;
/// use std::path::PathBuf;
/// let mut shell = Shell::builder()
///     .cwd(PathBuf::from("/project"))
///     .errexit(true)
///     .build();
/// ```
pub struct Shell {
    pub(crate) vars: Variables,
    /// Defined functions: name → AST body
    pub(crate) functions: HashMap<String, Command>,
    /// Last command's exit status ($?)
    pub(crate) exit_status: ExitStatus,
    /// Shell's PID ($$)
    pub(crate) pid: u32,
    /// Current working directory — per-Shell, not process-global.
    /// All relative paths (redirections, glob, source) resolve against this.
    pub(crate) cwd: PathBuf,
    /// Number of nested loops (for break/continue counting)
    pub(crate) loop_depth: usize,
    /// Shell options
    pub(crate) opts: ShellOpts,
    /// Trap handlers: signal name → command string
    pub(crate) traps: HashMap<String, String>,
    /// True when evaluating a condition (if test, while cond, && / || operands).
    /// Suppresses set -e (errexit). Mirrors dash's EV_TESTED flag.
    pub(crate) tested: bool,
    /// True when executing inside a forked child (pipeline stage, command subst).
    /// Subshells skip the extra fork to avoid pipe fd leaks.
    pub(crate) in_forked_child: bool,
    /// True when the current eval context will exit after the command completes.
    /// Allows eval_external to exec directly instead of fork+exec (dash's EV_EXIT).
    pub(crate) ev_exit: bool,
    /// Optional cancellation flag. When set to true, the shell aborts execution.
    cancel: Option<Arc<AtomicBool>>,
    /// Optional stdout sink. When set, builtin output goes here instead of fd 1.
    pub(crate) stdout_sink: Option<Arc<Mutex<dyn Write + Send>>>,
    /// Optional stderr sink. When set, error output goes here instead of fd 2.
    pub(crate) stderr_sink: Option<Arc<Mutex<dyn Write + Send>>>,
    /// PIDs of currently running child processes (for cancellation cleanup).
    pub(crate) child_pids: Vec<i32>,
    /// Optional execution deadline. When exceeded, shell aborts with TimedOut.
    timeout: Option<std::time::Instant>,
    /// Optional callback that replaces eval_external for spawning processes.
    /// Lets embedders control process creation (job control, sandboxing, etc.).
    external_handler: Option<ExternalHandler>,
    /// PID of the last background command ($!).
    last_bg_pid: Option<u32>,
}

/// Shell option flags (`set -e`, `set -u`, etc.).
#[derive(Debug, Default)]
pub struct ShellOpts {
    /// -e: exit on error
    pub errexit: bool,
    /// -u: treat unset variables as error
    pub nounset: bool,
    /// -x: print commands before execution (xtrace)
    pub xtrace: bool,
    /// -o pipefail: return highest nonzero status from any pipeline stage
    pub pipefail: bool,
    /// Interactive mode: tcsetpgrp for terminal control, WUNTRACED for job control.
    pub interactive: bool,
}

impl ShellOpts {
    /// Return the POSIX `$-` flags string (e.g. "eux").
    pub fn flags_string(&self) -> String {
        let mut s = String::new();
        if self.errexit {
            s.push('e');
        }
        if self.nounset {
            s.push('u');
        }
        if self.xtrace {
            s.push('x');
        }
        if self.interactive {
            s.push('i');
        }
        s
    }
}

impl expand::ShellExpand for Shell {
    fn vars(&self) -> &Variables {
        &self.vars
    }
    fn vars_mut(&mut self) -> &mut Variables {
        &mut self.vars
    }
    fn exit_status(&self) -> ExitStatus {
        self.exit_status
    }
    fn pid(&self) -> u32 {
        self.pid
    }
    fn cwd(&self) -> &Path {
        &self.cwd
    }
    fn shell_flags(&self) -> String {
        self.opts.flags_string()
    }
    fn last_bg_pid(&self) -> Option<u32> {
        self.last_bg_pid
    }
    fn nounset(&self) -> bool {
        self.opts.nounset
    }
    fn command_subst(&mut self, cmd: &Command) -> crate::error::Result<String> {
        self.command_subst(cmd)
    }
}

/// Builder for configuring a [`Shell`] instance.
///
/// Use [`Shell::builder()`] to start, then chain options, then `.build()`.
/// All settings have sensible defaults — you only need to set what you want.
pub struct ShellBuilder {
    cwd: Option<PathBuf>,
    errexit: bool,
    nounset: bool,
    xtrace: bool,
    pipefail: bool,
    interactive: bool,
    cancel: Option<Arc<AtomicBool>>,
    stdout_sink: Option<Arc<Mutex<dyn Write + Send>>>,
    stderr_sink: Option<Arc<Mutex<dyn Write + Send>>>,
    timeout: Option<std::time::Duration>,
    env_clear: bool,
    external_handler: Option<ExternalHandler>,
}

impl Default for ShellBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellBuilder {
    pub fn new() -> Self {
        ShellBuilder {
            cwd: None,
            errexit: false,
            nounset: false,
            xtrace: false,
            pipefail: false,
            interactive: false,
            cancel: None,
            stdout_sink: None,
            stderr_sink: None,
            timeout: None,
            env_clear: false,
            external_handler: None,
        }
    }

    pub fn cwd(mut self, path: PathBuf) -> Self {
        self.cwd = Some(path);
        self
    }
    pub fn errexit(mut self, v: bool) -> Self {
        self.errexit = v;
        self
    }
    pub fn nounset(mut self, v: bool) -> Self {
        self.nounset = v;
        self
    }
    pub fn xtrace(mut self, v: bool) -> Self {
        self.xtrace = v;
        self
    }
    pub fn pipefail(mut self, v: bool) -> Self {
        self.pipefail = v;
        self
    }
    /// Enable interactive mode (tcsetpgrp, WUNTRACED for job control).
    pub fn interactive(mut self, v: bool) -> Self {
        self.interactive = v;
        self
    }
    pub fn cancel_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.cancel = Some(flag);
        self
    }
    pub fn stdout_sink(mut self, sink: Arc<Mutex<dyn Write + Send>>) -> Self {
        self.stdout_sink = Some(sink);
        self
    }
    pub fn stderr_sink(mut self, sink: Arc<Mutex<dyn Write + Send>>) -> Self {
        self.stderr_sink = Some(sink);
        self
    }
    pub fn timeout(mut self, duration: std::time::Duration) -> Self {
        self.timeout = Some(duration);
        self
    }
    /// Don't inherit process environment variables.
    pub fn env_clear(mut self) -> Self {
        self.env_clear = true;
        self
    }
    /// Set a callback for external command execution.
    pub fn external_handler(mut self, handler: ExternalHandler) -> Self {
        self.external_handler = Some(handler);
        self
    }

    pub fn build(self) -> Shell {
        let vars = if self.env_clear {
            Variables::new_clean()
        } else {
            Variables::new()
        };
        let cwd = self
            .cwd
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
        Shell {
            vars,
            functions: HashMap::new(),
            exit_status: ExitStatus::SUCCESS,
            pid: std::process::id(),
            cwd,
            loop_depth: 0,
            opts: ShellOpts {
                errexit: self.errexit,
                nounset: self.nounset,
                xtrace: self.xtrace,
                pipefail: self.pipefail,
                interactive: self.interactive,
            },
            traps: HashMap::new(),
            tested: false,
            in_forked_child: false,
            ev_exit: false,
            cancel: self.cancel,
            stdout_sink: self.stdout_sink,
            stderr_sink: self.stderr_sink,
            child_pids: Vec::new(),
            timeout: self.timeout.map(|d| std::time::Instant::now() + d),
            external_handler: self.external_handler,
            last_bg_pid: None,
        }
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

impl Shell {
    /// Create a [`ShellBuilder`] for configuring a new shell instance.
    pub fn builder() -> ShellBuilder {
        ShellBuilder::new()
    }

    /// Create a new shell with default settings and inherited environment.
    pub fn new() -> Self {
        Shell {
            vars: Variables::new(),
            functions: HashMap::new(),
            exit_status: ExitStatus::SUCCESS,
            pid: std::process::id(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            loop_depth: 0,
            opts: ShellOpts::default(),
            traps: HashMap::new(),
            tested: false,
            in_forked_child: false,
            ev_exit: false,
            cancel: None,
            stdout_sink: None,
            stderr_sink: None,
            child_pids: Vec::new(),
            timeout: None,
            external_handler: None,
            last_bg_pid: None,
        }
    }

    /// Set a cancellation flag. When the flag is set to true, the shell
    /// will abort execution at the next check point with `ShellError::Cancelled`.
    pub fn set_cancel_flag(&mut self, flag: Arc<AtomicBool>) {
        self.cancel = Some(flag);
    }

    /// Set an execution timeout. The shell will abort with `ShellError::TimedOut`
    /// when the deadline is exceeded, checked at the same points as cancellation.
    pub fn set_timeout(&mut self, duration: std::time::Duration) {
        self.timeout = Some(std::time::Instant::now() + duration);
    }

    /// Variable storage.
    pub fn vars(&self) -> &Variables {
        &self.vars
    }
    /// Mutable variable storage.
    pub fn vars_mut(&mut self) -> &mut Variables {
        &mut self.vars
    }
    /// Defined functions.
    pub fn functions(&self) -> &HashMap<String, Command> {
        &self.functions
    }
    /// Last command's exit status (`$?`).
    pub fn exit_status(&self) -> ExitStatus {
        self.exit_status
    }
    /// Shell's PID (`$$`).
    pub fn pid(&self) -> u32 {
        self.pid
    }
    /// Current working directory.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    /// Shell options.
    pub fn opts(&self) -> &ShellOpts {
        &self.opts
    }
    /// Mutable shell options.
    pub fn opts_mut(&mut self) -> &mut ShellOpts {
        &mut self.opts
    }
    /// Trap handlers.
    pub fn traps(&self) -> &HashMap<String, String> {
        &self.traps
    }

    /// Check cancellation flag and timeout deadline.
    fn check_cancel(&self) -> crate::error::Result<()> {
        if let Some(ref flag) = self.cancel
            && flag.load(Ordering::Relaxed)
        {
            return Err(ShellError::Cancelled);
        }
        if let Some(deadline) = self.timeout
            && std::time::Instant::now() >= deadline
        {
            return Err(ShellError::TimedOut);
        }
        Ok(())
    }

    /// Set a stdout sink. Builtin output will be written here instead of fd 1.
    pub fn set_stdout_sink(&mut self, sink: Arc<Mutex<dyn Write + Send>>) {
        self.stdout_sink = Some(sink);
    }

    /// Set a stderr sink. Error output will be written here instead of fd 2.
    pub fn set_stderr_sink(&mut self, sink: Arc<Mutex<dyn Write + Send>>) {
        self.stderr_sink = Some(sink);
    }

    /// Write a string to stdout (sink or fd 1).
    /// Decodes PUA-encoded bytes back to raw bytes for non-UTF-8 preservation.
    pub(crate) fn write_out(&self, s: &str) {
        let data = crate::encoding::str_to_bytes(s);
        if let Some(ref sink) = self.stdout_sink {
            if let Ok(mut w) = sink.lock() {
                let _ = w.write_all(&data);
            }
        } else {
            // SAFETY: fd 1 (stdout) is always valid; data pointer and length are from a live Vec.
            unsafe {
                sys::write(1, data.as_ptr() as *const _, data.len());
            }
        }
    }

    /// Write a string to stderr (sink or fd 2).
    pub(crate) fn write_err(&self, s: &str) {
        let data = crate::encoding::str_to_bytes(s);
        if let Some(ref sink) = self.stderr_sink {
            if let Ok(mut w) = sink.lock() {
                let _ = w.write_all(&data);
            }
        } else {
            // SAFETY: fd 2 (stderr) is always valid; data pointer and length are from a live Vec.
            unsafe {
                sys::write(2, data.as_ptr() as *const _, data.len());
            }
        }
    }

    /// Write a formatted error message to stderr.
    pub(crate) fn err_msg(&self, msg: &str) {
        self.write_err(msg);
        if !msg.ends_with('\n') {
            self.write_err("\n");
        }
    }

    /// Wait for a child process, polling the cancel flag.
    /// On cancellation, kills the child's process group and returns Cancelled.
    /// In interactive mode, uses WUNTRACED to detect stopped processes.
    /// `pgid` is the process group (for Stopped reporting); 0 means use pid.
    fn wait_child(&mut self, pid: i32) -> crate::error::Result<ExitStatus> {
        self.wait_child_pgid(pid, pid)
    }

    fn wait_child_pgid(&mut self, pid: i32, pgid: i32) -> crate::error::Result<ExitStatus> {
        let wuntraced = if self.opts.interactive {
            libc::WUNTRACED
        } else {
            0
        };

        // If no cancel flag or timeout, just block
        if self.cancel.is_none() && self.timeout.is_none() {
            let mut status = 0i32;
            // SAFETY: pid is a valid child PID obtained from fork().
            unsafe {
                sys::waitpid(pid, &mut status, wuntraced);
            }
            if libc::WIFSTOPPED(status) {
                // Don't remove from child_pids — process is still alive
                return Err(ShellError::Stopped { pid, pgid });
            }
            self.child_pids.retain(|&p| p != pid);
            return Ok(ExitStatus::from_wait(status));
        }

        // Spawn a thread to do blocking waitpid; check cancel while waiting.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut status = 0i32;
            // SAFETY: pid is a valid child PID obtained from fork().
            unsafe {
                sys::waitpid(pid, &mut status, wuntraced);
            }
            let _ = tx.send(status);
        });
        loop {
            // Check for result with a short timeout to allow cancel checks
            match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                Ok(status) => {
                    if libc::WIFSTOPPED(status) {
                        return Err(ShellError::Stopped { pid, pgid });
                    }
                    self.child_pids.retain(|&p| p != pid);
                    return Ok(ExitStatus::from_wait(status));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    self.child_pids.retain(|&p| p != pid);
                    return Ok(ExitStatus::FAILURE);
                }
            }
            if let Err(e) = self.check_cancel() {
                // Cancel or timeout — kill and reap
                // SAFETY: pid is a valid child PID; negated for process group kill. SIGKILL is always valid.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
                // The thread will complete waitpid and send the status; just drain it
                let _ = rx.recv();
                self.child_pids.retain(|&p| p != pid);
                return Err(e);
            }
        }
    }

    /// Kill all tracked child processes (process groups).
    fn kill_children(&mut self) {
        for &pid in &self.child_pids {
            // Kill the process group (negative PID)
            // SAFETY: pid is a valid child PID from fork(); negated for process group kill.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        // Reap them
        for &pid in &self.child_pids {
            let mut status = 0i32;
            // SAFETY: pid is a valid child PID; reaping after kill.
            unsafe {
                sys::waitpid(pid, &mut status, 0);
            }
        }
        self.child_pids.clear();
    }

    /// Build the xtrace prefix string ($PS4, default "+ ").
    fn xtrace_prefix(&self) -> String {
        self.vars.get("PS4").unwrap_or("+ ").to_string()
    }

    /// Set a callback that replaces the default external command execution.
    ///
    /// When set, the handler is called instead of `eval_external` for commands
    /// that are not builtins or functions. Redirections are already applied to
    /// fds before the handler runs. The handler receives:
    /// - `args`: expanded arguments (args[0] is the command name)
    /// - `env`: prefix assignment pairs (`FOO=bar cmd` → `[("FOO", "bar")]`)
    pub fn set_external_handler(&mut self, handler: ExternalHandler) {
        self.external_handler = Some(handler);
    }

    /// Set the shell's working directory.
    pub fn set_cwd(&mut self, dir: PathBuf) {
        self.cwd = dir;
    }

    /// Resolve a path against the shell's working directory.
    /// Absolute paths are returned as-is; relative paths are joined with `self.cwd`.
    pub fn resolve_path(&self, p: &str) -> PathBuf {
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }

    /// Execute a shell script from source text.
    pub fn run_script(&mut self, source: &str) -> i32 {
        let mut parser = Parser::new(source);
        let program = match parser.parse() {
            Ok(p) => p,
            Err(e) => {
                self.err_msg(&format!("epsh: {e}"));
                return 2;
            }
        };

        self.run_program(&program).code()
    }

    /// Execute a pre-parsed program. Returns the final exit status.
    /// This is the library-facing entry point — parse once with `Parser`,
    /// then execute via `run_program`.
    pub fn run_program(&mut self, program: &crate::ast::Program) -> ExitStatus {
        for cmd in &program.commands {
            match self.eval_command(cmd) {
                Ok(status) => {
                    self.exit_status = status;
                }
                Err(ShellError::Exit(n)) => {
                    self.run_exit_trap();
                    return n;
                }
                Err(ShellError::Cancelled) | Err(ShellError::TimedOut) => {
                    self.kill_children();
                    return ExitStatus::from_signal(2); // 130 = SIGINT
                }
                Err(e) => {
                    self.err_msg(&format!("epsh: {e}"));
                    self.exit_status = ExitStatus::MISUSE;
                    self.run_exit_trap();
                    return self.exit_status;
                }
            }
            self.run_pending_traps();
        }

        self.run_exit_trap();
        self.exit_status
    }

    /// Check for pending signals and run their trap handlers.
    fn run_pending_traps(&mut self) {
        for sig_name in crate::signal::take_pending() {
            if let Some(action) = self.traps.get(sig_name) {
                // Empty action means ignore (trap '' SIG)
                if action.is_empty() {
                    continue;
                }
                let action = action.clone();
                let mut parser = Parser::new(&action);
                if let Ok(program) = parser.parse() {
                    for cmd in &program.commands {
                        let _ = self.eval_command(cmd);
                    }
                }
            }
        }
    }

    /// Run the EXIT trap if one is set.
    pub(crate) fn run_exit_trap(&mut self) {
        // Remove the trap to prevent recursive invocation
        if let Some(action) = self.traps.remove("EXIT") {
            let mut parser = Parser::new(&action);
            if let Ok(program) = parser.parse() {
                for cmd in &program.commands {
                    let _ = self.eval_command(cmd);
                }
            }
        }
    }

    /// Set shell arguments ($0, $1, $2, ...).
    pub fn set_args(&mut self, args: &[&str]) {
        if let Some(first) = args.first() {
            self.vars.arg0 = first.to_string();
        }
        self.vars.positional = args.iter().skip(1).map(|s| s.to_string()).collect();
    }

    /// Set a variable. Returns error if the variable is readonly.
    pub fn set_var(&mut self, name: &str, value: &str) -> Result<(), String> {
        self.vars.set(name, value)
    }

    /// Get a variable's value.
    pub fn get_var(&self, name: &str) -> Option<&str> {
        self.vars.get(name)
    }

    /// Expand a word to a list of fields (with field splitting and globbing).
    pub(crate) fn expand_fields(&mut self, word: &Word) -> crate::error::Result<Vec<String>> {
        expand::expand_word_to_fields(word, self)
    }

    /// Expand a word to a single string (no field splitting or globbing).
    pub(crate) fn expand_string(&mut self, word: &Word) -> crate::error::Result<String> {
        expand::expand_word_to_string(word, self)
    }

    /// Expand word parts into a fnmatch-ready pattern string.
    /// Glob chars from quoted regions are escaped.
    pub(crate) fn expand_pattern(&mut self, word: &Word) -> crate::error::Result<String> {
        expand::expand_pattern(&word.parts, self)
    }

    /// Evaluate a command node, returning its exit status.
    pub fn eval_command(&mut self, cmd: &Command) -> crate::error::Result<ExitStatus> {
        self.check_cancel()?;
        let status = self.eval_command_inner(cmd)?;
        self.exit_status = status;

        // Errexit check — mirrors dash's eval.c line 330.
        // Only fires from "leaf" nodes (simple commands, pipelines, subshells)
        // where checkexit = EV_TESTED. Compound commands (if, while, for, etc.)
        // propagate status but don't trigger errexit directly.
        // Bang (!) pipelines suppress errexit on the inverted result (POSIX).
        let is_bang = matches!(cmd, Command::Pipeline { bang: true, .. });
        let is_leaf = matches!(
            cmd,
            Command::Simple { .. }
                | Command::Pipeline { .. }
                | Command::Subshell { .. }
                | Command::Background { .. }
        );
        if self.opts.errexit && is_leaf && !self.tested && !is_bang && !status.success() {
            return Err(ShellError::Exit(status));
        }

        Ok(status)
    }

    fn eval_command_inner(&mut self, cmd: &Command) -> crate::error::Result<ExitStatus> {
        // ev_exit only applies to the final simple command in an exit context.
        // Clear it for compound commands so nested commands don't exec-direct.
        if !matches!(cmd, Command::Simple { .. }) {
            self.ev_exit = false;
        }
        match cmd {
            Command::Simple {
                assigns,
                args,
                redirs,
                span,
            } => self.eval_simple(assigns, args, redirs, *span),

            Command::Pipeline {
                commands,
                bang,
                span: _,
            } => {
                // bang (!) suppresses errexit for both the inner pipeline
                // AND the inverted result (POSIX requirement).
                let saved = self.tested;
                if *bang {
                    self.tested = true;
                }
                let status = self.eval_pipeline(commands)?;
                self.tested = saved;
                Ok(if *bang { status.inverted() } else { status })
            }

            Command::And(left, right) => {
                // Left side of && is always tested (errexit suppressed)
                let saved = self.tested;
                self.tested = true;
                let status = self.eval_command(left)?;
                self.tested = saved;
                self.exit_status = status;
                if status.success() {
                    // Right side inherits parent's tested flag (like dash)
                    self.eval_command(right)
                } else {
                    Ok(status)
                }
            }

            Command::Or(left, right) => {
                let saved = self.tested;
                self.tested = true;
                let status = self.eval_command(left)?;
                self.tested = saved;
                self.exit_status = status;
                if !status.success() {
                    self.eval_command(right)
                } else {
                    Ok(status)
                }
            }

            Command::Sequence(left, right) => {
                let status = self.eval_command(left)?;
                self.exit_status = status;
                self.eval_command(right)
            }

            Command::Subshell {
                body,
                redirs,
                span: _,
            } => {
                if self.in_forked_child {
                    // Already in a forked child (pipeline stage, command subst) —
                    // execute directly without double-forking. Mirrors dash's
                    // EV_EXIT optimization. Prevents pipe fd leaks.
                    let saved = self.setup_redirections(redirs)?;
                    let result = self.eval_command(body);
                    self.restore_redirections(saved);
                    result
                } else {
                    self.eval_in_subshell(body, redirs)
                }
            }

            Command::BraceGroup {
                body,
                redirs,
                span: _,
            } => {
                let saved = self.setup_redirections(redirs)?;
                let result = self.eval_command(body);
                self.restore_redirections(saved);
                result
            }

            Command::If {
                cond,
                then_part,
                else_part,
                ..
            } => {
                let saved = self.tested;
                self.tested = true;
                let cond_status = self.eval_command(cond)?;
                self.tested = saved;
                self.exit_status = cond_status;
                if cond_status.success() {
                    self.eval_command(then_part)
                } else if let Some(else_part) = else_part {
                    self.eval_command(else_part)
                } else {
                    Ok(ExitStatus::SUCCESS)
                }
            }

            Command::While { cond, body, .. } => {
                self.loop_depth += 1;
                let mut last_status = ExitStatus::SUCCESS;
                loop {
                    self.check_cancel()?;
                    let saved = self.tested;
                    self.tested = true;
                    let cond_status = self.eval_command(cond)?;
                    self.tested = saved;
                    self.exit_status = cond_status;
                    if !cond_status.success() {
                        break;
                    }
                    match self.eval_command(body) {
                        Ok(s) => last_status = s,
                        Err(ShellError::Break(1)) => break,
                        Err(ShellError::Break(n)) if n > 1 => {
                            self.loop_depth -= 1;
                            return Err(ShellError::Break(n - 1));
                        }
                        Err(ShellError::Continue(1)) => continue,
                        Err(ShellError::Continue(n)) if n > 1 => {
                            self.loop_depth -= 1;
                            return Err(ShellError::Continue(n - 1));
                        }
                        Err(e) => {
                            self.loop_depth -= 1;
                            return Err(e);
                        }
                    }
                }
                self.loop_depth -= 1;
                Ok(last_status)
            }

            Command::Until { cond, body, .. } => {
                self.loop_depth += 1;
                let mut last_status = ExitStatus::SUCCESS;
                loop {
                    self.check_cancel()?;
                    let saved = self.tested;
                    self.tested = true;
                    let cond_status = self.eval_command(cond)?;
                    self.tested = saved;
                    self.exit_status = cond_status;
                    if cond_status.success() {
                        break;
                    }
                    match self.eval_command(body) {
                        Ok(s) => last_status = s,
                        Err(ShellError::Break(1)) => break,
                        Err(ShellError::Break(n)) if n > 1 => {
                            self.loop_depth -= 1;
                            return Err(ShellError::Break(n - 1));
                        }
                        Err(ShellError::Continue(1)) => continue,
                        Err(ShellError::Continue(n)) if n > 1 => {
                            self.loop_depth -= 1;
                            return Err(ShellError::Continue(n - 1));
                        }
                        Err(e) => {
                            self.loop_depth -= 1;
                            return Err(e);
                        }
                    }
                }
                self.loop_depth -= 1;
                Ok(last_status)
            }

            Command::For {
                var, words, body, ..
            } => {
                let word_list = if let Some(words) = words {
                    let mut expanded = Vec::new();
                    for w in words {
                        expanded.extend(self.expand_fields(w)?);
                    }
                    expanded
                } else {
                    // No 'in' clause: use positional parameters
                    self.vars.positional.clone()
                };

                self.loop_depth += 1;
                let mut last_status = ExitStatus::SUCCESS;
                for value in &word_list {
                    self.check_cancel()?;
                    if let Err(e) = self.vars.set(var, value) {
                        self.err_msg(&e);
                        return Ok(ExitStatus::FAILURE);
                    }
                    match self.eval_command(body) {
                        Ok(s) => last_status = s,
                        Err(ShellError::Break(1)) => break,
                        Err(ShellError::Break(n)) if n > 1 => {
                            self.loop_depth -= 1;
                            return Err(ShellError::Break(n - 1));
                        }
                        Err(ShellError::Continue(1)) => continue,
                        Err(ShellError::Continue(n)) if n > 1 => {
                            self.loop_depth -= 1;
                            return Err(ShellError::Continue(n - 1));
                        }
                        Err(e) => {
                            self.loop_depth -= 1;
                            return Err(e);
                        }
                    }
                }
                self.loop_depth -= 1;
                Ok(last_status)
            }

            Command::Case { word, arms, .. } => {
                let expanded = expand::remove_glob_escapes(&self.expand_string(word)?);

                for arm in arms {
                    for pattern in &arm.patterns {
                        let pat = self.expand_pattern(pattern)?;
                        if glob::fnmatch(&pat, &expanded) {
                            if let Some(ref body) = arm.body {
                                return self.eval_command(body);
                            } else {
                                return Ok(ExitStatus::SUCCESS);
                            }
                        }
                    }
                }
                Ok(ExitStatus::SUCCESS)
            }

            Command::FuncDef { name, body, .. } => {
                self.functions.insert(name.clone(), *body.clone());
                Ok(ExitStatus::SUCCESS)
            }

            Command::Not(cmd) => {
                let saved = self.tested;
                self.tested = true;
                let status = self.eval_command(cmd)?;
                self.tested = saved;
                Ok(status.inverted())
            }

            Command::Background { cmd, redirs } => {
                // Fork and run in background
                self.eval_background(cmd, redirs)
            }
        }
    }

    /// Evaluate a simple command (the most common case).
    fn eval_simple(
        &mut self,
        assigns: &[Assignment],
        args: &[Word],
        redirs: &[Redir],
        span: Span,
    ) -> crate::error::Result<ExitStatus> {
        // Expand arguments
        let mut expanded_args: Vec<String> = Vec::new();
        for arg in args {
            expanded_args.extend(self.expand_fields(arg)?);
        }

        // No command name: just apply assignments to the shell environment.
        // Return current exit_status (may have been set by command substitutions
        // during argument expansion — e.g., `$(false)` with empty result).
        if expanded_args.is_empty() {
            // xtrace for bare assignments
            if self.opts.xtrace && !assigns.is_empty() {
                let mut trace = self.xtrace_prefix();
                for assign in assigns {
                    let value = self.expand_string(&assign.value)?;
                    if !trace.ends_with(' ') {
                        trace.push(' ');
                    }
                    trace.push_str(&assign.name);
                    trace.push('=');
                    trace.push_str(&value);
                }
                trace.push('\n');
                self.write_err(&trace);
            }
            for assign in assigns {
                let value = self.expand_string(&assign.value)?;
                self.vars
                    .set(&assign.name, &value)
                    .map_err(|msg| ShellError::Runtime { msg, span })?;
            }
            return Ok(self.exit_status);
        }

        // xtrace: print $PS4, assignments, then args (like dash's evalcommand)
        if self.opts.xtrace {
            let mut trace = self.xtrace_prefix();
            for assign in assigns {
                if let Ok(value) = self.expand_string(&assign.value) {
                    if !trace.ends_with(' ') {
                        trace.push(' ');
                    }
                    trace.push_str(&assign.name);
                    trace.push('=');
                    trace.push_str(&value);
                }
            }
            for arg in &expanded_args {
                if !trace.ends_with(' ') {
                    trace.push(' ');
                }
                trace.push_str(arg);
            }
            trace.push('\n');
            self.write_err(&trace);
        }

        let cmd_name = &expanded_args[0];

        // exec is special: its redirections are permanent (no save/restore)
        let is_exec = cmd_name == "exec";

        // Setup redirections before executing (applies to builtins, functions, externals)
        let saved_fds = self.setup_redirections(redirs)?;

        let result =
            if let Some(status) = self.try_builtin(cmd_name, &expanded_args, assigns, &[], span)? {
                Ok(status)
            } else if let Some(func_body) = self.functions.get(cmd_name).cloned() {
                self.ev_exit = false; // function body may have multiple commands
                self.eval_function(&func_body, &expanded_args, assigns, &[], span)
            } else if self.external_handler.is_some() {
                // Build prefix assignment env pairs for the handler
                let mut env_pairs = Vec::new();
                for assign in assigns {
                    let value = self.expand_string(&assign.value)?;
                    env_pairs.push((assign.name.clone(), value));
                }
                let handler = self.external_handler.as_mut().unwrap();
                handler(&expanded_args, &env_pairs)
            } else {
                self.eval_external(&expanded_args, assigns, &[], span)
            };

        if is_exec {
            // exec redirections are permanent — close saved copies instead of restoring
            for s in saved_fds {
                if let Some(copy) = s.saved_copy {
                    // SAFETY: copy is a valid fd from fcntl_dupfd_cloexec.
                    unsafe {
                        sys::close(copy);
                    }
                }
            }
        } else {
            self.restore_redirections(saved_fds);
        }
        result
    }

    /// Evaluate a function call.
    fn eval_function(
        &mut self,
        body: &Command,
        args: &[String],
        assigns: &[Assignment],
        redirs: &[Redir],
        _span: Span,
    ) -> crate::error::Result<ExitStatus> {
        // Save and set positional parameters (take avoids clone)
        let saved_positional = std::mem::replace(&mut self.vars.positional, args[1..].to_vec());

        // Push scope for local variables
        self.vars.push_scope();

        // Apply temporary assignments
        for assign in assigns {
            let value = self.expand_string(&assign.value)?;
            self.vars.make_local(&assign.name);
            let _ = self.vars.set(&assign.name, &value);
        }

        // Setup redirections
        let saved_fds = self.setup_redirections(redirs)?;

        // Execute function body
        let result = match self.eval_command(body) {
            Ok(s) => Ok(s),
            Err(ShellError::Return(n)) => Ok(n),
            Err(e) => Err(e),
        };

        // Restore state
        self.restore_redirections(saved_fds);
        self.vars.pop_scope();
        self.vars.positional = saved_positional;

        result
    }

    /// Handle exec/spawn failure — report error and return appropriate status.
    fn handle_exec_error(
        &self,
        e: &std::io::Error,
        cmd_name: &str,
    ) -> crate::error::Result<ExitStatus> {
        if e.kind() == std::io::ErrorKind::NotFound {
            self.err_msg(&format!("{cmd_name}: not found"));
            Ok(ExitStatus::NOT_FOUND)
        } else if e.kind() == std::io::ErrorKind::PermissionDenied {
            self.err_msg(&format!("{cmd_name}: permission denied"));
            Ok(ExitStatus::NOT_EXECUTABLE)
        } else {
            Err(ShellError::Io(std::io::Error::new(e.kind(), e.to_string())))
        }
    }

    /// Execute an external command.
    pub(crate) fn eval_external(
        &mut self,
        args: &[String],
        assigns: &[Assignment],
        redirs: &[Redir],
        _span: Span,
    ) -> crate::error::Result<ExitStatus> {
        self.check_cancel()?;
        let saved = self.setup_redirections(redirs)?;

        // When ev_exit is set (pipeline child about to _exit), exec directly
        // instead of fork+exec. Mirrors dash's shellexec fast-path.
        if self.ev_exit {
            // SAFETY: ev_exit is only set in forked children (pipeline/subshell),
            // which are single-threaded. set_var/set_current_dir are safe here.
            unsafe {
                for (k, v) in self.vars.exported_env() {
                    std::env::set_var(&k, &v);
                }
                for assign in assigns {
                    if let Ok(value) = self.expand_string(&assign.value) {
                        std::env::set_var(&assign.name, &value);
                    }
                }
                let _ = std::env::set_current_dir(&self.cwd);
            }
            let err = crate::builtins::exec::execvp(&args[0], args);
            self.restore_redirections(saved);
            return self.handle_exec_error(&err, &args[0]);
        }

        let mut cmd = std::process::Command::new(&args[0]);
        cmd.args(&args[1..]);
        cmd.current_dir(&self.cwd);
        // NOTE: no pre_exec here — allows Rust to use posix_spawn (1.4x faster).
        // Process group isolation is done from parent after spawn via setpgid.

        // Set environment: exported vars + temporary assignments
        cmd.env_clear();
        for (k, v) in self.vars.exported_env() {
            cmd.env(&k, &v);
        }
        for assign in assigns {
            let value = self.expand_string(&assign.value)?;
            cmd.env(&assign.name, &value);
        }

        // When sinks are set, capture child stdout/stderr and relay to sinks.
        if self.stdout_sink.is_some() {
            cmd.stdout(std::process::Stdio::piped());
        }
        if self.stderr_sink.is_some() {
            cmd.stderr(std::process::Stdio::piped());
        }

        let result = match cmd.spawn() {
            Ok(mut child) => {
                let child_id = child.id() as i32;
                // Isolate in own process group from parent (allows posix_spawn path)
                // SAFETY: child_id is a valid PID just returned by spawn().
                unsafe {
                    libc::setpgid(child_id, child_id);
                }
                self.child_pids.push(child_id);

                // Spawn relay threads for sinks, keeping handles to join later
                let mut handles = Vec::new();
                if let Some(stdout) = child.stdout.take() {
                    handles.push(spawn_relay(stdout, self.stdout_sink.clone().unwrap()));
                }
                if let Some(stderr) = child.stderr.take() {
                    handles.push(spawn_relay(stderr, self.stderr_sink.clone().unwrap()));
                }

                // Cancel-aware wait: spawn thread for blocking wait, check cancel in loop
                let wait_result = if self.cancel.is_some() || self.timeout.is_some() {
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        let _ = tx.send(child.wait());
                    });
                    loop {
                        match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                            Ok(result) => break result,
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                                break Err(std::io::Error::other("wait thread panicked"));
                            }
                        }
                        if let Err(e) = self.check_cancel() {
                            // Kill via PID since child was moved to the thread
                            // SAFETY: child_id is a valid PID from the spawned process.
                            unsafe {
                                libc::kill(child_id, libc::SIGKILL);
                            }
                            let _ = rx.recv(); // wait for the thread to finish
                            self.child_pids.retain(|&p| p != child_id);
                            for h in handles {
                                let _ = h.join();
                            }
                            self.restore_redirections(saved);
                            return Err(e);
                        }
                    }
                } else {
                    child.wait()
                };
                self.child_pids.retain(|&p| p != child_id);

                // Join relay threads to ensure all output is flushed
                for h in handles {
                    let _ = h.join();
                }

                match wait_result {
                    Ok(status) => Ok(ExitStatus::from(status.code().unwrap_or(128))),
                    Err(e) => Err(ShellError::Io(e)),
                }
            }
            Err(e) => self.handle_exec_error(&e, &args[0]),
        };

        self.restore_redirections(saved);
        result
    }

    /// Execute a pipeline.
    fn eval_pipeline(&mut self, commands: &[Command]) -> crate::error::Result<ExitStatus> {
        if commands.len() == 1 {
            return self.eval_command(&commands[0]);
        }

        // Create pipes between stages
        let mut pipes: Vec<(RawFd, RawFd)> = Vec::new();
        for _ in 0..commands.len() - 1 {
            let mut fds = [0i32; 2];
            // SAFETY: fds is a valid 2-element array for pipe() to write into.
            unsafe {
                if sys::pipe(fds.as_mut_ptr()) != 0 {
                    return Err(ShellError::Io(std::io::Error::last_os_error()));
                }
            }
            pipes.push((fds[0], fds[1]));
        }

        let mut children = Vec::new();
        let mut pgid: i32 = 0; // first child's PID becomes the pipeline's PGID

        for (i, cmd) in commands.iter().enumerate() {
            // SAFETY: Standard POSIX fork/dup2/close/setpgid pattern. Pipe fds are
            // valid from pipe() above. Child sets up its stdin/stdout from pipe fds
            // then closes all pipe fds before executing.
            unsafe {
                let pid = sys::fork();
                if pid < 0 {
                    // Fork failed — clean up pipes and kill already-forked children
                    for &(r, w) in &pipes {
                        sys::close(r);
                        sys::close(w);
                    }
                    self.kill_children();
                    return Err(ShellError::Io(std::io::Error::last_os_error()));
                }

                if pid == 0 {
                    // Child: join pipeline process group
                    // First child creates the group (pgid==0 → setpgid(0,0))
                    // Subsequent children join it
                    libc::setpgid(0, pgid);
                    self.in_forked_child = true;
                    // Connect stdin from previous pipe
                    if i > 0 {
                        sys::dup2(pipes[i - 1].0, 0);
                    }
                    // Connect stdout to next pipe
                    if i < commands.len() - 1 {
                        sys::dup2(pipes[i].1, 1);
                    }
                    // Close all pipe fds
                    for &(read_fd, write_fd) in &pipes {
                        sys::close(read_fd);
                        sys::close(write_fd);
                    }

                    // Mark this as exit context — eval_external can exec
                    // directly instead of fork+exec (dash's EV_EXIT).
                    self.ev_exit = true;
                    let status = match self.eval_command(cmd) {
                        Ok(s) => s,
                        Err(ShellError::Exit(n)) => n,
                        Err(_) => ExitStatus::FAILURE,
                    };
                    sys::exit_child(status);
                }

                // Parent: also call setpgid to avoid race with child
                if pgid == 0 {
                    pgid = pid; // first child's PID is the group leader
                }
                libc::setpgid(pid, pgid);
                children.push(pid);
                self.child_pids.push(pid);
            }
        }

        // Parent: close all pipe fds
        for &(read_fd, write_fd) in &pipes {
            // SAFETY: fds are valid from pipe() and not yet closed in parent.
            unsafe {
                sys::close(read_fd);
                sys::close(write_fd);
            }
        }

        // Interactive mode: give the pipeline the terminal
        if self.opts.interactive && pgid > 0 {
            // SAFETY: pgid is a valid process group from setpgid; fd 0 is stdin.
            unsafe {
                libc::tcsetpgrp(0, pgid);
            }
        }

        // Wait for all children, checking cancel between stages
        let mut last_status = ExitStatus::SUCCESS;
        let mut pipefail_status = ExitStatus::SUCCESS;
        for (i, &pid) in children.iter().enumerate() {
            match self.wait_child_pgid(pid, pgid) {
                Ok(stage_status) => {
                    if !stage_status.success() {
                        pipefail_status = stage_status;
                    }
                    if i == children.len() - 1 {
                        last_status = stage_status;
                    }
                }
                Err(ShellError::Stopped { pid: _, pgid: _ }) if self.opts.interactive => {
                    // Reclaim terminal before propagating
                    // SAFETY: getpgrp() and tcsetpgrp are always safe with valid fd.
                    unsafe {
                        libc::tcsetpgrp(0, libc::getpgrp());
                    }
                    return Err(ShellError::Stopped {
                        pid: children[children.len() - 1],
                        pgid,
                    });
                }
                Err(e @ (ShellError::Cancelled | ShellError::TimedOut)) => {
                    if self.opts.interactive {
                        unsafe {
                            libc::tcsetpgrp(0, libc::getpgrp());
                        }
                    }
                    self.kill_children();
                    return Err(e);
                }
                Err(e) => {
                    if self.opts.interactive {
                        unsafe {
                            libc::tcsetpgrp(0, libc::getpgrp());
                        }
                    }
                    return Err(e);
                }
            }
        }

        // Interactive mode: reclaim the terminal
        if self.opts.interactive && pgid > 0 {
            // SAFETY: getpgrp() returns our process group; fd 0 is stdin.
            unsafe {
                libc::tcsetpgrp(0, libc::getpgrp());
            }
        }

        if self.opts.pipefail && !pipefail_status.success() {
            Ok(pipefail_status)
        } else {
            Ok(last_status)
        }
    }

    /// Execute a command in a subshell (fork).
    fn eval_in_subshell(
        &mut self,
        body: &Command,
        redirs: &[Redir],
    ) -> crate::error::Result<ExitStatus> {
        // SAFETY: Standard POSIX fork pattern. Child isolates into its own
        // process group, executes the body, then _exit()s. Parent waits.
        unsafe {
            let pid = sys::fork();
            if pid < 0 {
                return Err(ShellError::Io(std::io::Error::last_os_error()));
            }

            if pid == 0 {
                // Child — isolate in own process group
                libc::setpgid(0, 0);
                let parent_exit_trap = self
                    .traps
                    .remove("EXIT")
                    .or_else(|| self.traps.remove("exit"));
                drop(parent_exit_trap); // parent EXIT trap doesn't run in subshell
                self.in_forked_child = true;

                let _saved = match self.setup_redirections(redirs) {
                    Ok(s) => s,
                    Err(_) => sys::exit_child(ExitStatus::FAILURE),
                };
                let status = match self.eval_command(body) {
                    Ok(s) => s,
                    Err(ShellError::Exit(n)) => n,
                    Err(_) => ExitStatus::FAILURE,
                };
                // Run subshell's own EXIT trap if it set one
                self.run_exit_trap();
                sys::exit_child(status);
            }

            // Parent
            self.child_pids.push(pid);
            self.wait_child(pid)
        }
    }

    /// Execute a command in the background.
    fn eval_background(
        &mut self,
        cmd: &Command,
        redirs: &[Redir],
    ) -> crate::error::Result<ExitStatus> {
        // SAFETY: Standard POSIX fork pattern. Child isolates into its own
        // process group and runs the command. Parent does not wait (background).
        unsafe {
            let pid = sys::fork();
            if pid < 0 {
                return Err(ShellError::Io(std::io::Error::last_os_error()));
            }

            if pid == 0 {
                libc::setpgid(0, 0);
                let _saved = match self.setup_redirections(redirs) {
                    Ok(s) => s,
                    Err(_) => sys::exit_child(ExitStatus::FAILURE),
                };
                let status = match self.eval_command(cmd) {
                    Ok(s) => s,
                    Err(ShellError::Exit(n)) => n,
                    Err(_) => ExitStatus::FAILURE,
                };
                sys::exit_child(status);
            }

            self.child_pids.push(pid);
            self.last_bg_pid = Some(pid as u32);
            // Don't wait — background process
            Ok(ExitStatus::SUCCESS)
        }
    }

    /// Execute a command substitution and return its output.
    pub fn command_subst(&mut self, cmd: &Command) -> crate::error::Result<String> {
        // In-process comsub for pure builtins: capture output without forking.
        // Pure builtins (echo, printf, true, false, :, pwd, type, test/[) don't
        // modify shell state, so running them in-process is safe and avoids the
        // fork+pipe+waitpid overhead entirely.
        if let Command::Simple {
            assigns,
            args,
            redirs,
            span,
        } = cmd
            && assigns.is_empty()
            && redirs.is_empty()
            && !args.is_empty()
        {
            let mut expanded = Vec::new();
            let expand_ok = (|| -> crate::error::Result<()> {
                for a in args {
                    expanded.extend(self.expand_fields(a)?);
                }
                Ok(())
            })();
            if expand_ok.is_ok() && !expanded.is_empty() && is_pure_builtin(&expanded[0]) {
                // Capture output to buffer instead of forking
                let saved_sink = self.stdout_sink.take();
                let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
                self.stdout_sink = Some(buf.clone());
                let status = self.try_builtin(&expanded[0], &expanded, &[], &[], *span);
                self.stdout_sink = saved_sink;
                if let Ok(Some(s)) = status {
                    self.exit_status = s;
                    let captured = buf.lock().unwrap();
                    let mut output = crate::encoding::bytes_to_str(&captured);
                    while output.ends_with('\n') {
                        output.pop();
                    }
                    return Ok(output);
                }
                // Fallback: not a builtin after all, continue to fork path
            }
        }

        // $(<file) optimization: read file directly without forking
        if let Command::Simple {
            assigns,
            args,
            redirs,
            ..
        } = cmd
            && assigns.is_empty()
            && args.is_empty()
            && redirs.len() == 1
            && let RedirKind::Input(ref word) = redirs[0].kind
        {
            let filename = self.expand_string(word)?;
            let filepath = self.resolve_path(&filename);
            match std::fs::read(&filepath) {
                Ok(bytes) => {
                    let mut content = crate::encoding::bytes_to_str(&bytes);
                    // Remove trailing newlines (like command substitution)
                    while content.ends_with('\n') {
                        content.pop();
                    }
                    self.exit_status = ExitStatus::SUCCESS;
                    return Ok(content);
                }
                Err(e) => {
                    self.err_msg(&format!("epsh: {filename}: {e}"));
                    self.exit_status = ExitStatus::FAILURE;
                    return Err(ShellError::Runtime {
                        msg: format!("{filename}: {e}"),
                        span: redirs[0].span,
                    });
                }
            }
        }

        let mut fds = [0i32; 2];
        // SAFETY: Standard POSIX pipe+fork pattern for command substitution.
        // Child redirects stdout to write end of pipe, parent reads from read end.
        // All fds are valid from pipe(). from_raw_fd takes ownership of fds[0].
        unsafe {
            if sys::pipe(fds.as_mut_ptr()) != 0 {
                return Err(ShellError::Io(std::io::Error::last_os_error()));
            }

            let pid = sys::fork();
            if pid < 0 {
                sys::close(fds[0]);
                sys::close(fds[1]);
                return Err(ShellError::Io(std::io::Error::last_os_error()));
            }

            if pid == 0 {
                // Child: redirect stdout to write end of pipe
                libc::setpgid(0, 0);
                self.in_forked_child = true;
                sys::close(fds[0]);
                sys::dup2(fds[1], 1);
                sys::close(fds[1]);

                let status = match self.eval_command(cmd) {
                    Ok(s) => s,
                    Err(ShellError::Exit(n)) => n,
                    Err(_) => ExitStatus::FAILURE,
                };
                self.run_exit_trap();
                sys::exit_child(status);
            }

            // Parent: read from read end
            sys::close(fds[1]);
            self.child_pids.push(pid);

            let mut raw_output = Vec::new();
            let mut read_file = std::fs::File::from_raw_fd(fds[0]);
            let _ = read_file.read_to_end(&mut raw_output);
            let mut output = crate::encoding::bytes_to_str(&raw_output);

            self.exit_status = self.wait_child(pid)?;

            // Remove trailing newlines (POSIX requirement)
            while output.ends_with('\n') {
                output.pop();
            }

            Ok(output)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_echo() {
        let mut shell = Shell::new();
        let status = shell.run_script("echo hello");
        assert_eq!(status, 0);
    }

    #[test]
    fn run_true_false() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_script("true"), 0);
        assert_eq!(shell.run_script("false"), 1);
    }

    #[test]
    fn run_variable_assignment() {
        let mut shell = Shell::new();
        shell.run_script("FOO=bar");
        assert_eq!(shell.get_var("FOO"), Some("bar"));
    }

    #[test]
    fn run_variable_expansion() {
        let mut shell = Shell::new();
        shell.run_script("X=hello");
        assert_eq!(shell.get_var("X"), Some("hello"));
    }

    #[test]
    fn run_and_list() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_script("true && true"), 0);
        assert_eq!(shell.run_script("false && true"), 1);
        assert_eq!(shell.run_script("true && false"), 1);
    }

    #[test]
    fn run_or_list() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_script("false || true"), 0);
        assert_eq!(shell.run_script("true || false"), 0);
        assert_eq!(shell.run_script("false || false"), 1);
    }

    #[test]
    fn run_if_true() {
        let mut shell = Shell::new();
        shell.run_script("if true; then X=yes; else X=no; fi");
        assert_eq!(shell.get_var("X"), Some("yes"));
    }

    #[test]
    fn run_if_false() {
        let mut shell = Shell::new();
        shell.run_script("if false; then X=yes; else X=no; fi");
        assert_eq!(shell.get_var("X"), Some("no"));
    }

    #[test]
    fn run_for_loop() {
        let mut shell = Shell::new();
        shell.run_script("RESULT=''; for x in a b c; do RESULT=\"${RESULT}${x}\"; done");
        assert_eq!(shell.get_var("RESULT"), Some("abc"));
    }

    #[test]
    fn run_while_loop() {
        let mut shell = Shell::new();
        // Use a counter that doesn't need arithmetic expansion
        shell.run_script("I=x; while test $I != xxx; do I=${I}x; done");
        assert_eq!(shell.get_var("I"), Some("xxx"));
    }

    #[test]
    fn run_function() {
        let mut shell = Shell::new();
        shell.run_script("greet() { RESULT=$1; }; greet world");
        assert_eq!(shell.get_var("RESULT"), Some("world"));
    }

    #[test]
    fn run_case() {
        let mut shell = Shell::new();
        shell.run_script("X=b; case $X in a) R=first;; b) R=second;; esac");
        assert_eq!(shell.get_var("R"), Some("second"));
    }

    #[test]
    fn run_negation() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_script("! true"), 1);
        assert_eq!(shell.run_script("! false"), 0);
    }

    #[test]
    fn run_exit_status() {
        let mut shell = Shell::new();
        shell.run_script("false");
        assert_eq!(shell.exit_status, ExitStatus::FAILURE);
        shell.run_script("true");
        assert_eq!(shell.exit_status, ExitStatus::SUCCESS);
    }

    #[test]
    fn run_pipeline_external() {
        let mut shell = Shell::new();
        // Simple pipeline test using external commands
        let status = shell.run_script("echo hello | cat");
        assert_eq!(status, 0);
    }

    #[test]
    fn run_set_errexit() {
        let mut shell = Shell::new();
        // set -e should cause exit on first failure
        // The script sets X=before, then false fails, so X=after never runs
        let status = shell.run_script("set -e; X=before; false; X=after");
        assert_eq!(status, 1);
        assert_eq!(shell.get_var("X"), Some("before"));
    }

    #[test]
    fn run_test_builtin() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_script("test -z ''"), 0);
        assert_eq!(shell.run_script("test -z 'notempty'"), 1);
        assert_eq!(shell.run_script("test -n 'notempty'"), 0);
        assert_eq!(shell.run_script("[ 1 -eq 1 ]"), 0);
        assert_eq!(shell.run_script("[ 1 -eq 2 ]"), 1);
    }

    #[test]
    fn run_arithmetic_expansion() {
        let mut shell = Shell::new();
        shell.run_script("X=$((2 + 3 * 4))");
        assert_eq!(shell.get_var("X"), Some("14"));
    }

    #[test]
    fn run_arithmetic_with_variables() {
        let mut shell = Shell::new();
        shell.run_script("A=10; B=3; C=$((A + B))");
        assert_eq!(shell.get_var("C"), Some("13"));
    }

    #[test]
    fn run_arithmetic_assignment() {
        let mut shell = Shell::new();
        shell.run_script("I=0; I=$((I + 1)); I=$((I + 1)); I=$((I + 1))");
        assert_eq!(shell.get_var("I"), Some("3"));
    }

    #[test]
    fn run_while_with_arithmetic() {
        let mut shell = Shell::new();
        shell.run_script("I=0; while test $I -lt 5; do I=$((I + 1)); done");
        assert_eq!(shell.get_var("I"), Some("5"));
    }

    #[test]
    fn run_subshell() {
        let mut shell = Shell::new();
        // Subshell changes shouldn't affect parent
        shell.run_script("X=outer; (X=inner)");
        assert_eq!(shell.get_var("X"), Some("outer"));
    }

    #[test]
    fn run_local_vars() {
        let mut shell = Shell::new();
        shell.run_script("X=global; f() { local X=local; }; f");
        assert_eq!(shell.get_var("X"), Some("global"));
    }

    #[test]
    fn run_shift() {
        let mut shell = Shell::new();
        shell.set_args(&["test", "a", "b", "c"]);
        shell.run_script("shift; R=$1");
        assert_eq!(shell.get_var("R"), Some("b"));
    }

    #[test]
    fn run_command_substitution() {
        let mut shell = Shell::new();
        shell.run_script("X=$(echo hello)");
        assert_eq!(shell.get_var("X"), Some("hello"));
    }

    #[test]
    fn run_command_substitution_backtick() {
        let mut shell = Shell::new();
        shell.run_script("X=`echo world`");
        assert_eq!(shell.get_var("X"), Some("world"));
    }

    #[test]
    fn run_nested_command_substitution() {
        let mut shell = Shell::new();
        shell.run_script("X=$(echo $(echo nested))");
        assert_eq!(shell.get_var("X"), Some("nested"));
    }

    #[test]
    fn run_command_substitution_in_args() {
        let mut shell = Shell::new();
        // echo receives the output of the subcommand
        let status = shell.run_script("R=$(echo hello world); true");
        assert_eq!(status, 0);
        assert_eq!(shell.get_var("R"), Some("hello world"));
    }

    #[test]
    fn run_heredoc() {
        let mut shell = Shell::new();
        shell.run_script("R=$(cat <<EOF\nhello world\nEOF\n)");
        assert_eq!(shell.get_var("R"), Some("hello world"));
    }

    #[test]
    fn run_test_compound() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_script("[ 1 -eq 1 -a 2 -eq 2 ]"), 0);
        assert_eq!(shell.run_script("[ 1 -eq 1 -a 2 -eq 3 ]"), 1);
        assert_eq!(shell.run_script("[ 1 -eq 2 -o 2 -eq 2 ]"), 0);
        assert_eq!(shell.run_script("[ 1 -eq 2 -o 2 -eq 3 ]"), 1);
    }

    #[test]
    fn run_test_file_ops() {
        let mut shell = Shell::new();
        assert_eq!(shell.run_script("test -d /tmp"), 0);
        assert_eq!(shell.run_script("test -f /tmp"), 1);
        assert_eq!(shell.run_script("test -e /nonexistent"), 1);
    }

    #[test]
    fn run_trap() {
        let mut shell = Shell::new();
        shell.run_script("trap 'echo caught' INT");
        assert!(shell.traps.contains_key("INT"));
    }
}
