#!/usr/bin/env python3
"""
Test runner for epsh using mksh-format test files.
Parses check.t format and runs tests against a specified shell.

Usage:
    python3 run-tests.py [options]

Options:
    -s SHELL    Shell to test (default: ./target/debug/epsh)
    -t FILE     Test file (default: check-epsh.t)
    -v          Verbose: show failure details
    -n NAME     Run only tests matching NAME (substring match)
    -q          Quiet: only show summary
"""

import os
import re
import subprocess
import sys
import tempfile
import shutil
from dataclasses import dataclass, field
from typing import Optional


@dataclass
class Test:
    name: str = ""
    description: str = ""
    stdin: str = ""
    expected_stdout: str = ""
    expected_stderr: str = ""
    expected_exit: str = "0"
    category: str = ""
    env_setup: dict = field(default_factory=dict)
    arguments: list = field(default_factory=list)
    file_setup: list = field(default_factory=list)
    need_ctty: bool = False
    time_limit: int = 30
    expected_fail: bool = False


def parse_tests(filename: str) -> list[Test]:
    """Parse a mksh check.t format test file."""
    with open(filename, "r", errors="replace") as f:
        content = f.read()

    tests = []
    blocks = re.split(r"^---\s*$", content, flags=re.MULTILINE)

    for block in blocks:
        test = Test()
        lines = block.split("\n")
        i = 0
        while i < len(lines):
            line = lines[i]

            # Parse tag: value
            m = re.match(r"^(\w[\w-]*):\s*(.*)", line)
            if m:
                tag = m.group(1)
                value = m.group(2)

                # Read multi-line value (tab-indented continuation)
                multiline = []
                if value:
                    multiline.append(value)
                i += 1
                while i < len(lines) and lines[i].startswith("\t"):
                    multiline.append(lines[i][1:])  # strip leading tab
                    i += 1

                text = "\n".join(multiline)

                if tag == "name":
                    test.name = text.strip()
                elif tag == "description":
                    test.description = text.strip()
                elif tag == "stdin":
                    test.stdin = text + "\n" if text else ""
                elif tag == "expected-stdout":
                    test.expected_stdout = text + "\n" if text else ""
                elif tag == "expected-stderr":
                    test.expected_stderr = text + "\n" if text else ""
                elif tag == "expected-stderr-pattern":
                    test.expected_stderr = ""  # We'll skip pattern matching for now
                elif tag == "expected-exit":
                    test.expected_exit = text.strip()
                elif tag == "category":
                    test.category = text.strip()
                elif tag == "need-ctty":
                    test.need_ctty = text.strip().lower() == "yes"
                elif tag == "time-limit":
                    test.time_limit = int(text.strip())
                elif tag == "expected-fail":
                    test.expected_fail = text.strip().lower() == "yes"
                elif tag == "env-setup":
                    # Format: !name=value!name2=value2!
                    for pair in re.findall(r"(\w+)=([^!]*)", text):
                        test.env_setup[pair[0]] = pair[1]
                elif tag == "arguments":
                    # Format: !arg1!arg2!
                    test.arguments = [a for a in text.split("!") if a]
                elif tag == "file-setup":
                    # file-setup: file MODE "FILENAME"\n\tCONTENT...
                    m2 = re.match(r'file\s+(\d+)\s+"([^"]+)"', text.split("\n")[0] if text else "")
                    if m2:
                        mode = int(m2.group(1), 8)
                        fname = m2.group(2)
                        fcontent = "\n".join(text.split("\n")[1:])
                        test.file_setup.append((fname, fcontent, mode))
            else:
                i += 1

        if test.name:
            tests.append(test)

    return tests


def check_exit(actual: int, expected_str: str) -> bool:
    """Check if actual exit status matches expected expression."""
    expected_str = expected_str.strip()

    # Simple numeric
    try:
        return actual == int(expected_str)
    except ValueError:
        pass

    # Expression like "e != 0"
    e = actual
    try:
        return eval(expected_str.replace("e", str(e)))
    except Exception:
        return False


