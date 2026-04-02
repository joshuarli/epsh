use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

use crate::ast::*;
use crate::error::{ShellError, Span};
use crate::expand;
use crate::glob;
use crate::parser::Parser;
use crate::sys;
use crate::var::Variables;

/// Shell state passed through evaluation.
pub struct Shell {
    pub vars: Variables,
    /// Defined functions: name → AST body
    pub functions: HashMap<String, Command>,
    /// Last command's exit status ($?)
    pub exit_status: i32,
    /// Shell's PID ($$)
    pub pid: u32,
    /// Number of nested loops (for break/continue counting)
    loop_depth: usize,
    /// Shell options
    pub opts: ShellOpts,
    /// Trap handlers: signal name → command string
    pub traps: HashMap<String, String>,
    /// True when evaluating a condition (if test, while cond, && / || operands).
    /// Suppresses set -e (errexit). Mirrors dash's EV_TESTED flag.
    tested: bool,
    /// Set by expansion when a fatal error occurs (bad substitution, etc.)
    pub expand_error: bool,
}

#[derive(Debug, Default)]
pub struct ShellOpts {
    /// -e: exit on error
    pub errexit: bool,
    /// -u: treat unset variables as error
    pub nounset: bool,
    /// -x: print commands before execution (xtrace)
    pub xtrace: bool,
}

/// Saved file descriptor for restoration after redirections.
struct SavedFd {
    target_fd: RawFd,
    saved_copy: Option<RawFd>,
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

impl Shell {
    pub fn new() -> Self {
        Shell {
            vars: Variables::new(),
            functions: HashMap::new(),
            exit_status: 0,
            pid: std::process::id(),
            loop_depth: 0,
            opts: ShellOpts::default(),
            traps: HashMap::new(),
            tested: false,
            expand_error: false,
        }
    }

    /// Execute a shell script from source text.
    pub fn run_script(&mut self, source: &str) -> i32 {
        let mut parser = Parser::new(source);
        let program = match parser.parse() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("epsh: {e}");
                return 2;
            }
        };

        for cmd in &program.commands {
            match self.eval_command(cmd) {
                Ok(status) => {
                    self.exit_status = status;
                }
                Err(ShellError::Exit(n)) => {
                    self.run_exit_trap();
                    return n;
                }
                Err(e) => {
                    eprintln!("epsh: {e}");
                    self.exit_status = 1;
                    if self.opts.errexit {
                        self.run_exit_trap();
                        return 1;
                    }
                }
            }
        }

