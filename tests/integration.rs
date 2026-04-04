use std::process::Command;
use std::path::Path;

fn epsh() -> Command {
    let bin = env!("CARGO_BIN_EXE_epsh");
    Command::new(bin)
}

/// Run a script in a specific working directory.
fn run_in(dir: &Path, script: &str) -> (String, String, i32) {
    let out = epsh()
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .output()
        .expect("failed to execute epsh");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(128),
    )
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

mod xtrace {
    use super::*;

    #[test]
    fn basic_trace() {
        let (stdout, stderr, _) = run("set -x; echo hello");
        assert_eq!(stdout, "hello\n");
        assert!(stderr.contains("+ echo hello"), "stderr: {stderr}");
    }

    #[test]
    fn trace_assignment() {
        let (_, stderr, _) = run("set -x; x=hello");
        assert!(stderr.contains("x=hello"), "stderr: {stderr}");
    }

    #[test]
    fn trace_with_expansion() {
        let (stdout, stderr, _) = run("set -x; x=world; echo hello $x");
        assert_eq!(stdout, "hello world\n");
        assert!(stderr.contains("+ echo hello world"), "stderr: {stderr}");
    }

    #[test]
    fn custom_ps4() {
        let (_, stderr, _) = run("PS4='>> '; set -x; echo hi");
        assert!(stderr.contains(">> echo hi"), "stderr: {stderr}");
    }
}

mod glob_files {
    use super::*;
    use std::fs;
    use std::os::unix::fs as unix_fs;

    /// glob-bad-2: Check that symbolic links aren't stat()'d
    #[test]
    fn glob_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("dir");
        fs::create_dir(&sub).unwrap();
        unix_fs::symlink("non-existent-file", sub.join("abc")).unwrap();

        let (stdout, _, code) = run_in(dir.path(), "echo d*/*\necho d*/abc");
        assert_eq!(code, 0);
        assert_eq!(stdout, "dir/abc\ndir/abc\n");
    }

    /// glob-range-6: glob vs test bracket expression
    #[test]
    fn glob_star_b_star() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("abc"), "").unwrap();
        fs::write(dir.path().join("cbc"), "").unwrap();

        let (stdout, _, code) = run_in(dir.path(), "echo *b*");
        assert_eq!(code, 0);
        assert_eq!(stdout, "abc cbc\n");
    }

    /// glob-range-1: ranges and special chars in brackets
    #[test]
    fn glob_bracket_ranges() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["-bc", "abc", "bbc", "cbc", "!bc", "^bc", "+bc", ",bc", "0bc", "1bc"] {
            fs::write(dir.path().join(name), "").unwrap();
        }

        // [ab-]* — dash at end is literal
        let (stdout, _, _) = run_in(dir.path(), "echo [ab-]*");
        assert_eq!(stdout, "-bc abc bbc\n");

        // [-ab]* — dash at start is literal
        let (stdout, _, _) = run_in(dir.path(), "echo [-ab]*");
        assert_eq!(stdout, "-bc abc bbc\n");

        // [!ab]* — negated
        let (stdout, _, _) = run_in(dir.path(), "echo [!ab]*");
        assert_eq!(stdout, "!bc +bc ,bc -bc 0bc 1bc ^bc cbc\n");

        // [^ab]* — ^ is NOT negation in POSIX, it's literal
        let (stdout, _, _) = run_in(dir.path(), "echo [^ab]*");
        assert_eq!(stdout, "^bc abc bbc\n");

        // [+--]* — range from + to - (ASCII 43-45)
        let (stdout, _, _) = run_in(dir.path(), "echo [+--]*");
        assert_eq!(stdout, "+bc ,bc -bc\n");

        // [--1]* — range from - to 1 (ASCII 45-49)
        let (stdout, _, _) = run_in(dir.path(), "echo [--1]*");
        assert_eq!(stdout, "-bc 0bc 1bc\n");
    }
}

/// Tests adapted from oils/spec/ — POSIX-compatible edge cases
mod oils_spec {
    use super::*;