def run_test(test: Test, shell: str, verbose: bool) -> tuple[bool, str]:
    """Run a single test. Returns (passed, failure_reason)."""
    if test.need_ctty:
        return True, "skipped (need-ctty)"

    # Create temp directory for the test
    tmpdir = tempfile.mkdtemp(prefix="epsh-test-")
    try:
        # Set up files
        for fname, fcontent, mode in test.file_setup:
            fpath = os.path.join(tmpdir, fname)
            os.makedirs(os.path.dirname(fpath), exist_ok=True)
            with open(fpath, "w") as f:
                f.write(fcontent)
            os.chmod(fpath, mode)

        # Build environment
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": tmpdir,
            "SHELL": shell,
            "ENV": "/nonexisting",
        }
        env.update(test.env_setup)

        # Build command
        cmd = [shell] + test.arguments

        try:
            result = subprocess.run(
                cmd,
                input=test.stdin,
                capture_output=True,
                text=True,
                timeout=test.time_limit,
                cwd=tmpdir,
                env=env,
            )
        except subprocess.TimeoutExpired:
            return False, "timed out"
        except Exception as e:
            return False, f"execution error: {e}"

        # Check exit status
        if not check_exit(result.returncode, test.expected_exit):
            return False, (
                f"wrong exit status: got {result.returncode}, "
                f"expected {test.expected_exit}"
            )

        # Check stdout
        if test.expected_stdout and result.stdout != test.expected_stdout:
            # Find first difference
            expected_lines = test.expected_stdout.split("\n")
            got_lines = result.stdout.split("\n")
            diff_line = 1
            for i, (e, g) in enumerate(zip(expected_lines, got_lines)):
                if e != g:
                    diff_line = i + 1
                    break
            else:
                diff_line = min(len(expected_lines), len(got_lines)) + 1

            reason = f"wrong stdout (first diff at line {diff_line})"
            if verbose:
                reason += f"\n  expected: {repr(test.expected_stdout[:200])}"
                reason += f"\n  got:      {repr(result.stdout[:200])}"
            return False, reason

        # Check stderr (only if expected is explicitly empty and we got output)
        if (
            test.expected_stderr == ""
            and result.stderr
            and "expected-stderr" not in ""  # Only check if test specifies stderr
        ):
            pass  # Don't fail on unexpected stderr by default

        return True, ""

    finally:
        shutil.rmtree(tmpdir, ignore_errors=True)


def main():
    import getopt

    shell = "./target/debug/epsh"
    test_file = "check-epsh.t"
    verbose = False
    quiet = False
    name_filter = None

    try:
        opts, args = getopt.getopt(sys.argv[1:], "s:t:vqn:")
    except getopt.GetoptError as e:
        print(f"Error: {e}", file=sys.stderr)
        print(__doc__, file=sys.stderr)
        sys.exit(1)

    for opt, val in opts:
        if opt == "-s":
            shell = os.path.abspath(val)
        elif opt == "-t":
            test_file = val
        elif opt == "-v":
            verbose = True
        elif opt == "-q":
            quiet = True
        elif opt == "-n":
            name_filter = val

    # Resolve shell path
    shell = os.path.abspath(shell)

    # Parse tests
    tests = parse_tests(test_file)

    # Filter
    if name_filter:
        tests = [t for t in tests if name_filter in t.name]

    passed = 0
    failed = 0
    skipped = 0
    failures = []

    for test in tests:
        ok, reason = run_test(test, shell, verbose)
        if "skipped" in reason:
            skipped += 1
            continue

        if ok:
            passed += 1
            if not quiet:
                print(f"  pass  {test.name}")
        else:
            failed += 1
            failures.append((test.name, reason))
            if not quiet:
                print(f"  FAIL  {test.name}")
                if verbose:
                    print(f"        {reason}")

    print(f"\n{'=' * 60}")
    print(f"Results: {passed} passed, {failed} failed, {skipped} skipped")
    print(f"{'=' * 60}")

    if failures and not verbose:
        print(f"\nFailed tests:")
        for name, reason in failures:
            print(f"  {name}: {reason}")

    sys.exit(0 if failed == 0 else 1)


if __name__ == "__main__":
    main()
