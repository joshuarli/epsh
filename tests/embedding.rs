use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use epsh::ast::Program;
use epsh::builtins::{BUILTIN_NAMES, is_builtin};
use epsh::error::ExitStatus;
use epsh::eval::Shell;
use epsh::parser::Parser;

fn parse(src: &str) -> Program {
    Parser::new(src).parse().unwrap()
}

mod builder {
    use super::*;

    #[test]
    fn default_builder() {
        let shell = Shell::builder().build();
        assert_eq!(shell.exit_status(), ExitStatus::SUCCESS);
    }

    #[test]
    fn builder_with_cwd() {
        let mut shell = Shell::builder().cwd(PathBuf::from("/tmp")).build();
        let status = shell.run_program(&parse("pwd"));
        assert_eq!(status, ExitStatus::SUCCESS);
    }

    #[test]
    fn builder_with_options() {
        let shell = Shell::builder()
            .errexit(true)
            .nounset(true)
            .pipefail(true)
            .build();
        assert!(shell.opts().errexit);
        assert!(shell.opts().nounset);
        assert!(shell.opts().pipefail);
    }

    #[test]
    fn builder_env_clear() {
        let mut shell = Shell::builder().env_clear().build();
        // PATH should not be inherited
        let status = shell.run_program(&parse("echo ${PATH-unset}"));
        // Can't easily check output without sink, just verify it runs
        assert_eq!(status, ExitStatus::SUCCESS);
    }

    #[test]
    fn builder_with_sinks() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .stdout_sink(stdout.clone())
            .stderr_sink(stderr.clone())
            .build();
        shell.run_program(&parse("echo hello"));
        let out = String::from_utf8_lossy(&stdout.lock().unwrap()).to_string();
        assert_eq!(out, "hello\n");
    }

    #[test]
    fn builder_chained() {
        let cancel = Arc::new(AtomicBool::new(false));
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .cwd(PathBuf::from("/tmp"))
            .errexit(true)
            .cancel_flag(cancel)
            .stdout_sink(stdout.clone())
            .timeout(Duration::from_secs(60))
            .build();
        shell.run_program(&parse("echo ok"));
        let out = String::from_utf8_lossy(&stdout.lock().unwrap()).to_string();
        assert_eq!(out, "ok\n");
    }
}

mod parse_then_execute {
    use super::*;

    #[test]
    fn basic_parse_execute() {
        let program = parse("echo hello");
        let mut shell = Shell::new();
        let status = shell.run_program(&program);
        assert_eq!(status, ExitStatus::SUCCESS);
    }

    #[test]
    fn reuse_parsed_program() {
        let program = parse("x=$((x + 1)); echo $x");
        let mut shell = Shell::new();
        let _ = shell.set_var("x", "0");
        shell.run_program(&program);
        assert_eq!(shell.get_var("x"), Some("1"));
        shell.run_program(&program);
        assert_eq!(shell.get_var("x"), Some("2"));
    }

    #[test]
    fn parse_error_doesnt_crash() {
        let result = Parser::new("if; then; fi; (((").parse();
        assert!(result.is_err());
    }

    #[test]
    fn inspect_ast() {
        let program = parse("echo hello world");
        assert_eq!(program.commands.len(), 1);
    }
}

mod cancellation {
    use super::*;

    #[test]
    fn cancel_stops_execution() {
        let cancel = Arc::new(AtomicBool::new(false));
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .cancel_flag(cancel.clone())
            .stdout_sink(stdout.clone())
            .build();

        // Set cancel before running — should abort immediately
        cancel.store(true, Ordering::Relaxed);
        let status = shell.run_program(&parse("echo should-not-appear"));
        assert_eq!(status.code(), 130); // SIGINT
        let out = stdout.lock().unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn cancel_during_loop() {
        let cancel = Arc::new(AtomicBool::new(false));
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let cancel2 = cancel.clone();

        let mut shell = Shell::builder()
            .cancel_flag(cancel)
            .stdout_sink(stdout.clone())
            .build();

        // Cancel after a short delay
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            cancel2.store(true, Ordering::Relaxed);
        });

        let status = shell.run_program(&parse("while true; do echo x; done"));
        assert_eq!(status.code(), 130);
    }
}