    #[test]
    fn fatal_error_question_mark() {
        // Standalone ${a?msg} is fatal
        let (stdout, _, code) = run("echo ${a?bc}; echo blah");
        assert_eq!(stdout, "");
        assert_ne!(code, 0);
    }

    #[test]
    fn readonly_var_is_fatal() {
        let (stdout, _, code) = run("readonly abc=123; abc=def; echo status=$?");
        assert_eq!(stdout, "");
        assert_ne!(code, 0);
    }

    #[test]
    fn var_and_func_same_name() {
        assert_output(
            "potato() { echo hello; }; potato=42; echo $potato; potato",
            "42\nhello\n",
        );
    }

    #[test]
    fn for_loop_newline_before_in() {
        assert_output(
            "for i\nin one two three\ndo echo $i\ndone",
            "one\ntwo\nthree\n",
        );
    }

    #[test]
    fn comsub_case_in_subshell() {
        // Use (pattern) form — bare pattern) inside $() is ambiguous with closing )
        assert_output(
            "echo $(foo=a; case $foo in ([0-9]) echo number;; ([a-z]) echo letter;; esac)",
            "letter\n",
        );
    }

    #[test]
    fn comsub_word_part() {
        assert_output(
            "foo=FOO; echo $(echo $foo)bar$(echo $foo)",
            "FOObarFOO\n",
        );
    }

    #[test]
    fn backtick_word_part() {
        assert_output(
            "foo=FOO; echo `echo $foo`bar`echo $foo`",
            "FOObarFOO\n",
        );
    }

    #[test]
    fn making_command_from_comsub() {
        assert_output("$(echo ec)$(echo ho) split builtin", "split builtin\n");
    }

    #[test]
    fn comsub_exit_code() {
        assert_output(
            "echo $(echo x; exit 33); echo $?; x=$(echo x; exit 33); echo $?",
            "x\n0\n33\n",
        );
    }

    #[test]
    fn empty_comsub() {
        // Empty $() vanishes in unquoted context — -$()- becomes --
        assert_output("echo -$()-  .$(). ", "-- ..\n");
    }

    #[test]
    fn errexit_aborts_early() {
        assert_stdout_status("set -o errexit; false; echo done", "", 1);
    }

    #[test]
    fn errexit_nonexistent_command() {
        assert_stdout_status("set -o errexit; nonexistent__ZZ; echo done", "", 127);
    }

    #[test]
    fn errexit_brace_group() {
        assert_stdout_status(
            "set -o errexit; { echo one; false; echo two; }",
            "one\n",
            1,
        );
    }

    #[test]
    fn errexit_if_suppressed() {
        assert_output(
            "set -o errexit\nif { echo one; false; echo two; }; then\n  echo three\nfi\necho four",
            "one\ntwo\nthree\nfour\n",
        );
    }

    #[test]
    fn errexit_with_bang() {
        assert_output(
            "set -o errexit; echo one; ! true; echo two; ! false; echo three",
            "one\ntwo\nthree\n",
        );
    }

    #[test]
    fn errexit_subshell() {
        assert_stdout_status(
            "set -o errexit; ( echo one; false; echo two; ); echo three",
            "one\n",
            1,
        );
    }

    #[test]
    fn assignment_no_word_splitting() {
        assert_output(
            r#"words='one two'; a=$words; echo "$a""#,
            "one two\n",
        );
    }

    #[test]
    fn assignment_no_glob() {
        assert_output(
            r#"a='*.nope'; b=$a; echo "$b""#,
            "*.nope\n",
        );
    }

    #[test]
    fn empty_assignment() {
        assert_output(r#"EMPTY=; echo "[$EMPTY]""#, "[]\n");
    }

    #[test]
    fn ifs_custom_delimiter() {
        assert_output(
            "IFS=x; X='onextwoxxthree'; y=$X; echo $y",
            "one two  three\n",
        );
    }

    #[test]
    fn pipeline_brace_group() {
        assert_output(
            "echo hello | { read i; echo $i; } | { read i; echo $i; } | cat",
            "hello\n",
        );
    }
}

mod nounset {
    use super::*;

