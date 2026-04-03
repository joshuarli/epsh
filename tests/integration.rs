use std::process::Command;

fn epsh() -> Command {
    let bin = env!("CARGO_BIN_EXE_epsh");
    Command::new(bin)
}

/// Run a script via `epsh -c` and return (stdout, stderr, exit_code).
fn run(script: &str) -> (String, String, i32) {
    let out = epsh()
        .arg("-c")
        .arg(script)
        .output()
        .expect("failed to execute epsh");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(128),
    )
}

fn assert_output(script: &str, expected_stdout: &str) {
    let (stdout, stderr, code) = run(script);
    assert_eq!(
        stdout, expected_stdout,
        "script: {script:?}\nstderr: {stderr}\nexit code: {code}"
    );
    assert_eq!(code, 0, "script: {script:?}\nstderr: {stderr}");
}

fn assert_status(script: &str, expected_code: i32) {
    let (_, _, code) = run(script);
    assert_eq!(code, expected_code, "script: {script:?}");
}

fn assert_stdout_status(script: &str, expected_stdout: &str, expected_code: i32) {
    let (stdout, stderr, code) = run(script);
    assert_eq!(
        stdout, expected_stdout,
        "script: {script:?}\nstderr: {stderr}"
    );
    assert_eq!(code, expected_code, "script: {script:?}\nstderr: {stderr}");
}

mod builtins {
    use super::*;

    #[test]
    fn echo_basic() {
        assert_output("echo hello world", "hello world\n");
    }

    #[test]
    fn echo_no_newline() {
        assert_output("echo -n hello", "hello");
    }

    #[test]
    fn echo_escape() {
        assert_output(r#"echo -e "hello\tworld""#, "hello\tworld\n");
    }

    #[test]
    fn true_false() {
        assert_status("true", 0);
        assert_status("false", 1);
    }

    #[test]
    fn colon() {
        assert_status(":", 0);
    }

    #[test]
    fn exit_code() {
        assert_status("exit 42", 42);
    }

    #[test]
    fn exit_preserves_status() {
        assert_status("false; exit", 1);
    }

    #[test]
    fn test_builtin_eq() {
        assert_status("test 1 -eq 1", 0);
        assert_status("test 1 -eq 2", 1);
    }

    #[test]
    fn test_bracket() {
        assert_status("[ -n hello ]", 0);
        assert_status("[ -z hello ]", 1);
        assert_status("[ -z '' ]", 0);
    }

    #[test]
    fn cd_and_pwd() {
        let (stdout, _, code) = run("cd /tmp && pwd");
        assert_eq!(code, 0);
        // macOS: /tmp is a symlink to /private/tmp
        assert!(
            stdout.trim() == "/tmp" || stdout.trim() == "/private/tmp",
            "unexpected pwd: {stdout}"
        );
    }

    #[test]
    fn export_and_subshell() {
        assert_output("X=hello; export X; echo $X", "hello\n");
    }

    #[test]
    fn unset_variable() {
        assert_output("X=hello; unset X; echo ${X-unset}", "unset\n");
    }

    #[test]
    fn readonly_prevents_set() {
        let (_, stderr, code) = run("readonly X=1; X=2");
        assert_ne!(code, 0);
        assert!(stderr.contains("readonly"), "stderr: {stderr}");
    }

    #[test]
    fn local_in_function() {
        assert_output(
            "f() { local x=inner; echo $x; }; x=outer; f; echo $x",
            "inner\nouter\n",
        );
    }

    #[test]
    fn set_positional() {
        assert_output("set -- a b c; echo $1 $2 $3", "a b c\n");
    }

    #[test]
    fn shift_positional() {
        assert_output("set -- a b c; shift; echo $1 $2", "b c\n");
    }

    #[test]
    fn read_builtin() {
        // read in a pipeline runs in a subshell, so variable doesn't persist
        // Use a heredoc or process substitution instead
        assert_output("read x <<EOF\nhello\nEOF\necho $x", "hello\n");
    }

    #[test]
    fn eval_builtin() {
        assert_output("eval 'echo hello'", "hello\n");
    }

    #[test]
    fn command_builtin() {
        assert_output("command echo hello", "hello\n");
    }

    #[test]
    fn type_builtin() {
        let (stdout, _, code) = run("type echo");
        assert_eq!(code, 0);
        assert!(stdout.contains("builtin"), "stdout: {stdout}");
    }

    #[test]
    fn trap_exit() {
        assert_output("trap 'echo bye' EXIT; echo hi", "hi\nbye\n");
    }

    #[test]
    fn printf_basic() {
        assert_output("printf '%s %s\\n' hello world", "hello world\n");
    }

    #[test]
    fn printf_format_d() {
        assert_output("printf '%d\\n' 42", "42\n");
    }

    #[test]
    fn umask_display() {
        let (stdout, _, code) = run("umask");
        assert_eq!(code, 0);
        assert!(stdout.trim().len() == 4, "expected 4-digit octal: {stdout}");
    }

    #[test]
    fn getopts_basic() {
        assert_output(
            r#"set -- -a -b foo; while getopts ab opt; do echo $opt; done; echo $OPTIND"#,
            "a\nb\n3\n",
        );
    }

    #[test]
    fn break_loop() {
        assert_output("for x in a b c; do echo $x; break; done", "a\n");
    }

    #[test]
    fn continue_loop() {
        assert_output(
            r#"for x in a b c; do
                if [ "$x" = b ]; then continue; fi
                echo $x
            done"#,
            "a\nc\n",
        );
    }
}

mod expansion {
    use super::*;

