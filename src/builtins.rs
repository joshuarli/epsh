use std::os::unix::io::AsRawFd;

use crate::ast::*;
use crate::error::{ExitStatus, ShellError, Span};
use crate::eval::Shell;
use crate::parser::Parser;
use crate::sys;
use crate::test_cmd::test_eval;

impl Shell {
    /// Try to run a builtin command. Returns None if not a builtin.
    pub(crate) fn try_builtin(
        &mut self,
        name: &str,
        args: &[String],
        _assigns: &[Assignment],
        redirs: &[Redir],
        span: Span,
    ) -> crate::error::Result<Option<ExitStatus>> {
        let status = match name {
            ":" | "true" => Some(ExitStatus::SUCCESS),
            "false" => Some(ExitStatus::FAILURE),
            "echo" => Some(self.builtin_echo(args)),
            "cd" => Some(self.builtin_cd(args)),
            "pwd" => match std::env::current_dir() {
                Ok(p) => {
                    write_stdout(&format!("{}\n", p.display()));
                    Some(ExitStatus::SUCCESS)
                }
                Err(e) => {
                    eprintln!("pwd: {e}");
                    Some(ExitStatus::FAILURE)
                }
            },
            "exit" => {
                let code = args
                    .get(1)
                    .and_then(|s| s.parse::<i32>().ok())
                    .map(ExitStatus)
                    .unwrap_or(self.exit_status);
                return Err(ShellError::Exit(code));
            }
            "return" => {
                let code = args
                    .get(1)
                    .and_then(|s| s.parse::<i32>().ok())
                    .map(ExitStatus)
                    .unwrap_or(self.exit_status);
                return Err(ShellError::Return(code));
            }
            "break" => {
                if let Some(arg) = args.get(1) {
                    match arg.parse::<usize>() {
                        Ok(0) => {
                            eprintln!("break: Illegal number: {arg}");
                            return Err(ShellError::Exit(ExitStatus::FAILURE));
                        }
                        Ok(n) => {
                            if self.loop_depth == 0 {
                                Some(ExitStatus::SUCCESS)
                            } else {
                                return Err(ShellError::Break(n.min(self.loop_depth)));
                            }
                        }
                        Err(_) => {
                            eprintln!("break: Illegal number: {arg}");
                            return Err(ShellError::Exit(ExitStatus::FAILURE));
                        }
                    }
                } else if self.loop_depth == 0 {
                    Some(ExitStatus::SUCCESS)
                } else {
                    return Err(ShellError::Break(1));
                }
            }
            "continue" => {
                if let Some(arg) = args.get(1) {
                    match arg.parse::<usize>() {
                        Ok(0) => {
                            eprintln!("continue: Illegal number: {arg}");
                            return Err(ShellError::Exit(ExitStatus::FAILURE));
                        }
                        Ok(n) => {
                            if self.loop_depth == 0 {
                                Some(ExitStatus::SUCCESS)
                            } else {
                                return Err(ShellError::Continue(n.min(self.loop_depth)));
                            }
                        }
                        Err(_) => {
                            eprintln!("continue: Illegal number: {arg}");
                            return Err(ShellError::Exit(ExitStatus::FAILURE));
                        }
                    }
                } else if self.loop_depth == 0 {
                    Some(ExitStatus::SUCCESS)
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

    fn builtin_echo(&self, args: &[String]) -> ExitStatus {
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
        ExitStatus::SUCCESS
    }

    fn builtin_cd(&mut self, args: &[String]) -> ExitStatus {
        let dir = if args.len() > 1 {
            args[1].as_str()
        } else {
            match self.vars.get("HOME") {
                Some(h) => h,
                None => {
                    eprintln!("cd: HOME not set");
                    return ExitStatus::FAILURE;
                }
            }
        };

        match std::env::set_current_dir(dir) {
            Ok(()) => {
                if let Ok(pwd) = std::env::current_dir() {
                    let _ = self.vars.set("PWD", &pwd.to_string_lossy());
                }
                ExitStatus::SUCCESS
            }
            Err(e) => {
                eprintln!("cd: {dir}: {e}");
                ExitStatus::FAILURE
            }
        }
    }

    fn builtin_export(&mut self, args: &[String]) -> ExitStatus {
        if args.len() <= 1 {
            // Print all exported variables
            for (k, v) in self.vars.exported_env() {
                write_stdout(&format!("export {k}=\"{v}\"\n"));
            }
            return ExitStatus::SUCCESS;
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
        ExitStatus::SUCCESS
    }

    fn builtin_readonly(&mut self, args: &[String]) -> ExitStatus {
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
        ExitStatus::SUCCESS
    }

    fn builtin_unset(&mut self, args: &[String]) -> ExitStatus {
        let mut status = ExitStatus::SUCCESS;
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
                status = ExitStatus::FAILURE;
            }
        }
        status
    }

    fn builtin_set(&mut self, args: &[String]) -> ExitStatus {
        if args.len() <= 1 {
            return ExitStatus::SUCCESS;
        }

        let mut i = 1;
        while i < args.len() {
            let arg = &args[i];
            if arg == "--" {
                i += 1;
                // Remaining args become positional parameters
                self.vars.positional = args[i..].to_vec();
                return ExitStatus::SUCCESS;
            } else if arg.starts_with('-') || arg.starts_with('+') {
                let enable = arg.starts_with('-');
                for ch in arg[1..].chars() {
                    match ch {
                        'e' => self.opts.errexit = enable,
                        'u' => self.opts.nounset = enable,
                        'x' => self.opts.xtrace = enable,
                        _ => {
                            eprintln!("set: unknown option: -{ch}");
                            return ExitStatus::FAILURE;
                        }
                    }
                }
                i += 1;
            } else {
                // Positional parameters
                self.vars.positional = args[i..].to_vec();
                return ExitStatus::SUCCESS;
            }
        }
        ExitStatus::SUCCESS
    }

    fn builtin_shift(&mut self, args: &[String]) -> ExitStatus {
        let n = args
            .get(1)
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(1);
        if n > self.vars.positional.len() {
            eprintln!("shift: can't shift that many");
            return ExitStatus::FAILURE;
        }
        self.vars.positional = self.vars.positional[n..].to_vec();
        ExitStatus::SUCCESS
    }

    fn builtin_eval(&mut self, args: &[String]) -> crate::error::Result<ExitStatus> {
        if args.len() <= 1 {
            return Ok(ExitStatus::SUCCESS);
        }
        let script = args[1..].join(" ");
        // eval must propagate control flow (break, continue, return, exit)
        // unlike run_script which catches them
        let mut parser = Parser::new(&script);
        let program = match parser.parse() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("epsh: {e}");
                return Ok(ExitStatus::MISUSE);
            }
        };
        let mut status = ExitStatus::SUCCESS;
        for cmd in &program.commands {
            status = self.eval_command(cmd)?;
            self.exit_status = status;
        }
        Ok(status)
    }

    fn builtin_dot(&mut self, args: &[String], _span: Span) -> crate::error::Result<ExitStatus> {
        if args.len() <= 1 {
            eprintln!(".: filename argument required");
            return Err(ShellError::Exit(ExitStatus::MISUSE));
        }
        let filename = &args[1];
        let content = match std::fs::read_to_string(filename) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(".: {filename}: {e}");
                // . is a special builtin — file not found is fatal
                return Err(ShellError::Exit(ExitStatus::NOT_FOUND));
            }
        };
        // dot runs in the current shell (not a subshell), and must
        // propagate break/continue/return/exit
        let mut parser = Parser::new(&content);
        let program = match parser.parse() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("epsh: {e}");
                return Ok(ExitStatus::MISUSE);
            }
        };
        let mut status = ExitStatus::SUCCESS;
        for cmd in &program.commands {
            status = self.eval_command(cmd)?;
            self.exit_status = status;
        }
        Ok(status)
    }

    fn builtin_test(&self, args: &[String]) -> ExitStatus {
        let args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let args = if args[0] == "[" {
            if args.last() != Some(&"]") {
                eprintln!("[: missing ]");
                return ExitStatus::MISUSE;
            }
            &args[1..args.len() - 1]
        } else {
            &args[1..]
        };
        test_eval(args)
    }

    fn builtin_read(&mut self, args: &[String]) -> ExitStatus {
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
            return ExitStatus::FAILURE;
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
        ExitStatus::SUCCESS
    }

    fn builtin_local(&mut self, args: &[String]) -> ExitStatus {
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
        ExitStatus::SUCCESS
    }

    fn builtin_exec(
        &mut self,
        args: &[String],
        redirs: &[Redir],
        _span: Span,
    ) -> crate::error::Result<ExitStatus> {
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
            return Ok(ExitStatus::SUCCESS);
        }

        // exec with command — replace process
        let err = exec::execvp(&args[0], args);
        eprintln!("exec: {}: {err}", args[0]);
        Ok(ExitStatus::NOT_EXECUTABLE)
    }

    fn builtin_command(&mut self, args: &[String], span: Span) -> crate::error::Result<ExitStatus> {
        if args.len() <= 1 {
            return Ok(ExitStatus::SUCCESS);
        }
        // Skip -v/-V flags
        let mut i = 1;
        while i < args.len() && args[i].starts_with('-') {
            i += 1;
        }
        if i >= args.len() {
            return Ok(ExitStatus::SUCCESS);
        }
        let new_args: Vec<String> = args[i..].to_vec();

        // `command` bypasses functions but NOT builtins
        if let Some(status) = self.try_builtin(&new_args[0], &new_args, &[], &[], span)? {
            Ok(status)
        } else {
            self.eval_external(&new_args, &[], &[], span)
        }
    }

    fn builtin_type(&self, args: &[String]) -> ExitStatus {
        let mut status = ExitStatus::SUCCESS;
        for name in &args[1..] {
            if self.functions.contains_key(name.as_str()) {
                write_stdout(&format!("{name} is a function\n"));
            } else if is_builtin(name) {
                write_stdout(&format!("{name} is a shell builtin\n"));
            } else if let Ok(path) = which(name) {
                write_stdout(&format!("{name} is {path}\n"));
            } else {
                eprintln!("{name}: not found");
                status = ExitStatus::FAILURE;
            }
        }
        status
    }

    fn builtin_wait(&mut self, _args: &[String]) -> ExitStatus {
        // Wait for all background children
        loop {
            let mut status = 0i32;
            let pid = unsafe { sys::waitpid(-1, &mut status, 0) };
            if pid <= 0 {
                break;
            }
        }
        ExitStatus::SUCCESS
    }

    fn builtin_trap(&mut self, args: &[String]) -> ExitStatus {
        if args.len() <= 1 {
            // Print current traps
            for (sig, action) in &self.traps {
                write_stdout(&format!("trap -- '{}' {}\n", action, sig));
            }
            return ExitStatus::SUCCESS;
        }
        if args.len() == 2 {
            // trap '' SIG or trap - SIG
            if args[1] == "-" {
                // Reset all traps
                self.traps.clear();
            }
            return ExitStatus::SUCCESS;
        }
        let action = &args[1];
        for sig_name in &args[2..] {
            if action == "-" {
                self.traps.remove(sig_name.as_str());
            } else {
                self.traps.insert(sig_name.clone(), action.clone());
            }
        }
        ExitStatus::SUCCESS
    }

    fn builtin_umask(&self, args: &[String]) -> ExitStatus {
        if args.len() <= 1 {
            let mask = unsafe { sys::umask(0) };
            unsafe {
                sys::umask(mask);
            }
            write_stdout(&format!("{mask:04o}\n"));
            return ExitStatus::SUCCESS;
        }
        if let Ok(mask) = u32::from_str_radix(&args[1], 8) {
            unsafe {
                sys::umask(mask as libc::mode_t);
            }
            ExitStatus::SUCCESS
        } else {
            eprintln!("umask: {}: invalid mask", args[1]);
            ExitStatus::FAILURE
        }
    }

    fn builtin_getopts(&mut self, args: &[String]) -> ExitStatus {
        if args.len() < 3 {
            eprintln!("getopts: usage: getopts optstring name [arg ...]");
            return ExitStatus::MISUSE;
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
            return ExitStatus::FAILURE;
        }

        let arg = &argv[optind - 1];
        if !arg.starts_with('-') || arg == "-" || arg == "--" {
            let _ = self.vars.set(name, "?");
            return ExitStatus::FAILURE;
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
                        return ExitStatus::SUCCESS;
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
                ExitStatus::SUCCESS
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
                ExitStatus::SUCCESS
            }
        }
    }

    fn builtin_printf(&self, args: &[String]) -> ExitStatus {
        if args.len() < 2 {
            eprintln!("printf: usage: printf format [arguments]");
            return ExitStatus::FAILURE;
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
        ExitStatus::SUCCESS
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