    #[test]
    fn unset_var_errors() {
        let (_, stderr, code) = run("set -u; echo $NONEXISTENT");
        assert_ne!(code, 0);
        assert!(stderr.contains("parameter not set"), "stderr: {stderr}");
    }

    #[test]
    fn set_var_ok() {
        assert_output("set -u; X=hello; echo $X", "hello\n");
    }

    #[test]
    fn special_params_ok() {
        // $?, $$, $# should never trigger nounset
        let (stdout, _, code) = run("set -u; echo $?");
        assert_eq!(code, 0);
        assert_eq!(stdout, "0\n");
    }

    #[test]
    fn default_op_bypasses() {
        assert_output("set -u; echo ${UNSET-fallback}", "fallback\n");
    }

    #[test]
    fn assign_op_bypasses() {
        assert_output("set -u; echo ${UNSET=assigned}; echo $UNSET", "assigned\nassigned\n");
    }

    #[test]
    fn alternative_op_bypasses() {
        assert_output("set -u; echo \"${UNSET+alt}\"", "\n");
    }

    #[test]
    fn error_op_still_errors() {
        let (_, stderr, code) = run("set -u; echo ${UNSET?custom msg}");
        assert_ne!(code, 0);
        assert!(stderr.contains("custom msg"), "stderr: {stderr}");
    }

    #[test]
    fn empty_var_not_unset() {
        // set -u should NOT error on empty-but-set variables
        assert_output("set -u; X=''; echo \"$X\"", "\n");
    }

    #[test]
    fn length_of_unset_errors() {
        let (_, stderr, code) = run("set -u; echo ${#NONEXISTENT}");
        assert_ne!(code, 0);
        assert!(stderr.contains("parameter not set"), "stderr: {stderr}");
    }
}

mod kill_builtin {
    use super::*;

    #[test]
    fn kill_zero_tests_process() {
        // kill -0 tests if process exists without sending a signal
        assert_output("kill -0 $$; echo $?", "0\n");
    }

    #[test]
    fn kill_invalid_pid() {
        let (_, stderr, code) = run("kill -0 999999999");
        assert_ne!(code, 0);
        assert!(stderr.contains("999999999"), "stderr: {stderr}");
    }

    #[test]
    fn kill_list_signals() {
        let (stdout, _, code) = run("kill -l");
        assert_eq!(code, 0);
        assert!(stdout.contains("SIGTERM"), "stdout: {stdout}");
        assert!(stdout.contains("SIGINT"), "stdout: {stdout}");
    }

    #[test]
    fn kill_list_exit_status() {
        // 130 = 128 + 2 (SIGINT)
        assert_output("kill -l 130", "INT\n");
    }

    #[test]
    fn kill_sends_signal() {
        // Start a background sleep and kill it
        assert_output("sleep 10 & kill $!; wait; echo done", "done\n");
    }

    #[test]
    fn kill_named_signal() {
        assert_output("sleep 10 & kill -s TERM $!; wait; echo done", "done\n");
    }

    #[test]
    fn kill_is_builtin() {
        assert_output("command -v kill", "kill\n");
    }
}

mod exec_redirects {
    use super::*;