        self.run_exit_trap();
        self.exit_status
    }

    /// Run the EXIT trap if one is set.
    fn run_exit_trap(&mut self) {
        // Remove the trap to prevent recursive invocation
        if let Some(action) = self.traps.remove("EXIT") {
            let mut parser = Parser::new(&action);
            if let Ok(program) = parser.parse() {
                for cmd in &program.commands {
                    let _ = self.eval_command(cmd);
                }
            }
        }
        // Also check lowercase "exit"
        if let Some(action) = self.traps.remove("exit") {
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

    /// Set a variable.
    pub fn set_var(&mut self, name: &str, value: &str) {
        let _ = self.vars.set(name, value);
    }

    /// Get a variable's value.
    pub fn get_var(&self, name: &str) -> Option<&str> {
        self.vars.get(name)
    }

    /// Expand a word to a list of fields (with field splitting and globbing).
    fn expand_fields(&mut self, word: &Word) -> Vec<String> {
        // Use a raw pointer to self for the cmd_subst closure.
        // This is safe because command_subst forks — the child gets its own
        // copy of all data, and the parent only reads from a pipe.
        let self_ptr = self as *mut Shell;
        let mut cmd_fn = |cmd: &Command| -> String {
            let shell = unsafe { &mut *self_ptr };
            shell.command_subst(cmd).unwrap_or_default()
        };
        let mut cmd_subst: Option<&mut dyn FnMut(&Command) -> String> = Some(&mut cmd_fn);
        expand::expand_word_to_fields(
            word,
            &mut self.vars,
            self.exit_status,
            self.pid,
            &mut cmd_subst,
        )
    }

    /// Expand a word to a single string (no field splitting or globbing).
    fn expand_string(&mut self, word: &Word) -> String {
        let self_ptr = self as *mut Shell;
        let mut cmd_fn = |cmd: &Command| -> String {
            let shell = unsafe { &mut *self_ptr };
            shell.command_subst(cmd).unwrap_or_default()
        };
        let mut cmd_subst: Option<&mut dyn FnMut(&Command) -> String> = Some(&mut cmd_fn);
        expand::expand_word_to_string(
            word,
            &mut self.vars,
            self.exit_status,
            self.pid,
            &mut cmd_subst,
        )
    }

    /// Evaluate a command node, returning its exit status.
    pub fn eval_command(&mut self, cmd: &Command) -> crate::error::Result<i32> {
        let status = self.eval_command_inner(cmd)?;
        self.exit_status = status;

        // Errexit check — mirrors dash's eval.c line 330.
        // Only fires from "leaf" nodes (simple commands, pipelines, subshells)
        // where checkexit = EV_TESTED. Compound commands (if, while, for, etc.)
        // propagate status but don't trigger errexit directly.
        let is_leaf = matches!(
            cmd,
            Command::Simple { .. }
                | Command::Pipeline { .. }
                | Command::Subshell { .. }
                | Command::Background { .. }
        );
        if self.opts.errexit && is_leaf && !self.tested && status != 0 {
            return Err(ShellError::Exit(status));
        }

        Ok(status)
    }

    fn eval_command_inner(&mut self, cmd: &Command) -> crate::error::Result<i32> {
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
                let status = self.eval_pipeline(commands)?;
                Ok(if *bang {
                    if status == 0 { 1 } else { 0 }
                } else {
                    status
                })
            }

            Command::And(left, right) => {
                // Left side of && is always tested (errexit suppressed)
                let saved = self.tested;
                self.tested = true;
                let status = self.eval_command(left)?;
                self.tested = saved;
                self.exit_status = status;
                if status == 0 {
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
                if status != 0 {
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
                // Fork a child process for the subshell
                self.eval_in_subshell(body, redirs)
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
                if cond_status == 0 {
                    self.eval_command(then_part)
                } else if let Some(else_part) = else_part {
                    self.eval_command(else_part)
                } else {
                    Ok(0)
                }
            }

            Command::While { cond, body, .. } => {
                self.loop_depth += 1;
                let mut last_status = 0;
                loop {
                    let saved = self.tested;
                    self.tested = true;
                    let cond_status = self.eval_command(cond)?;
                    self.tested = saved;
                    self.exit_status = cond_status;
                    if cond_status != 0 {
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
                let mut last_status = 0;
                loop {
                    let saved = self.tested;
                    self.tested = true;
                    let cond_status = self.eval_command(cond)?;
                    self.tested = saved;
                    self.exit_status = cond_status;
                    if cond_status == 0 {
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
                        expanded.extend(self.expand_fields(w));
                    }
                    expanded
                } else {
                    // No 'in' clause: use positional parameters
                    self.vars.positional.clone()
                };

                self.loop_depth += 1;
                let mut last_status = 0;
                for value in &word_list {
                    let _ = self.vars.set(var, value);
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
                let expanded = self.expand_string(word);

                for arm in arms {
                    for pattern in &arm.patterns {
                        let pat = self.expand_string(pattern);
                        if glob::fnmatch(&pat, &expanded) {
                            if let Some(ref body) = arm.body {
                                return self.eval_command(body);
                            } else {
                                return Ok(0);
                            }
                        }
                    }
                }
                Ok(0)
            }

            Command::FuncDef { name, body, .. } => {
                self.functions.insert(name.clone(), *body.clone());
                Ok(0)
            }

            Command::Not(cmd) => {
                let saved = self.tested;
                self.tested = true;
                let status = self.eval_command(cmd)?;
                self.tested = saved;
                Ok(if status == 0 { 1 } else { 0 })
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
    ) -> crate::error::Result<i32> {
        // Expand arguments
        let mut expanded_args: Vec<String> = Vec::new();
        for arg in args {
            expanded_args.extend(self.expand_fields(arg));
        }

        // No command name: just apply assignments to the shell environment
        if expanded_args.is_empty() {
            for assign in assigns {
                let value = self.expand_string(&assign.value);
                self.vars
                    .set(&assign.name, &value)
                    .map_err(|msg| ShellError::Runtime { msg, span })?;
            }
            return Ok(0);
        }

        let cmd_name = &expanded_args[0];

        // Setup redirections before executing (applies to builtins, functions, externals)
        let saved_fds = self.setup_redirections(redirs)?;

        let result =
            if let Some(status) = self.try_builtin(cmd_name, &expanded_args, assigns, &[], span)? {
                Ok(status)
            } else if let Some(func_body) = self.functions.get(cmd_name).cloned() {
                self.eval_function(&func_body, &expanded_args, assigns, &[], span)
            } else {
                self.eval_external(&expanded_args, assigns, &[], span)
            };

        self.restore_redirections(saved_fds);
        result
    }

    /// Try to run a builtin command. Returns None if not a builtin.
    fn try_builtin(
        &mut self,
        name: &str,
        args: &[String],
        _assigns: &[Assignment],
        redirs: &[Redir],
        span: Span,
    ) -> crate::error::Result<Option<i32>> {
        let status = match name {
            ":" | "true" => Some(0),
            "false" => Some(1),
            "echo" => Some(self.builtin_echo(args)),
            "cd" => Some(self.builtin_cd(args)),
            "pwd" => match std::env::current_dir() {
                Ok(p) => {
                    write_stdout(&format!("{}\n", p.display()));
                    Some(0)
                }
                Err(e) => {
                    eprintln!("pwd: {e}");
                    Some(1)
                }
            },
            "exit" => {
                let code = args
                    .get(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(self.exit_status);
                return Err(ShellError::Exit(code));
            }
            "return" => {
                let code = args
                    .get(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(self.exit_status);
                return Err(ShellError::Return(code));
            }
            "break" => {
                if let Some(arg) = args.get(1) {
                    match arg.parse::<usize>() {
                        Ok(0) => {
                            eprintln!("break: Illegal number: {arg}");
                            return Err(ShellError::Exit(1));
                        }
                        Ok(n) => {
                            if self.loop_depth == 0 {
                                Some(0)
                            } else {
                                return Err(ShellError::Break(n.min(self.loop_depth)));
                            }
                        }
                        Err(_) => {
                            eprintln!("break: Illegal number: {arg}");
                            return Err(ShellError::Exit(1));
                        }
                    }
                } else if self.loop_depth == 0 {
                    Some(0)
                } else {
                    return Err(ShellError::Break(1));
                }
            }
            "continue" => {
                if let Some(arg) = args.get(1) {
                    match arg.parse::<usize>() {
                        Ok(0) => {
                            eprintln!("continue: Illegal number: {arg}");
                            return Err(ShellError::Exit(1));
                        }
                        Ok(n) => {
                            if self.loop_depth == 0 {
                                Some(0)
                            } else {
                                return Err(ShellError::Continue(n.min(self.loop_depth)));
                            }
                        }
                        Err(_) => {
                            eprintln!("continue: Illegal number: {arg}");
                            return Err(ShellError::Exit(1));
                        }
                    }
                } else if self.loop_depth == 0 {
                    Some(0)
                } else {
                    return Err(ShellError::Continue(1));
                }
            }
            "export" => Some(self.builtin_export(args)),
            "readonly" => Some(self.builtin_readonly(args)),
            "unset" => Some(self.builtin_unset(args)),
            "set" => Some(self.builtin_set(args)),
            "shift" => Some(self.builtin_shift(args)),
            "eval" => return self.builtin_eval(args).map(Some),
            "." | "source" => Some(self.builtin_dot(args, span)?),
            "test" | "[" => Some(self.builtin_test(args)),
            "read" => Some(self.builtin_read(args)),
            "local" => Some(self.builtin_local(args)),
            "exec" => return self.builtin_exec(args, redirs, span).map(Some),
            "command" => Some(self.builtin_command(args, span)?),
            "type" => Some(self.builtin_type(args)),
            "wait" => Some(self.builtin_wait(args)),
            "trap" => Some(self.builtin_trap(args)),
            "umask" => Some(self.builtin_umask(args)),
            "getopts" => Some(self.builtin_getopts(args)),
            "printf" => Some(self.builtin_printf(args)),
            _ => None,
        };

        Ok(status)
    }

    /// Evaluate a function call.
    fn eval_function(
        &mut self,
        body: &Command,
        args: &[String],
        assigns: &[Assignment],
        redirs: &[Redir],
        _span: Span,
    ) -> crate::error::Result<i32> {
        // Save and set positional parameters
        let saved_positional = self.vars.positional.clone();
        self.vars.positional = args[1..].to_vec();

        // Push scope for local variables
        self.vars.push_scope();

        // Apply temporary assignments
        for assign in assigns {
            let value = self.expand_string(&assign.value);
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

    /// Execute an external command.
    fn eval_external(
        &mut self,
        args: &[String],
        assigns: &[Assignment],
        redirs: &[Redir],
        _span: Span,
    ) -> crate::error::Result<i32> {
        let saved = self.setup_redirections(redirs)?;

        let mut cmd = std::process::Command::new(&args[0]);
        cmd.args(&args[1..]);

        // Set environment: exported vars + temporary assignments
        cmd.env_clear();
        for (k, v) in self.vars.exported_env() {
            cmd.env(&k, &v);
        }
        for assign in assigns {
            let value = self.expand_string(&assign.value);
            cmd.env(&assign.name, &value);
        }

        // Inherit stdio (redirections are already applied to our fds)
        let result = match cmd.status() {
            Ok(status) => {
                let code = status.code().unwrap_or(128);
                Ok(code)
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    eprintln!("{}: not found", args[0]);
                    Ok(127)
                } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                    eprintln!("{}: permission denied", args[0]);
                    Ok(126)
                } else {
                    Err(ShellError::Io(e))
                }
            }
        };

        self.restore_redirections(saved);
        result
    }

    /// Execute a pipeline.
    fn eval_pipeline(&mut self, commands: &[Command]) -> crate::error::Result<i32> {
        if commands.len() == 1 {
            return self.eval_command(&commands[0]);
        }

        // Create pipes between stages
        let mut pipes: Vec<(RawFd, RawFd)> = Vec::new();
        for _ in 0..commands.len() - 1 {
            let mut fds = [0i32; 2];
            unsafe {
                if sys::pipe(fds.as_mut_ptr()) != 0 {
                    return Err(ShellError::Io(std::io::Error::last_os_error()));
                }
            }
            pipes.push((fds[0], fds[1]));
        }

        let mut children = Vec::new();

        for (i, cmd) in commands.iter().enumerate() {
            unsafe {
                let pid = sys::fork();
                if pid < 0 {
                    return Err(ShellError::Io(std::io::Error::last_os_error()));
                }

                if pid == 0 {
                    // Child process
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

                    // Execute command
                    let status = match self.eval_command(cmd) {
                        Ok(s) => s,
                        Err(ShellError::Exit(n)) => n,
                        Err(_) => 1,
                    };
                    sys::exit_child(status);
                }

                children.push(pid);
            }
        }

        // Parent: close all pipe fds
        for &(read_fd, write_fd) in &pipes {
            unsafe {
                sys::close(read_fd);
                sys::close(write_fd);
            }
        }

        // Wait for all children, return status of the last one
        let mut last_status = 0;
        for (i, &pid) in children.iter().enumerate() {
            let mut status = 0i32;
            unsafe {
                sys::waitpid(pid, &mut status, 0);
            }
            if i == children.len() - 1 {
                if sys::wifexited(status) {
                    last_status = sys::wexitstatus(status);
                } else {
                    last_status = 128 + sys::wtermsig(status);
                }
            }
        }

        Ok(last_status)
    }

    /// Execute a command in a subshell (fork).
    fn eval_in_subshell(&mut self, body: &Command, redirs: &[Redir]) -> crate::error::Result<i32> {
        unsafe {
            let pid = sys::fork();
            if pid < 0 {
                return Err(ShellError::Io(std::io::Error::last_os_error()));
            }

            if pid == 0 {
                // Child — clear parent's traps, they don't inherit
                let parent_exit_trap = self.traps.remove("EXIT")
                    .or_else(|| self.traps.remove("exit"));
                drop(parent_exit_trap); // parent EXIT trap doesn't run in subshell

                let _saved = match self.setup_redirections(redirs) {
                    Ok(s) => s,
                    Err(_) => sys::exit_child(1),
                };
                let status = match self.eval_command(body) {
                    Ok(s) => s,
                    Err(ShellError::Exit(n)) => n,
                    Err(_) => 1,
                };
                // Run subshell's own EXIT trap if it set one
                self.run_exit_trap();
                sys::exit_child(status);
            }

            // Parent
            let mut status = 0i32;
            sys::waitpid(pid, &mut status, 0);
            if sys::wifexited(status) {
                Ok(sys::wexitstatus(status))
            } else {
                Ok(128 + sys::wtermsig(status))
            }
        }
    }

    /// Execute a command in the background.
    fn eval_background(&mut self, cmd: &Command, redirs: &[Redir]) -> crate::error::Result<i32> {
        unsafe {
            let pid = sys::fork();
            if pid < 0 {
                return Err(ShellError::Io(std::io::Error::last_os_error()));
            }

            if pid == 0 {
                let _saved = match self.setup_redirections(redirs) {
                    Ok(s) => s,
                    Err(_) => sys::exit_child(1),
                };
                let status = match self.eval_command(cmd) {
                    Ok(s) => s,
                    Err(ShellError::Exit(n)) => n,
                    Err(_) => 1,
                };
                sys::exit_child(status);
            }

            // Don't wait — background process
            Ok(0)
        }
    }

    /// Apply redirections. Returns saved FD state for restoration.
    fn setup_redirections(&mut self, redirs: &[Redir]) -> crate::error::Result<Vec<SavedFd>> {
        let mut saved = Vec::new();

        for redir in redirs {
            let target_fd = redir.fd;

            // Save the current fd
            let saved_copy = {
                let copy = sys::fcntl_dupfd_cloexec(target_fd, 10);
                if copy >= 0 { Some(copy) } else { None }
            };

            saved.push(SavedFd {
                target_fd,
                saved_copy,
            });

            match &redir.kind {
                RedirKind::Input(word) => {
                    let filename = self.expand_string(word);
                    let file = std::fs::File::open(&filename).map_err(|e| {
                        eprintln!("{filename}: {e}");
                        ShellError::Io(e)
                    })?;
                    unsafe {
                        sys::dup2(file.as_raw_fd(), target_fd);
                    }
                }
                RedirKind::Output(word) | RedirKind::Clobber(word) => {
                    let filename = self.expand_string(word);
                    let file = std::fs::File::create(&filename).map_err(|e| {
                        eprintln!("{filename}: {e}");
                        ShellError::Io(e)
                    })?;
                    unsafe {
                        sys::dup2(file.as_raw_fd(), target_fd);
                    }
                }
                RedirKind::Append(word) => {
                    let filename = self.expand_string(word);
                    let file = std::fs::OpenOptions::new()
                        .create(true)
                        .truncate(false)
                        .append(true)
                        .open(&filename)
                        .map_err(|e| {
                            eprintln!("{filename}: {e}");
                            ShellError::Io(e)
                        })?;
                    unsafe {
                        sys::dup2(file.as_raw_fd(), target_fd);
                    }
                }
                RedirKind::ReadWrite(word) => {
                    let filename = self.expand_string(word);
                    let file = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false)
                        .open(&filename)
                        .map_err(|e| {
                            eprintln!("{filename}: {e}");
                            ShellError::Io(e)
                        })?;
                    unsafe {
                        sys::dup2(file.as_raw_fd(), target_fd);
                    }
                }
                RedirKind::DupInput(word) | RedirKind::DupOutput(word) => {
                    let fd_str = self.expand_string(word);
                    if fd_str == "-" {
                        unsafe {
                            sys::close(target_fd);
                        }
                    } else if let Ok(source_fd) = fd_str.parse::<i32>() {
                        unsafe {
                            sys::dup2(source_fd, target_fd);
                        }
                    } else {
                        return Err(ShellError::Runtime {
                            msg: format!("{fd_str}: bad file descriptor"),
                            span: redir.span,
                        });
                    }
                }
                RedirKind::HereDoc { body, quoted } | RedirKind::HereDocStrip { body, quoted } => {
                    let mut fds = [0i32; 2];
                    unsafe {
                        sys::pipe(fds.as_mut_ptr());
                    }
                    let write_end = unsafe { std::fs::File::from_raw_fd(fds[1]) };
                    let read_fd = fds[0];

                    // Expand body if delimiter was unquoted (like double-quote context)
                    let expanded = if !quoted {
                        let word = Word {
                            parts: crate::parser::parse_word_parts(body),
                            span: redir.span,
                        };
                        self.expand_string(&word)
                    } else {
                        body.clone()
                    };
                    let _ = (&write_end).write_all(expanded.as_bytes());
                    drop(write_end);

                    unsafe {
                        sys::dup2(read_fd, target_fd);
                        sys::close(read_fd);
                    }
                }
            }
        }

        Ok(saved)
    }

    /// Restore file descriptors after redirections.
    fn restore_redirections(&self, saved: Vec<SavedFd>) {
        for s in saved.into_iter().rev() {
            if let Some(copy) = s.saved_copy {
                unsafe {
                    sys::dup2(copy, s.target_fd);
                    sys::close(copy);
                }
            }
        }
    }

    /// Execute a command substitution and return its output.
    pub fn command_subst(&mut self, cmd: &Command) -> crate::error::Result<String> {
        let mut fds = [0i32; 2];
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
                sys::close(fds[0]);
                sys::dup2(fds[1], 1);
                sys::close(fds[1]);

                let status = match self.eval_command(cmd) {
                    Ok(s) => s,
                    Err(ShellError::Exit(n)) => n,
                    Err(_) => 1,
                };
                sys::exit_child(status);
            }

            // Parent: read from read end
            sys::close(fds[1]);

            let mut output = String::new();
            let mut read_file = std::fs::File::from_raw_fd(fds[0]);
            let _ = read_file.read_to_string(&mut output);

            let mut status = 0i32;
            sys::waitpid(pid, &mut status, 0);
            if sys::wifexited(status) {
                self.exit_status = sys::wexitstatus(status);
            }

            // Remove trailing newlines (POSIX requirement)
            while output.ends_with('\n') {
                output.pop();
            }

            Ok(output)
        }
    }

    // ── Builtins ──────────────────────────────────────────────────────

    fn builtin_echo(&self, args: &[String]) -> i32 {
        let mut i = 1;
        let mut newline = true;
        let mut escape = false;

        // Check for -n and -e/-E flags
        while i < args.len() {
            if args[i] == "-n" {
                newline = false;
                i += 1;
            } else if args[i] == "-e" {
                escape = true;
                i += 1;
            } else if args[i] == "-E" {
                escape = false;
                i += 1;
            } else {
                break;
            }
        }

        let mut text = args[i..].join(" ");
        if escape {
            text = unescape_echo(&text);
        }
        if newline {
            text.push('\n');
        }
        // Write directly to fd 1 to bypass Rust's stdout buffering.
        // This is necessary for command substitution where fd 1 is a pipe.
        unsafe {
            sys::write(1, text.as_ptr() as *const _, text.len());
        }
        0
    }

    fn builtin_cd(&mut self, args: &[String]) -> i32 {
        let dir = if args.len() > 1 {
            args[1].as_str()
        } else {
            match self.vars.get("HOME") {
                Some(h) => h,
                None => {
                    eprintln!("cd: HOME not set");
                    return 1;
                }
            }
        };

        match std::env::set_current_dir(dir) {
            Ok(()) => {
                if let Ok(pwd) = std::env::current_dir() {
                    let _ = self.vars.set("PWD", &pwd.to_string_lossy());
                }
                0
            }
            Err(e) => {
                eprintln!("cd: {dir}: {e}");
                1
            }
        }
    }

    fn builtin_export(&mut self, args: &[String]) -> i32 {
        if args.len() <= 1 {
            // Print all exported variables
            for (k, v) in self.vars.exported_env() {
                write_stdout(&format!("export {k}=\"{v}\"\n"));
            }
            return 0;
        }

        for arg in &args[1..] {
            if let Some(eq) = arg.find('=') {
                let name = &arg[..eq];
                let value = &arg[eq + 1..];
                let _ = self.vars.set(name, value);
                self.vars.export(name);
            } else {
                self.vars.export(arg);
            }
        }
        0
    }

    fn builtin_readonly(&mut self, args: &[String]) -> i32 {
        for arg in &args[1..] {
            if let Some(eq) = arg.find('=') {
                let name = &arg[..eq];
                let value = &arg[eq + 1..];
                let _ = self.vars.set(name, value);
                self.vars.set_readonly(name);
            } else {
                self.vars.set_readonly(arg);
            }
        }
        0
    }

    fn builtin_unset(&mut self, args: &[String]) -> i32 {
        let mut status = 0;
        let mut unset_funcs = false;
        let mut i = 1;

        while i < args.len() {
            if args[i] == "-v" {
                unset_funcs = false;
                i += 1;
            } else if args[i] == "-f" {
                unset_funcs = true;
                i += 1;
            } else {
                break;
            }
        }

        for arg in &args[i..] {
            if unset_funcs {
                self.functions.remove(arg.as_str());
            } else if let Err(e) = self.vars.unset(arg) {
                eprintln!("unset: {e}");
                status = 1;
            }
        }
        status
    }

    fn builtin_set(&mut self, args: &[String]) -> i32 {
        if args.len() <= 1 {
            return 0;
        }

        let mut i = 1;
        while i < args.len() {
            let arg = &args[i];
            if arg == "--" {
                i += 1;
                // Remaining args become positional parameters
                self.vars.positional = args[i..].to_vec();
                return 0;
            } else if arg.starts_with('-') || arg.starts_with('+') {
                let enable = arg.starts_with('-');
                for ch in arg[1..].chars() {
                    match ch {
                        'e' => self.opts.errexit = enable,
                        'u' => self.opts.nounset = enable,
                        'x' => self.opts.xtrace = enable,
                        _ => {
                            eprintln!("set: unknown option: -{ch}");
                            return 1;
                        }
                    }
                }
                i += 1;
            } else {
                // Positional parameters
                self.vars.positional = args[i..].to_vec();
                return 0;
            }
        }
        0
    }

    fn builtin_shift(&mut self, args: &[String]) -> i32 {
        let n = args
            .get(1)
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(1);
        if n > self.vars.positional.len() {
            eprintln!("shift: can't shift that many");
            return 1;
        }
        self.vars.positional = self.vars.positional[n..].to_vec();
        0
    }

    fn builtin_eval(&mut self, args: &[String]) -> crate::error::Result<i32> {
        if args.len() <= 1 {
            return Ok(0);
        }
        let script = args[1..].join(" ");
        // eval must propagate control flow (break, continue, return, exit)
        // unlike run_script which catches them
        let mut parser = Parser::new(&script);
        let program = match parser.parse() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("epsh: {e}");
                return Ok(2);
            }
        };
        let mut status = 0;
        for cmd in &program.commands {
            status = self.eval_command(cmd)?;
            self.exit_status = status;
        }
        Ok(status)
    }

    fn builtin_dot(&mut self, args: &[String], _span: Span) -> crate::error::Result<i32> {
        if args.len() <= 1 {
            eprintln!(".: filename argument required");
            return Err(ShellError::Exit(2));
        }
        let filename = &args[1];
        let content = match std::fs::read_to_string(filename) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(".: {filename}: {e}");
                // . is a special builtin — file not found is fatal
                return Err(ShellError::Exit(127));
            }
        };
        // dot runs in the current shell (not a subshell), and must
        // propagate break/continue/return/exit
        let mut parser = Parser::new(&content);
        let program = match parser.parse() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("epsh: {e}");
                return Ok(2);
            }
        };
        let mut status = 0;
        for cmd in &program.commands {
            status = self.eval_command(cmd)?;
            self.exit_status = status;
        }
        Ok(status)
    }

    fn builtin_test(&self, args: &[String]) -> i32 {
        let args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let args = if args[0] == "[" {
            if args.last() != Some(&"]") {
                eprintln!("[: missing ]");
                return 2;
            }
            &args[1..args.len() - 1]
        } else {
            &args[1..]
        };
        test_eval(args)
    }

    fn builtin_read(&mut self, args: &[String]) -> i32 {
        // Parse options
        let mut i = 1;
        let mut raw_mode = false;
        while i < args.len() && args[i].starts_with('-') {
            if args[i] == "-r" {
                raw_mode = true;
            }
            i += 1;
        }

        let var_names: Vec<&str> = if i < args.len() {
            args[i..].iter().map(|s| s.as_str()).collect()
        } else {
            vec!["REPLY"]
        };

        // Read a line char-by-char from fd 0 (works with redirections)
        let mut line = String::new();
        let mut buf = [0u8; 1];
        let mut got_data = false;
        let mut continued = false;
        loop {
            let n = unsafe { sys::read(0, buf.as_mut_ptr().cast(), 1) };
            if n <= 0 {
                break; // EOF or error
            }
            got_data = true;
            let ch = buf[0] as char;
            if ch == '\n' {
                if continued {
                    continued = false;
                    continue;
                }
                break;
            }
            if ch == '\\' && !raw_mode {
                // Line continuation: peek at next char
                let n2 = unsafe { sys::read(0, buf.as_mut_ptr().cast(), 1) };
                if n2 <= 0 {
                    line.push('\\');
                    break;
                }
                if buf[0] == b'\n' {
                    continued = true;
                    continue;
                }
                line.push(buf[0] as char);
                continued = false;
                continue;
            }
            continued = false;
            line.push(ch);
        }

        if !got_data {
            // EOF: set variables to empty
            for name in &var_names {
                let _ = self.vars.set(name, "");
            }
            return 1;
        }

        // Split on IFS and assign to variables
        let ifs = self.vars.ifs().to_string();
        if var_names.len() == 1 {
            // Single variable gets the whole line (with leading/trailing IFS stripped)
            let trimmed = line.trim_matches(|c: char| ifs.contains(c));
            let _ = self.vars.set(var_names[0], trimmed);
        } else {
            // Multiple variables: split on IFS, last gets remainder
            let mut fields = Vec::new();
            let chars: Vec<char> = line.chars().collect();
            let mut pos = 0;

            // Skip leading IFS whitespace
            while pos < chars.len() && ifs.contains(chars[pos]) && chars[pos].is_whitespace() {
                pos += 1;
            }

            for vi in 0..var_names.len() {
                if vi == var_names.len() - 1 {
                    // Last variable gets the rest
                    let rest: String = chars[pos..].iter().collect();
                    // Trim trailing IFS whitespace
                    let trimmed =
                        rest.trim_end_matches(|c: char| ifs.contains(c) && c.is_whitespace());
                    fields.push(trimmed.to_string());
                } else {
                    // Accumulate until next IFS char
                    let start = pos;
                    while pos < chars.len() && !ifs.contains(chars[pos]) {
                        pos += 1;
                    }
                    fields.push(chars[start..pos].iter().collect());
                    // Skip IFS delimiters
                    while pos < chars.len() && ifs.contains(chars[pos]) {
                        pos += 1;
                    }
                }
            }

            for (vi, name) in var_names.iter().enumerate() {
                let value = fields.get(vi).map(|s| s.as_str()).unwrap_or("");
                let _ = self.vars.set(name, value);
            }
        }
        0
    }

    fn builtin_local(&mut self, args: &[String]) -> i32 {
        for arg in &args[1..] {
            if let Some(eq) = arg.find('=') {
                let name = &arg[..eq];
                let value = &arg[eq + 1..];
                self.vars.make_local(name);
                let _ = self.vars.set(name, value);
            } else {
                self.vars.make_local(arg);
            }
        }
        0
    }

    fn builtin_exec(
        &mut self,
        args: &[String],
        redirs: &[Redir],
        _span: Span,
    ) -> crate::error::Result<i32> {
        // Apply redirections to the shell itself (no save/restore)
        for redir in redirs {
            // Same setup but don't save
            match &redir.kind {
                RedirKind::Input(word) => {
                    let filename = self.expand_string(word);
                    let file = std::fs::File::open(&filename)?;
                    unsafe {
                        sys::dup2(file.as_raw_fd(), redir.fd);
                    }
                    std::mem::forget(file);
                }
                RedirKind::Output(word) | RedirKind::Clobber(word) => {
                    let filename = self.expand_string(word);
                    let file = std::fs::File::create(&filename)?;
                    unsafe {
                        sys::dup2(file.as_raw_fd(), redir.fd);
                    }
                    std::mem::forget(file);
                }
                _ => {} // TODO: other redirect types for exec
            }
        }

        if args.len() <= 1 {
            // exec with only redirections — modify shell's fds
            return Ok(0);
        }

        // exec with command — replace process
        let err = exec::execvp(&args[0], args);
        eprintln!("exec: {}: {err}", args[0]);
        Ok(126)
    }

    fn builtin_command(&mut self, args: &[String], span: Span) -> crate::error::Result<i32> {
        if args.len() <= 1 {
            return Ok(0);
        }
        // Skip -v/-V flags
        let mut i = 1;
        while i < args.len() && args[i].starts_with('-') {
            i += 1;
        }
        if i >= args.len() {
            return Ok(0);
        }
        let new_args: Vec<String> = args[i..].to_vec();

        // `command` bypasses functions but NOT builtins
        if let Some(status) = self.try_builtin(&new_args[0], &new_args, &[], &[], span)? {
            Ok(status)
        } else {
            self.eval_external(&new_args, &[], &[], span)
        }
    }

    fn builtin_type(&self, args: &[String]) -> i32 {
        let mut status = 0;
        for name in &args[1..] {
            if self.functions.contains_key(name.as_str()) {
                write_stdout(&format!("{name} is a function\n"));
            } else if is_builtin(name) {
                write_stdout(&format!("{name} is a shell builtin\n"));
            } else if let Ok(path) = which(name) {
                write_stdout(&format!("{name} is {path}\n"));
            } else {
                eprintln!("{name}: not found");
                status = 1;
            }
        }
        status
    }

    fn builtin_wait(&mut self, _args: &[String]) -> i32 {
        // Wait for all background children
        loop {
            let mut status = 0i32;
            let pid = unsafe { sys::waitpid(-1, &mut status, 0) };
            if pid <= 0 {
                break;
            }
        }
        0
    }

    fn builtin_trap(&mut self, args: &[String]) -> i32 {
        if args.len() <= 1 {
            // Print current traps
            for (sig, action) in &self.traps {
                write_stdout(&format!("trap -- '{}' {}\n", action, sig));
            }
            return 0;
        }
        if args.len() == 2 {
            // trap '' SIG or trap - SIG
            if args[1] == "-" {
                // Reset all traps
                self.traps.clear();
            }
            return 0;
        }
        let action = &args[1];
        for sig_name in &args[2..] {
            if action == "-" {
                self.traps.remove(sig_name.as_str());
            } else {
                self.traps.insert(sig_name.clone(), action.clone());
            }
        }
        0
    }

    fn builtin_umask(&self, args: &[String]) -> i32 {
        if args.len() <= 1 {
            let mask = unsafe { sys::umask(0) };
            unsafe {
                sys::umask(mask);
            }
            write_stdout(&format!("{mask:04o}\n"));
            return 0;
        }
        if let Ok(mask) = u32::from_str_radix(&args[1], 8) {
            unsafe {
                sys::umask(mask as libc::mode_t);
            }
            0
        } else {
            eprintln!("umask: {}: invalid mask", args[1]);
            1
        }
    }

    fn builtin_getopts(&mut self, args: &[String]) -> i32 {
        if args.len() < 3 {
            eprintln!("getopts: usage: getopts optstring name [arg ...]");
            return 2;
        }
        let optstring = &args[1];
        let name = &args[2];
        let argv: Vec<String> = if args.len() > 3 {
            args[3..].to_vec()
        } else {
            self.vars.positional.clone()
        };

        let optind: usize = self
            .vars
            .get("OPTIND")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);

        if optind < 1 || optind > argv.len() {
            let _ = self.vars.set(name, "?");
            return 1;
        }

        let arg = &argv[optind - 1];
        if !arg.starts_with('-') || arg == "-" || arg == "--" {
            let _ = self.vars.set(name, "?");
            return 1;
        }

        let optchars: Vec<char> = optstring.chars().collect();
        let arg_chars: Vec<char> = arg.chars().collect();

        // Track position within the current arg (for grouped short opts)
        let optpos: usize = self
            .vars
            .get("_OPTPOS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);

        if optpos >= arg_chars.len() {
            // Move to next arg
            let _ = self.vars.set("OPTIND", &(optind + 1).to_string());
            let _ = self.vars.set("_OPTPOS", "1");
            return self.builtin_getopts(args);
        }

        let opt = arg_chars[optpos];

        // Find opt in optstring
        let found = optchars.iter().position(|&c| c == opt);
        match found {
            Some(pos) => {
                let _ = self.vars.set(name, &opt.to_string());
                let needs_arg = pos + 1 < optchars.len() && optchars[pos + 1] == ':';
                if needs_arg {
                    // Option argument
                    if optpos + 1 < arg_chars.len() {
                        // Argument is rest of current word
                        let optarg: String = arg_chars[optpos + 1..].iter().collect();
                        let _ = self.vars.set("OPTARG", &optarg);
                        let _ = self.vars.set("OPTIND", &(optind + 1).to_string());
                        let _ = self.vars.set("_OPTPOS", "1");
                    } else if optind < argv.len() {
                        // Argument is next word
                        let _ = self.vars.set("OPTARG", &argv[optind]);
                        let _ = self.vars.set("OPTIND", &(optind + 2).to_string());
                        let _ = self.vars.set("_OPTPOS", "1");
                    } else {
                        eprintln!("getopts: option requires argument -- {opt}");
                        let _ = self.vars.set(name, "?");
                        let _ = self.vars.set("OPTIND", &(optind + 1).to_string());
                        return 0;
                    }
                } else {
                    let _ = self.vars.unset("OPTARG");
                    if optpos + 1 >= arg_chars.len() {
                        let _ = self.vars.set("OPTIND", &(optind + 1).to_string());
                        let _ = self.vars.set("_OPTPOS", "1");
                    } else {
                        let _ = self.vars.set("_OPTPOS", &(optpos + 1).to_string());
                    }
                }
                0
            }
            None => {
                eprintln!("getopts: illegal option -- {opt}");
                let _ = self.vars.set(name, "?");
                if optpos + 1 >= arg_chars.len() {
                    let _ = self.vars.set("OPTIND", &(optind + 1).to_string());
                    let _ = self.vars.set("_OPTPOS", "1");
                } else {
                    let _ = self.vars.set("_OPTPOS", &(optpos + 1).to_string());
                }
                0
            }
        }
    }

    fn builtin_printf(&self, args: &[String]) -> i32 {
        if args.len() < 2 {
            eprintln!("printf: usage: printf format [arguments]");
            return 1;
        }

        let format = &args[1];
        let mut arg_idx = 2;
        let fmt_chars: Vec<char> = format.chars().collect();
        let mut i = 0;
        let mut out = String::new();

        use std::fmt::Write;

        while i < fmt_chars.len() {
            if fmt_chars[i] == '\\' {
                i += 1;
                if i < fmt_chars.len() {
                    match fmt_chars[i] {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        '\\' => out.push('\\'),
                        '"' => out.push('"'),
                        'a' => out.push('\x07'),
                        'b' => out.push('\x08'),
                        'f' => out.push('\x0c'),
                        'v' => out.push('\x0b'),
                        '0' => {
                            i += 1;
                            let start = i;
                            while i < fmt_chars.len()
                                && i < start + 3
                                && matches!(fmt_chars[i], '0'..='7')
                            {
                                i += 1;
                            }
                            let oct: String = fmt_chars[start..i].iter().collect();
                            let n = u8::from_str_radix(&oct, 8).unwrap_or(0);
                            out.push(n as char);
                            continue;
                        }
                        c => {
                            out.push('\\');
                            out.push(c);
                        }
                    }
                    i += 1;
                } else {
                    out.push('\\');
                }
            } else if fmt_chars[i] == '%' {
                i += 1;
                if i >= fmt_chars.len() {
                    out.push('%');
                    break;
                }
                let mut flags = String::new();
                while i < fmt_chars.len() && matches!(fmt_chars[i], '-' | '+' | ' ' | '0' | '#') {
                    flags.push(fmt_chars[i]);
                    i += 1;
                }
                let mut width_s = String::new();
                while i < fmt_chars.len() && fmt_chars[i].is_ascii_digit() {
                    width_s.push(fmt_chars[i]);
                    i += 1;
                }
                let width: usize = width_s.parse().unwrap_or(0);
                let mut precision = None;
                if i < fmt_chars.len() && fmt_chars[i] == '.' {
                    i += 1;
                    let mut prec = String::new();
                    while i < fmt_chars.len() && fmt_chars[i].is_ascii_digit() {
                        prec.push(fmt_chars[i]);
                        i += 1;
                    }
                    precision = Some(prec.parse::<usize>().unwrap_or(0));
                }
                if i >= fmt_chars.len() {
                    break;
                }
                let conv = fmt_chars[i];
                i += 1;
                let left_align = flags.contains('-');
                let zero_pad = flags.contains('0') && !left_align;
                match conv {
                    's' => {
                        let val = args.get(arg_idx).map(|s| s.as_str()).unwrap_or("");
                        arg_idx += 1;
                        let val = if let Some(p) = precision {
                            &val[..val.len().min(p)]
                        } else {
                            val
                        };
                        if left_align {
                            let _ = write!(out, "{val:<width$}");
                        } else {
                            let _ = write!(out, "{val:>width$}");
                        }
                    }
                    'd' | 'i' => {
                        let val = args
                            .get(arg_idx)
                            .and_then(|s| s.parse::<i64>().ok())
                            .unwrap_or(0);
                        arg_idx += 1;
                        if zero_pad {
                            let _ = write!(out, "{val:0>width$}");
                        } else if left_align {
                            let _ = write!(out, "{val:<width$}");
                        } else {
                            let _ = write!(out, "{val:>width$}");
                        }
                    }
                    'o' => {
                        let val = args
                            .get(arg_idx)
                            .and_then(|s| s.parse::<i64>().ok())
                            .unwrap_or(0);
                        arg_idx += 1;
                        let _ = write!(out, "{val:o}");
                    }
                    'x' => {
                        let val = args
                            .get(arg_idx)
                            .and_then(|s| s.parse::<i64>().ok())
                            .unwrap_or(0);
                        arg_idx += 1;
                        let _ = write!(out, "{val:x}");
                    }
                    'X' => {
                        let val = args
                            .get(arg_idx)
                            .and_then(|s| s.parse::<i64>().ok())
                            .unwrap_or(0);
                        arg_idx += 1;
                        let _ = write!(out, "{val:X}");
                    }
                    'c' => {
                        let val = args
                            .get(arg_idx)
                            .and_then(|s| s.chars().next())
                            .unwrap_or('\0');
                        arg_idx += 1;
                        if val != '\0' {
                            out.push(val);
                        }
                    }
                    '%' => out.push('%'),
                    'b' => {
                        let val = args.get(arg_idx).map(|s| s.as_str()).unwrap_or("");
                        arg_idx += 1;
                        out.push_str(&unescape_echo(val));
                    }
                    c => {
                        out.push('%');
                        out.push(c);
                    }
                }
            } else {
                out.push(fmt_chars[i]);
                i += 1;
            }
        }

        write_stdout(&out);
        0
    }
}