    #[test]
    fn variable() {
        assert_output("x=hello; echo $x", "hello\n");
    }

    #[test]
    fn default_value() {
        assert_output("echo ${x-default}", "default\n");
    }

    #[test]
    fn default_colon() {
        assert_output("x=''; echo ${x:-default}", "default\n");
    }

    #[test]
    fn assign_default() {
        assert_output("echo ${x=assigned}; echo $x", "assigned\nassigned\n");
    }

    #[test]
    fn alternative_value() {
        assert_output("x=set; echo ${x+alt}", "alt\n");
        assert_output("echo ${x+alt}", "\n");
    }

    #[test]
    fn string_length() {
        assert_output("x=hello; echo ${#x}", "5\n");
    }

    #[test]
    fn trim_suffix() {
        assert_output("x=hello.txt; echo ${x%.txt}", "hello\n");
        assert_output("x=a.b.c; echo ${x%%.*}", "a\n");
    }

    #[test]
    fn trim_prefix() {
        assert_output("x=/usr/local/bin; echo ${x#*/}", "usr/local/bin\n");
        assert_output("x=/usr/local/bin; echo ${x##*/}", "bin\n");
    }

    #[test]
    fn command_substitution() {
        assert_output("echo $(echo hello)", "hello\n");
    }

    #[test]
    fn backtick_substitution() {
        assert_output("echo `echo hello`", "hello\n");
    }

    #[test]
    fn arithmetic() {
        assert_output("echo $((2 + 3))", "5\n");
        assert_output("echo $((10 / 3))", "3\n");
        assert_output("echo $((2 * 3 + 1))", "7\n");
    }

    #[test]
    fn tilde_expansion() {
        let (stdout, _, _) = run("echo ~");
        assert!(stdout.starts_with('/'), "expected absolute path: {stdout}");
    }

    #[test]
    fn special_params() {
        assert_output("echo $?", "0\n");
        assert_output("set -- a b c; echo $#", "3\n");
        assert_output("set -- a b c; echo $@", "a b c\n");
    }

