use std::collections::HashMap;
use std::io::Read;
use std::os::unix::io::{FromRawFd, RawFd};

use crate::ast::*;
use crate::error::{ExitStatus, ShellError, Span};
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
    pub exit_status: ExitStatus,
    /// Shell's PID ($$)
    pub pid: u32,
    /// Number of nested loops (for break/continue counting)
    pub(crate) loop_depth: usize,
    /// Shell options
    pub opts: ShellOpts,
    /// Trap handlers: signal name → command string
    pub traps: HashMap<String, String>,
    /// True when evaluating a condition (if test, while cond, && / || operands).
    /// Suppresses set -e (errexit). Mirrors dash's EV_TESTED flag.
    pub(crate) tested: bool,
    /// True when executing inside a forked child (pipeline stage, command subst).
    /// Subshells skip the extra fork to avoid pipe fd leaks.
    pub(crate) in_forked_child: bool,
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

impl expand::ShellExpand for Shell {
    fn vars(&self) -> &Variables { &self.vars }
    fn vars_mut(&mut self) -> &mut Variables { &mut self.vars }
    fn exit_status(&self) -> ExitStatus { self.exit_status }
    fn pid(&self) -> u32 { self.pid }
    fn command_subst(&mut self, cmd: &Command) -> crate::error::Result<String> {
        self.command_subst(cmd)
    }
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
            exit_status: ExitStatus::SUCCESS,
            pid: std::process::id(),
            loop_depth: 0,
            opts: ShellOpts::default(),
            traps: HashMap::new(),
            tested: false,
            in_forked_child: false,
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
                    return n.code();
                }
                Err(e) => {
                    eprintln!("epsh: {e}");
                    self.exit_status = ExitStatus::MISUSE;
                    // Non-interactive shell: most errors are fatal
                    self.run_exit_trap();
                    return self.exit_status.code();
                }
            }
        }

        self.run_exit_trap();
        self.exit_status.code()
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
        if self.opts.errexit && is_leaf && !self.tested && !status.success() {
            return Err(ShellError::Exit(status));
        }

        Ok(status)
    }

    fn eval_command_inner(&mut self, cmd: &Command) -> crate::error::Result<ExitStatus> {
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
                // bang (!) suppresses errexit, like dash's EV_TESTED
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
            for assign in assigns {
                let value = self.expand_string(&assign.value)?;
                self.vars
                    .set(&assign.name, &value)
                    .map_err(|msg| ShellError::Runtime { msg, span })?;
            }
            return Ok(self.exit_status);
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

    /// Evaluate a function call.
    fn eval_function(
        &mut self,
        body: &Command,
        args: &[String],
        assigns: &[Assignment],
        redirs: &[Redir],
        _span: Span,
    ) -> crate::error::Result<ExitStatus> {
        // Save and set positional parameters
        let saved_positional = self.vars.positional.clone();
        self.vars.positional = args[1..].to_vec();

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

    /// Execute an external command.
    pub(crate) fn eval_external(
        &mut self,
        args: &[String],
        assigns: &[Assignment],
        redirs: &[Redir],
        _span: Span,
    ) -> crate::error::Result<ExitStatus> {
        let saved = self.setup_redirections(redirs)?;

        let mut cmd = std::process::Command::new(&args[0]);
        cmd.args(&args[1..]);

        // Set environment: exported vars + temporary assignments
        cmd.env_clear();
        for (k, v) in self.vars.exported_env() {
            cmd.env(&k, &v);
        }
        for assign in assigns {
            let value = self.expand_string(&assign.value)?;
            cmd.env(&assign.name, &value);
        }

        // Inherit stdio (redirections are already applied to our fds)
        let result = match cmd.status() {
            Ok(status) => {
                let code = status.code().unwrap_or(128);
                Ok(ExitStatus::from(code))
            }
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    eprintln!("{}: not found", args[0]);
                    Ok(ExitStatus::NOT_FOUND)
                } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                    eprintln!("{}: permission denied", args[0]);
                    Ok(ExitStatus::NOT_EXECUTABLE)
                } else {
                    Err(ShellError::Io(e))
                }
            }
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
                    // Child process — mark as forked so subshells don't double-fork
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

                    // Execute command
                    let status = match self.eval_command(cmd) {
                        Ok(s) => s,
                        Err(ShellError::Exit(n)) => n,
                        Err(_) => ExitStatus::FAILURE,
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
        let mut last_status = ExitStatus::SUCCESS;
        for (i, &pid) in children.iter().enumerate() {
            let mut status = 0i32;
            unsafe {
                sys::waitpid(pid, &mut status, 0);
            }
            if i == children.len() - 1 {
                last_status = ExitStatus::from_wait(status);
            }
        }

        Ok(last_status)
    }

    /// Execute a command in a subshell (fork).
    fn eval_in_subshell(&mut self, body: &Command, redirs: &[Redir]) -> crate::error::Result<ExitStatus> {
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
            let mut status = 0i32;
            sys::waitpid(pid, &mut status, 0);
            Ok(ExitStatus::from_wait(status))
        }
    }

    /// Execute a command in the background.
    fn eval_background(&mut self, cmd: &Command, redirs: &[Redir]) -> crate::error::Result<ExitStatus> {
        unsafe {
            let pid = sys::fork();
            if pid < 0 {
                return Err(ShellError::Io(std::io::Error::last_os_error()));
            }

            if pid == 0 {
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

            // Don't wait — background process
            Ok(ExitStatus::SUCCESS)
        }
    }

    /// Execute a command substitution and return its output.
    pub fn command_subst(&mut self, cmd: &Command) -> crate::error::Result<String> {
        // $(<file) optimization: read file directly without forking
        if let Command::Simple { assigns, args, redirs, .. } = cmd
            && assigns.is_empty() && args.is_empty() && redirs.len() == 1
            && let RedirKind::Input(ref word) = redirs[0].kind
        {
                    let filename = self.expand_string(word)?;
                    match std::fs::read_to_string(&filename) {
                        Ok(mut content) => {
                            // Remove trailing newlines (like command substitution)
                            while content.ends_with('\n') {
                                content.pop();
                            }
                            self.exit_status = ExitStatus::SUCCESS;
                            return Ok(content);
                        }
                        Err(e) => {
                            eprintln!("epsh: {filename}: {e}");
                            self.exit_status = ExitStatus::FAILURE;
                            return Err(ShellError::Runtime {
                                msg: format!("{filename}: {e}"),
                                span: redirs[0].span,
                            });
                        }
                    }
        }

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

            let mut output = String::new();
            let mut read_file = std::fs::File::from_raw_fd(fds[0]);
            let _ = read_file.read_to_string(&mut output);

            let mut status = 0i32;
            sys::waitpid(pid, &mut status, 0);
            if sys::wifexited(status) {
                self.exit_status = ExitStatus::from(sys::wexitstatus(status));
            }

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