// ── Helper functions ──────────────────────────────────────────────────

/// Full POSIX test/[ implementation with compound expressions.
fn test_eval(args: &[&str]) -> i32 {
    if args.is_empty() {
        return 1;
    }
    let mut pos = 0;
    let result = test_or(args, &mut pos);
    if result { 0 } else { 1 }
}

fn test_or(args: &[&str], pos: &mut usize) -> bool {
    let mut result = test_and(args, pos);
    while *pos < args.len() && args[*pos] == "-o" {
        *pos += 1;
        let right = test_and(args, pos);
        result = result || right;
    }
    result
}

fn test_and(args: &[&str], pos: &mut usize) -> bool {
    let mut result = test_not(args, pos);
    while *pos < args.len() && args[*pos] == "-a" {
        *pos += 1;
        let right = test_not(args, pos);
        result = result && right;
    }
    result
}

fn test_not(args: &[&str], pos: &mut usize) -> bool {
    if *pos < args.len() && args[*pos] == "!" {
        *pos += 1;
        !test_primary(args, pos)
    } else {
        test_primary(args, pos)
    }
}

fn test_primary(args: &[&str], pos: &mut usize) -> bool {
    if *pos >= args.len() {
        return false;
    }

    // Parenthesized expression
    if args[*pos] == "(" {
        *pos += 1;
        let result = test_or(args, pos);
        if *pos < args.len() && args[*pos] == ")" {
            *pos += 1;
        }
        return result;
    }

    // Binary operators (check if next token is binary op)
    if *pos + 2 <= args.len() {
        let maybe_op = if *pos + 1 < args.len() {
            args[*pos + 1]
        } else {
            ""
        };
        match maybe_op {
            "=" | "==" => {
                let left = args[*pos];
                *pos += 2;
                let right = if *pos < args.len() { args[*pos] } else { "" };
                *pos += 1;
                return left == right;
            }
            "!=" => {
                let left = args[*pos];
                *pos += 2;
                let right = if *pos < args.len() { args[*pos] } else { "" };
                *pos += 1;
                return left != right;
            }
            "<" => {
                let left = args[*pos];
                *pos += 2;
                let right = if *pos < args.len() { args[*pos] } else { "" };
                *pos += 1;
                return left < right;
            }
            ">" => {
                let left = args[*pos];
                *pos += 2;
                let right = if *pos < args.len() { args[*pos] } else { "" };
                *pos += 1;
                return left > right;
            }
            "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" => {
                let left = args[*pos].parse::<i64>().unwrap_or(0);
                let op = args[*pos + 1];
                *pos += 2;
                let right = if *pos < args.len() {
                    let r = args[*pos].parse::<i64>().unwrap_or(0);
                    *pos += 1;
                    r
                } else {
                    *pos += 1;
                    0
                };
                return match op {
                    "-eq" => left == right,
                    "-ne" => left != right,
                    "-lt" => left < right,
                    "-le" => left <= right,
                    "-gt" => left > right,
                    "-ge" => left >= right,
                    _ => false,
                };
            }
            "-nt" | "-ot" | "-ef" => {
                // File comparison operators
                let left = args[*pos];
                *pos += 2;
                let right = if *pos < args.len() {
                    let r = args[*pos];
                    *pos += 1;
                    r
                } else {
                    *pos += 1;
                    ""
                };
                let lm = std::fs::metadata(left).ok();
                let rm = std::fs::metadata(right).ok();
                return match maybe_op {
                    "-nt" => match (lm, rm) {
                        (Some(l), Some(r)) => l.modified().ok() > r.modified().ok(),
                        (Some(_), None) => true,
                        _ => false,
                    },
                    "-ot" => match (lm, rm) {
                        (Some(l), Some(r)) => l.modified().ok() < r.modified().ok(),
                        (None, Some(_)) => true,
                        _ => false,
                    },
                    "-ef" => {
                        use std::os::unix::fs::MetadataExt;
                        match (lm, rm) {
                            (Some(l), Some(r)) => l.dev() == r.dev() && l.ino() == r.ino(),
                            _ => false,
                        }
                    }
                    _ => false,
                };
            }
            _ => {}
        }
    }

    // Unary operators — only match if there's a following operand
    // (and the operand isn't a closing paren or binary op)
    let op = args[*pos];
    let has_operand = *pos + 1 < args.len()
        && !matches!(
            args[*pos + 1],
            ")" | "-a" | "-o" | "=" | "!=" | "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge"
        );
    match op {
        "-n" if has_operand => {
            *pos += 1;
            let s = args[*pos];
            *pos += 1;
            !s.is_empty()
        }
        "-z" if has_operand => {
            *pos += 1;
            let s = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            s.is_empty()
        }
        "-e" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            std::path::Path::new(p).exists()
        }
        "-f" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            std::path::Path::new(p).is_file()
        }
        "-d" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            std::path::Path::new(p).is_dir()
        }
        "-L" | "-h" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            std::path::Path::new(p)
                .symlink_metadata()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
        }
        "-r" | "-w" | "-x" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            use std::os::unix::fs::MetadataExt;
            let uid = unsafe { sys::getuid() };
            match std::fs::metadata(p) {
                Ok(m) => {
                    let mode = m.mode();
                    let is_owner = m.uid() == uid;
                    match op {
                        "-r" => {
                            if is_owner {
                                mode & 0o400 != 0
                            } else {
                                mode & 0o004 != 0
                            }
                        }
                        "-w" => {
                            if is_owner {
                                mode & 0o200 != 0
                            } else {
                                mode & 0o002 != 0
                            }
                        }
                        "-x" => {
                            if is_owner {
                                mode & 0o100 != 0
                            } else {
                                mode & 0o001 != 0
                            }
                        }
                        _ => false,
                    }
                }
                Err(_) => false,
            }
        }
        "-s" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            std::fs::metadata(p).map(|m| m.len() > 0).unwrap_or(false)
        }
        "-t" if has_operand => {
            *pos += 1;
            let fd = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                "1"
            };
            let fd = fd.parse::<i32>().unwrap_or(1);
            unsafe { sys::isatty(fd) != 0 }
        }
        "-p" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            use std::os::unix::fs::FileTypeExt;
            std::fs::metadata(p)
                .map(|m| m.file_type().is_fifo())
                .unwrap_or(false)
        }
        "-b" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            use std::os::unix::fs::FileTypeExt;
            std::fs::metadata(p)
                .map(|m| m.file_type().is_block_device())
                .unwrap_or(false)
        }
        "-c" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            use std::os::unix::fs::FileTypeExt;
            std::fs::metadata(p)
                .map(|m| m.file_type().is_char_device())
                .unwrap_or(false)
        }
        "-S" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            use std::os::unix::fs::FileTypeExt;
            std::fs::metadata(p)
                .map(|m| m.file_type().is_socket())
                .unwrap_or(false)
        }
        "-u" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(p)
                .map(|m| m.mode() & 0o4000 != 0)
                .unwrap_or(false)
        }
        "-g" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(p)
                .map(|m| m.mode() & 0o2000 != 0)
                .unwrap_or(false)
        }
        "-k" if has_operand => {
            *pos += 1;
            let p = if *pos < args.len() {
                let r = args[*pos];
                *pos += 1;
                r
            } else {
                ""
            };
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(p)
                .map(|m| m.mode() & 0o1000 != 0)
                .unwrap_or(false)
        }
        _ => {
            // Bare string: true if non-empty
            *pos += 1;
            !op.is_empty()
        }
    }
}