    #[test]
    fn double_quote_preserves_spaces() {
        assert_output(r#"x="a  b"; echo "$x""#, "a  b\n");
    }

    #[test]
    fn single_quote_no_expansion() {
        assert_output("x=hello; echo '$x'", "$x\n");
    }

    #[test]
    fn ifs_splitting() {
        assert_output("x='a:b:c'; IFS=:; echo $x", "a b c\n");
    }

    #[test]
    fn glob_expansion() {
        assert_output("echo /dev/nul?", "/dev/null\n");
    }

    #[test]
    fn error_message() {
        let (_, stderr, code) = run(r#"echo ${x?missing}"#);
        assert_ne!(code, 0);
        assert!(stderr.contains("missing"), "stderr: {stderr}");
    }

    #[test]
    fn nested_comsub() {
        assert_output("echo $(echo $(echo deep))", "deep\n");
    }

    #[test]
    fn comsub_in_default() {
        assert_output("echo ${x:-$(echo hello)}", "hello\n");
    }
}

mod control_flow {
    use super::*;

    #[test]
    fn if_then_else() {
        assert_output("if true; then echo yes; else echo no; fi", "yes\n");
        assert_output("if false; then echo yes; else echo no; fi", "no\n");
    }

    #[test]
    fn elif() {
        assert_output(
            "if false; then echo 1; elif true; then echo 2; else echo 3; fi",
            "2\n",
        );
    }

    #[test]
    fn while_loop() {
        assert_output(
            "x=3; while [ $x -gt 0 ]; do echo $x; x=$((x-1)); done",
            "3\n2\n1\n",
        );
    }

    #[test]
    fn until_loop() {
        assert_output(
            "x=0; until [ $x -eq 3 ]; do echo $x; x=$((x+1)); done",
            "0\n1\n2\n",
        );
    }

    #[test]
    fn for_loop() {
        assert_output("for x in a b c; do echo $x; done", "a\nb\nc\n");
    }

    #[test]
    fn for_positional() {
        assert_output("set -- x y z; for i; do echo $i; done", "x\ny\nz\n");
    }

    #[test]
    fn case_statement() {
        assert_output(
            r#"case hello in
                hi) echo 1 ;;
                hello) echo 2 ;;
                *) echo 3 ;;
            esac"#,
            "2\n",
        );
    }

    #[test]
    fn case_pattern_or() {
        assert_output(
            r#"case foo in
                foo|bar) echo matched ;;
                *) echo nope ;;
            esac"#,
            "matched\n",
        );
    }

    #[test]
    fn pipeline() {
        assert_output("echo hello | cat", "hello\n");
        assert_output("echo abc | tr a-z A-Z", "ABC\n");
    }

    #[test]
    fn pipeline_bang() {
        assert_status("! false", 0);
        assert_status("! true", 1);
    }

    #[test]
    fn and_list() {
        assert_output("true && echo yes", "yes\n");
        // false && echo yes → exit 1 (from false), no output
        assert_stdout_status("false && echo yes", "", 1);
    }

    #[test]
    fn or_list() {
        assert_output("false || echo fallback", "fallback\n");
        assert_output("true || echo fallback", "");
    }

    #[test]
    fn subshell() {
        assert_output("(echo hello)", "hello\n");
        assert_output("x=outer; (x=inner; echo $x); echo $x", "inner\nouter\n");
    }

    #[test]
    fn brace_group() {
        assert_output("{ echo hello; }", "hello\n");
    }

    #[test]
    fn function_def() {
        assert_output("greet() { echo hello $1; }; greet world", "hello world\n");
    }

    #[test]
    fn function_return() {
        assert_output(
            "f() { echo before; return 0; echo after; }; f",
            "before\n",
        );
    }

    #[test]
    fn background() {
        // Just verify it doesn't hang; background processes run asynchronously
        assert_status("echo bg &", 0);
    }

    #[test]
    fn sequence() {
        assert_output("echo a; echo b; echo c", "a\nb\nc\n");
    }

    #[test]
    fn errexit() {
        assert_status("set -e; true; false; echo should-not-reach", 1);
    }

    #[test]
    fn errexit_in_condition() {
        // errexit should NOT trigger inside if condition
        assert_output(
            "set -e; if false; then echo yes; else echo no; fi; echo reached",
            "no\nreached\n",
        );
    }

    #[test]
    fn errexit_and_or() {
        // errexit suppressed in left side of && and ||
        assert_output("set -e; false || echo ok; echo reached", "ok\nreached\n");
    }
}

mod redirections {
    use super::*;

    #[test]
    fn output_redirect() {
        let (stdout, _, _) = run("echo hello > /dev/null; echo $?");
        assert_eq!(stdout, "0\n");
    }

    #[test]
    fn input_redirect() {
        assert_output("cat < /dev/null", "");
    }