mod timeout {
    use super::*;

    #[test]
    fn timeout_stops_execution() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .stdout_sink(stdout.clone())
            .timeout(Duration::from_millis(50))
            .build();
        let status = shell.run_program(&parse("while true; do echo x; done"));
        assert_eq!(status.code(), 130);
    }

    #[test]
    fn no_timeout_if_fast() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .stdout_sink(stdout.clone())
            .timeout(Duration::from_secs(10))
            .build();
        let status = shell.run_program(&parse("echo hello"));
        assert_eq!(status, ExitStatus::SUCCESS);
        let out = String::from_utf8_lossy(&stdout.lock().unwrap()).to_string();
        assert_eq!(out, "hello\n");
    }

    #[test]
    fn set_timeout_after_construction() {
        let mut shell = Shell::new();
        shell.set_timeout(Duration::from_millis(50));
        let status = shell.run_program(&parse("while true; do :; done"));
        assert_eq!(status.code(), 130);
    }
}

mod output_sinks {
    use super::*;

    #[test]
    fn capture_stdout() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout.clone()).build();
        shell.run_program(&parse("echo hello; echo world"));
        let out = String::from_utf8_lossy(&stdout.lock().unwrap()).to_string();
        assert_eq!(out, "hello\nworld\n");
    }

    #[test]
    fn capture_stderr() {
        let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stderr_sink(stderr.clone()).build();
        // Shell error messages go through write_err → stderr_sink
        shell.run_program(&parse("nonexistent_cmd_xyz"));
        let err = String::from_utf8_lossy(&stderr.lock().unwrap()).to_string();
        assert!(err.contains("not found"), "stderr: {err}");
    }

    #[test]
    fn capture_external_command() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout.clone()).build();
        shell.run_program(&parse("/bin/echo external"));
        let out = String::from_utf8_lossy(&stdout.lock().unwrap()).to_string();
        assert_eq!(out, "external\n");
    }

    #[test]
    fn separate_stdout_stderr() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stderr = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder()
            .stdout_sink(stdout.clone())
            .stderr_sink(stderr.clone())
            .build();
        // echo goes to stdout_sink, error message goes to stderr_sink
        shell.run_program(&parse("echo out; nonexistent_cmd_xyz"));
        assert_eq!(
            String::from_utf8_lossy(&stdout.lock().unwrap()).as_ref(),
            "out\n"
        );
        let err = String::from_utf8_lossy(&stderr.lock().unwrap()).to_string();
        assert!(err.contains("not found"), "stderr: {err}");
    }
}

mod cwd_isolation {
    use super::*;

    #[test]
    fn shells_have_independent_cwd() {
        let stdout1 = Arc::new(Mutex::new(Vec::<u8>::new()));
        let stdout2 = Arc::new(Mutex::new(Vec::<u8>::new()));

        let mut shell1 = Shell::builder()
            .cwd(PathBuf::from("/tmp"))
            .stdout_sink(stdout1.clone())
            .build();
        let mut shell2 = Shell::builder()
            .cwd(PathBuf::from("/"))
            .stdout_sink(stdout2.clone())
            .build();

        shell1.run_program(&parse("pwd"));
        shell2.run_program(&parse("pwd"));

        let out1 = String::from_utf8_lossy(&stdout1.lock().unwrap()).to_string();
        let out2 = String::from_utf8_lossy(&stdout2.lock().unwrap()).to_string();
        // macOS: /tmp may resolve to /private/tmp
        assert!(out1.trim() == "/tmp" || out1.trim() == "/private/tmp");
        assert_eq!(out2.trim(), "/");
    }

    #[test]
    fn cd_updates_shell_cwd() {
        let mut shell = Shell::builder().cwd(PathBuf::from("/")).build();
        shell.run_program(&parse("cd /tmp"));
        // macOS: /tmp → /private/tmp
        let cwd = shell.cwd().to_string_lossy().to_string();
        assert!(cwd == "/tmp" || cwd == "/private/tmp");
    }
}

mod builtin_list {
    use super::*;