fn is_builtin(name: &str) -> bool {
    matches!(
        name,
        ":" | "true"
            | "false"
            | "echo"
            | "cd"
            | "pwd"
            | "exit"
            | "return"
            | "break"
            | "continue"
            | "export"
            | "readonly"
            | "unset"
            | "set"
            | "shift"
            | "eval"
            | "."
            | "source"
            | "test"
            | "["
            | "read"
            | "local"
            | "exec"
            | "command"
            | "type"
            | "wait"
            | "trap"
            | "umask"
            | "getopts"
            | "printf"
    )
}

fn which(name: &str) -> Result<String, ()> {
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        let candidate = format!("{dir}/{name}");
        if std::path::Path::new(&candidate).is_file() {
            return Ok(candidate);
        }
    }
    Err(())
}

/// Write a string directly to fd 1, bypassing Rust's stdout buffering.
/// Necessary for correct behavior in command substitution (fd 1 may be a pipe).
fn write_stdout(s: &str) {
    unsafe {
        sys::write(1, s.as_ptr() as *const _, s.len());
    }
}

fn unescape_echo(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('t') => result.push('\t'),
                Some('\\') => result.push('\\'),
                Some('a') => result.push('\x07'),
                Some('b') => result.push('\x08'),
                Some('f') => result.push('\x0c'),
                Some('r') => result.push('\r'),
                Some('v') => result.push('\x0b'),
                Some(c) => {
                    result.push('\\');
                    result.push(c);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(ch);
        }
    }
    result
}

/// exec family helpers
mod exec {
    use std::ffi::CString;

    pub fn execvp(cmd: &str, args: &[String]) -> std::io::Error {
        let c_cmd = CString::new(cmd.as_bytes()).unwrap();
        let c_args: Vec<CString> = args
            .iter()
            .map(|a| CString::new(a.as_bytes()).unwrap())
            .collect();
        let c_argv: Vec<*const i8> = c_args
            .iter()
            .map(|a| a.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();

        unsafe {
            crate::sys::execvp(c_cmd.as_ptr(), c_argv.as_ptr());
        }
        std::io::Error::last_os_error()
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
        assert_eq!(shell.exit_status, 1);
        shell.run_script("true");
        assert_eq!(shell.exit_status, 0);
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