    #[test]
    fn append_redirect() {
        assert_output(
            "f=/tmp/epsh-test-$$; echo a > $f; echo b >> $f; cat $f; rm $f",
            "a\nb\n",
        );
    }

    #[test]
    fn heredoc() {
        assert_output(
            "cat <<EOF\nhello world\nEOF",
            "hello world\n",
        );
    }

    #[test]
    fn heredoc_expansion() {
        assert_output(
            "x=world; cat <<EOF\nhello $x\nEOF",
            "hello world\n",
        );
    }

    #[test]
    fn heredoc_quoted_no_expansion() {
        assert_output(
            "x=world; cat <<'EOF'\nhello $x\nEOF",
            "hello $x\n",
        );
    }

    #[test]
    fn heredoc_strip_tabs() {
        assert_output(
            "cat <<-EOF\n\thello\n\tworld\nEOF",
            "hello\nworld\n",
        );
    }

    #[test]
    fn multiple_heredocs() {
        assert_output(
            "cat <<EOF1; cat <<EOF2\none\nEOF1\ntwo\nEOF2",
            "one\ntwo\n",
        );
    }

    #[test]
    fn fd_dup() {
        assert_output("echo hello 1>&2 2>/dev/null", "");
    }
}

mod assignment {
    use super::*;

    #[test]
    fn simple_assignment() {
        assert_output("x=hello; echo $x", "hello\n");
    }

    #[test]
    fn multi_assignment() {
        assert_output("a=1 b=2; echo $a $b", "1 2\n");
    }

    #[test]
    fn assignment_with_command() {
        assert_output("x=hello echo $x", "\n"); // x is not set for expansion before echo runs
    }

    #[test]
    fn command_prefix_assignment() {
        // Assignment as prefix exports to that command's environment
        assert_output("x=before; x=during env | grep '^x='; echo $x", "x=during\nbefore\n");
    }
}

/// Tests adapted from oils/spec/posix.test.sh
mod oils_posix {
    use super::*;

    #[test]
    fn empty_for_loop() {
        assert_output("set -- a b; for x in; do echo hi; echo $x; done", "");
    }

    #[test]
    fn empty_for_loop_without_in() {
        assert_output(
            "set -- a b; for x do echo hi; echo $x; done",
            "hi\na\nhi\nb\n",
        );
    }

    #[test]
    fn empty_case() {
        assert_output("case foo in esac", "");
    }

    #[test]
    fn last_case_without_double_semi() {
        assert_output(
            "foo=a; case $foo in a) echo A ;; b) echo B\nesac",
            "A\n",
        );
    }

    #[test]
    fn only_case_without_double_semi() {
        assert_output("foo=a; case $foo in a) echo A\nesac", "A\n");
    }

    #[test]
    fn case_with_optional_paren() {
        assert_output(
            "foo=a; case $foo in (a) echo A ;; (b) echo B\nesac",
            "A\n",
        );
    }

    #[test]
    fn empty_action_last_case() {
        assert_output(
            "foo=b; case $foo in a) echo A ;; b)\nesac",
            "",
        );
    }

    #[test]
    fn case_with_pipe_pattern() {
        assert_output(
            "foo=a; case $foo in a|b) echo A ;; c)\nesac",
            "A\n",
        );
    }

    #[test]
    fn bare_semicolon_syntax_error() {
        assert_status(";", 2);
    }

    #[test]
    fn comsub_in_default() {
        assert_output("echo ${x:-$(echo /bin)}", "/bin\n");
    }

    #[test]
    fn arithmetic_in_while() {
        assert_output(
            "x=3; while [ $x -gt 0 ]; do echo $x; x=$(($x-1)); done",
            "3\n2\n1\n",
        );
    }

    #[test]
    fn multiple_heredocs_on_one_line() {
        assert_output(
            "cat <<EOF1; cat <<EOF2\none\nEOF1\ntwo\nEOF2",
            "one\ntwo\n",
        );
    }

    #[test]
    fn heredoc_echo_heredoc() {
        assert_output(
            "cat <<EOF1; echo two; cat <<EOF2\none\nEOF1\nthree\nEOF2",
            "one\ntwo\nthree\n",
        );
    }
}