    #[test]
    fn exec_output_redirect() {
        // After exec > file, output goes to the file, not terminal
        let tmp = format!("/tmp/epsh_test_exec_out_{}", std::process::id());
        let _ = std::fs::remove_file(&tmp);
        let (stdout, _, code) = run(
            &format!("exec > {tmp}; echo hello")
        );
        assert_eq!(code, 0);
        assert_eq!(stdout, "");
        let contents = std::fs::read_to_string(&tmp).unwrap();
        assert_eq!(contents, "hello\n");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn exec_append_redirect() {
        let tmp = format!("/tmp/epsh_test_exec_app_{}", std::process::id());
        let _ = std::fs::remove_file(&tmp);
        let (_, _, code) = run(
            &format!("echo first > {tmp}; exec >> {tmp}; echo second")
        );
        assert_eq!(code, 0);
        let contents = std::fs::read_to_string(&tmp).unwrap();
        assert_eq!(contents, "first\nsecond\n");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn exec_dup_fd() {
        assert_output("exec 3>&1; echo dup_works >&3", "dup_works\n");
    }

    #[test]
    fn exec_close_fd() {
        let tmp = format!("/tmp/epsh_test_exec_close_{}", std::process::id());
        let _ = std::fs::remove_file(&tmp);
        let (stdout, _, code) = run(
            &format!("exec 3>{tmp}; echo via_fd3 >&3; exec 3>&-; cat {tmp}")
        );
        assert_eq!(code, 0);
        assert_eq!(stdout, "via_fd3\n");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn exec_read_write() {
        let tmp = format!("/tmp/epsh_test_exec_rw_{}", std::process::id());
        let _ = std::fs::remove_file(&tmp);
        let (stdout, _, code) = run(
            &format!("echo content > {tmp}; exec 4<>{tmp}; read line <&4; echo $line")
        );
        assert_eq!(code, 0);
        assert_eq!(stdout, "content\n");
        let _ = std::fs::remove_file(&tmp);
    }
}

mod read_builtin {
    use super::*;

    #[test]
    fn read_normal_line() {
        assert_output("echo hello | { read x; echo $x; }", "hello\n");
    }

    #[test]
    fn read_eof_no_data() {
        assert_stdout_status("printf '' | { read x; echo \"x=$x\"; }", "x=\n", 0);
    }

    #[test]
    fn read_eof_partial_data() {
        // read should assign the partial data but return 1
        let (stdout, _, _) = run("printf partial | { read x; echo \"x=$x status=$?\"; }");
        assert_eq!(stdout, "x=partial status=1\n");
    }

    #[test]
    fn read_while_loop_eof() {
        // while read should process all complete lines, stop at EOF
        let (stdout, _, code) = run(
            "printf 'a\\nb\\n' | { n=0; while read line; do n=$((n+1)); done; echo $n; }"
        );
        assert_eq!(code, 0);
        assert_eq!(stdout, "2\n");
    }

    #[test]
    fn read_while_loop_partial_last_line() {
        // The partial last line is available after the loop
        let (stdout, _, _) = run(
            "printf 'a\\nb\\npartial' | { while read line; do echo $line; done; echo \"last=$line\"; }"
        );
        assert_eq!(stdout, "a\nb\nlast=partial\n");
    }

    #[test]
    fn read_multiple_vars() {
        assert_output(
            "echo 'one two three four' | { read a b c; echo \"a=$a b=$b c=$c\"; }",
            "a=one b=two c=three four\n"
        );
    }
}

mod heredoc_in_compounds {
    use super::*;

    #[test]
    fn heredoc_in_function() {
        assert_output("f() { cat <<EOF\nhello\nEOF\n}; f", "hello\n");
    }

    #[test]
    fn heredoc_with_expansion_in_function() {
        assert_output(
            "f() { x=world; cat <<EOF\nhello $x\nEOF\n}; f",
            "hello world\n",
        );
    }

    #[test]
    fn multiple_heredocs_in_function() {
        assert_output(
            "f() { cat <<EOF\nfirst\nEOF\ncat <<EOF\nsecond\nEOF\n}; f",
            "first\nsecond\n",
        );
    }

    #[test]
    fn heredoc_in_nested_function() {
        assert_output(
            "f() { g() { cat <<EOF\nnested\nEOF\n}; g; }; f",
            "nested\n",
        );
    }

    #[test]
    fn heredoc_in_for_loop() {
        assert_output(
            "for i in a b; do cat <<EOF\n$i\nEOF\ndone",
            "a\nb\n",
        );
    }

    #[test]
    fn heredoc_in_if() {
        assert_output("if true; then cat <<EOF\nyes\nEOF\nfi", "yes\n");
    }

    #[test]
    fn heredoc_in_while() {
        assert_output(
            "x=1; while [ \"$x\" = 1 ]; do cat <<EOF\nloop\nEOF\nx=0; done",
            "loop\n",
        );
    }

    #[test]
    fn heredoc_in_case() {
        assert_output(
            "case x in x) cat <<EOF\nmatched\nEOF\n;; esac",
            "matched\n",
        );
    }

    #[test]
    fn function_called_twice_with_heredoc() {
        assert_output("f() { cat <<EOF\nhi\nEOF\n}; f; f", "hi\nhi\n");
    }
}

mod printf_format {
    use super::*;

    #[test]
    fn printf_loops_format_over_remaining_args() {
        assert_output("printf '%s\n' a b c", "a\nb\nc\n");
    }

    #[test]
    fn printf_loops_two_arg_format() {
        assert_output("printf '%s=%s\n' x 1 y 2", "x=1\ny=2\n");
    }

    #[test]
    fn printf_partial_final_iteration() {
        // Last iteration has fewer args than format specs; missing args default to empty
        assert_output("printf '%s,%s\n' a b c", "a,b\nc,\n");
    }

    #[test]
    fn printf_no_args_runs_format_once() {
        assert_output("printf 'hello\n'", "hello\n");
    }
}

mod dot_return {
    use super::*;

    #[test]
    fn return_exits_dot_script() {
        let (stdout, _, code) = run(
            "echo 'echo before; return 0; echo after' > /tmp/epsh_dot_ret_$$.sh; . /tmp/epsh_dot_ret_$$.sh; echo continued; rm -f /tmp/epsh_dot_ret_$$.sh"
        );
        assert_eq!(stdout, "before\ncontinued\n");
        assert_eq!(code, 0);
    }

    #[test]
    fn return_with_status_from_dot_script() {
        let (stdout, _, code) = run(
            "echo 'return 42' > /tmp/epsh_dot_ret2_$$.sh; . /tmp/epsh_dot_ret2_$$.sh; echo \"rc=$?\"; rm -f /tmp/epsh_dot_ret2_$$.sh"
        );
        assert_eq!(stdout, "rc=42\n");
        assert_eq!(code, 0);
    }
}

mod fd_numbers {
    use super::*;

    #[test]
    fn three_digit_fd_redirect() {
        // Use fd 100 — redirect output, then write to it
        assert_output(
            "exec 100>/tmp/epsh_fd100_$$.txt; echo hello >&100; exec 100>&-; cat /tmp/epsh_fd100_$$.txt; rm -f /tmp/epsh_fd100_$$.txt",
            "hello\n",
        );
    }

    #[test]
    fn two_digit_fd_redirect() {
        assert_output(
            "exec 10>/tmp/epsh_fd10_$$.txt; echo hi >&10; exec 10>&-; cat /tmp/epsh_fd10_$$.txt; rm -f /tmp/epsh_fd10_$$.txt",
            "hi\n",
        );
    }
}

mod getopts_safety {
    use super::*;

    #[test]
    fn getopts_basic() {
        assert_output(
            "set -- -a -b; while getopts ab opt; do echo $opt; done",
            "a\nb\n",
        );
    }

    #[test]
    fn getopts_with_arg() {
        assert_output(
            "set -- -f file; while getopts f: opt; do echo \"$opt=$OPTARG\"; done",
            "f=file\n",
        );
    }
}

mod set_dash {
    use super::*;

    #[test]
    fn set_dash_clears_xe() {
        // set - should turn off -x and -e per POSIX
        // With xtrace on, "echo hello" would produce "+ echo hello\n" on stderr
        let (stdout, stderr, code) = run("set -x; set -; echo hello");
        assert_eq!(stdout, "hello\n");
        assert!(!stderr.contains("+ echo"), "xtrace should be off after 'set -', got stderr: {stderr}");
        assert_eq!(code, 0);
    }

    #[test]
    fn set_dashdash_stops_flag_processing() {
        assert_output(
            "set -- -a -b -c; echo $1 $2 $3",
            "-a -b -c\n",
        );
    }
}

mod readonly_enforcement {
    use super::*;

    #[test]
    fn readonly_assignment_errors() {
        let (_, stderr, code) = run("readonly X=1; X=2");
        assert!(stderr.contains("readonly"), "stderr: {stderr}");
        assert_ne!(code, 0);
    }

    #[test]
    fn export_readonly_errors() {
        let (_, stderr, code) = run("readonly X=1; export X=2");
        assert!(stderr.contains("readonly"), "stderr: {stderr}");
        assert_ne!(code, 0);
    }

    #[test]
    fn read_readonly_errors() {
        let (_, stderr, code) = run("readonly X=1; echo hello | read X");
        assert!(stderr.contains("readonly"), "stderr: {stderr}");
        assert_ne!(code, 0);
    }

    #[test]
    fn for_readonly_errors() {
        let (_, stderr, code) = run("readonly i=1; for i in a b; do echo $i; done");
        assert!(stderr.contains("readonly"), "stderr: {stderr}");
        assert_ne!(code, 0);
    }

    #[test]
    fn readonly_assignment_exits_shell() {
        // POSIX: assignment to readonly var in non-interactive shell causes exit
        let (_, stderr, code) = run("readonly X=1; X=2; echo should_not_reach");
        assert!(stderr.contains("readonly"), "stderr: {stderr}");
        assert_ne!(code, 0);
    }
}

mod ifs_star_expansion {
    use super::*;

    #[test]
    fn star_trim_with_empty_ifs() {
        // IFS-subst-6: ${x#$*} with IFS="" should concatenate $* for trim
        assert_output(
            "showargs() { for s_arg in \"$@\"; do echo -n \"<$s_arg> \"; done; echo .; }; IFS=; x=abc; set -- a b; showargs ${x#$*}",
            "<c> .\n",
        );
    }

    #[test]
    fn star_assign_with_empty_ifs() {
        // IFS-subst-10: ${var=$*} with IFS="" — assign joins, result is single field
        assert_output(
            "showargs() { for s_arg in \"$@\"; do echo -n \"<$s_arg> \"; done; echo .; }; set -- one \"two three\" four; unset -v var; save_IFS=$IFS; IFS=; set -- ${var=$*}; IFS=$save_IFS; echo \"var=$var\"; showargs \"$@\"",
            "var=onetwo threefour\n<onetwo threefour> .\n",
        );
    }

    #[test]
    fn star_alternative_with_empty_ifs() {
        // xxx-variable-syntax-4 last case: IFS= with "" "" should produce empty $*
        assert_output(
            "foo() { echo \"<$*> X${*:+ }X\"; }; IFS=; foo \"\" \"\"",
            "<> XX\n",
        );
    }

    #[test]
    fn star_joins_with_ifs_char() {
        assert_output(
            "IFS=:; set -- a b c; echo \"$*\"",
            "a:b:c\n",
        );
    }
}

mod at_expansion {
    use super::*;

    #[test]
    fn quoted_at_merges_prefix() {
        // """$@" — empty prefix merges with first $@ element
        assert_output(
            "n() { echo $#; }; set -- a b; n \"\"\"$@\"",
            "2\n",
        );
    }

    #[test]
    fn quoted_at_merges_suffix() {
        // "$@""" — empty suffix merges with last $@ element
        assert_output(
            "n() { echo $#; }; set -- a b; n \"$@\"\"\"",
            "2\n",
        );
    }

    #[test]
    fn var_prefix_at() {
        // "$e""$@" — empty var prefix merges with first $@ element
        assert_output(
            "n() { echo $#; }; unset e; set -- a b; n \"$e\"\"$@\"",
            "2\n",
        );
    }

    #[test]
    fn empty_at_produces_nothing() {
        // "$@" with no positionals produces zero fields
        assert_output(
            "n() { echo $#; }; set --; n \"$@\"",
            "0\n",
        );
    }

    #[test]
    fn empty_at_with_prefix_produces_one() {
        // """$@" with no positionals — prefix "" exists, $@ empty → 1 field
        assert_output(
            "n() { echo $#; }; set --; n \"\"\"$@\"",
            "1\n",
        );
    }
}