    #[test]
    fn builtin_names_contains_core() {
        assert!(BUILTIN_NAMES.contains(&"echo"));
        assert!(BUILTIN_NAMES.contains(&"cd"));
        assert!(BUILTIN_NAMES.contains(&"export"));
        assert!(BUILTIN_NAMES.contains(&"test"));
        assert!(BUILTIN_NAMES.contains(&":"));
    }

    #[test]
    fn is_builtin_works() {
        assert!(is_builtin("echo"));
        assert!(is_builtin("["));
        assert!(!is_builtin("ls"));
        assert!(!is_builtin("git"));
    }

    #[test]
    fn builtin_names_matches_try_builtin() {
        // Every name in BUILTIN_NAMES should be recognized by try_builtin
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout).build();
        for &name in BUILTIN_NAMES {
            // Just verify they don't crash — some need args. Redirect stdin from
            // /dev/null so interactive builtins like `read` get immediate EOF.
            let _ = shell.run_program(&parse(&format!("{name} --help </dev/null 2>/dev/null")));
        }
    }
}

mod external_handler {
    use super::*;
    use epsh::eval::ExternalHandler;

    #[test]
    fn handler_receives_args() {
        let captured_args = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let args_ref = captured_args.clone();
        let handler: ExternalHandler = Box::new(move |args, _env| {
            args_ref.lock().unwrap().push(args.to_vec());
            Ok(ExitStatus::SUCCESS)
        });
        let mut shell = Shell::builder().external_handler(handler).build();
        shell.run_program(&parse("nonexistent_cmd foo bar"));
        let calls = captured_args.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], vec!["nonexistent_cmd", "foo", "bar"]);
    }

    #[test]
    fn handler_receives_env_pairs() {
        let captured_env = Arc::new(Mutex::new(Vec::<Vec<(String, String)>>::new()));
        let env_ref = captured_env.clone();
        let handler: ExternalHandler = Box::new(move |_args, env| {
            env_ref.lock().unwrap().push(env.to_vec());
            Ok(ExitStatus::SUCCESS)
        });
        let mut shell = Shell::builder().external_handler(handler).build();
        shell.run_program(&parse("FOO=bar BAZ=qux mycmd"));
        let calls = captured_env.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains(&("FOO".into(), "bar".into())));
        assert!(calls[0].contains(&("BAZ".into(), "qux".into())));
    }

    #[test]
    fn handler_not_called_for_builtins() {
        let call_count = Arc::new(Mutex::new(0));
        let count_ref = call_count.clone();
        let handler: ExternalHandler = Box::new(move |_args, _env| {
            *count_ref.lock().unwrap() += 1;
            Ok(ExitStatus::SUCCESS)
        });
        let mut shell = Shell::builder().external_handler(handler).build();
        shell.run_program(&parse("echo hello; true; false; : ; pwd"));
        assert_eq!(*call_count.lock().unwrap(), 0);
    }

    #[test]
    fn handler_exit_status_propagates() {
        let handler: ExternalHandler = Box::new(|_args, _env| Ok(ExitStatus::from(42)));
        let mut shell = Shell::builder().external_handler(handler).build();
        let status = shell.run_program(&parse("mycmd"));
        assert_eq!(status.code(), 42);
    }
}

mod variables {
    use super::*;

    #[test]
    fn set_get_var() {
        let mut shell = Shell::new();
        let _ = shell.set_var("MY_VAR", "hello");
        assert_eq!(shell.get_var("MY_VAR"), Some("hello"));
    }

    #[test]
    fn var_persists_across_commands() {
        let stdout = Arc::new(Mutex::new(Vec::<u8>::new()));
        let mut shell = Shell::builder().stdout_sink(stdout.clone()).build();
        shell.run_program(&parse("X=42"));
        shell.run_program(&parse("echo $X"));
        let out = String::from_utf8_lossy(&stdout.lock().unwrap()).to_string();
        assert_eq!(out, "42\n");
    }

    #[test]
    fn exit_status_accessible() {
        let mut shell = Shell::new();
        shell.run_program(&parse("false"));
        assert_eq!(shell.exit_status(), ExitStatus::FAILURE);
        shell.run_program(&parse("true"));
        assert_eq!(shell.exit_status(), ExitStatus::SUCCESS);
    }
}
