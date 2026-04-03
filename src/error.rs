use std::fmt;

/// Source position for error reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub offset: usize,
    pub line: u32,
    pub col: u32,
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.col)
    }
}

/// Shell errors and control flow signals.
///
/// Control flow (Exit, Return, Break, Continue) propagates up the call stack
/// via Result, replacing dash/posh's setjmp/longjmp. Execution errors carry
/// source positions for diagnostics.
#[derive(Debug)]
pub enum ShellError {
    /// `exit [n]` — terminate the shell
    Exit(ExitStatus),
    /// `return [n]` — return from function or dot-script
    Return(ExitStatus),
    /// `break [n]` — break from n enclosing loops
    Break(usize),
    /// `continue [n]` — continue nth enclosing loop
    Continue(usize),
    /// Syntax error during parsing
    Syntax { msg: String, span: Span },
    /// Command not found in PATH or builtins
    CommandNotFound(String),
    /// I/O error (redirections, pipes, file operations)
    Io(std::io::Error),
    /// Generic runtime error with position
    Runtime { msg: String, span: Span },
    /// Execution was cancelled via the cancel flag
    Cancelled,
}

impl fmt::Display for ShellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShellError::Exit(n) => write!(f, "exit {n}"),
            ShellError::Return(n) => write!(f, "return {n}"),
            ShellError::Break(n) => write!(f, "break {n}"),
            ShellError::Continue(n) => write!(f, "continue {n}"),
            ShellError::Syntax { msg, span } => write!(f, "{span}: syntax error: {msg}"),
            ShellError::CommandNotFound(name) => write!(f, "{name}: not found"),
            ShellError::Io(e) => write!(f, "{e}"),
            ShellError::Runtime { msg, span } => write!(f, "{span}: {msg}"),
            ShellError::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for ShellError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ShellError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ShellError {
    fn from(e: std::io::Error) -> Self {
        ShellError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, ShellError>;

/// Shell exit status (0-255). Provides type safety over bare i32.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExitStatus(i32);

impl ExitStatus {
    pub const SUCCESS: ExitStatus = ExitStatus(0);
    pub const FAILURE: ExitStatus = ExitStatus(1);
    pub const MISUSE: ExitStatus = ExitStatus(2);
    pub const NOT_FOUND: ExitStatus = ExitStatus(127);
    pub const NOT_EXECUTABLE: ExitStatus = ExitStatus(126);

    /// The raw exit code as an integer.
    pub fn code(self) -> i32 {
        self.0
    }

    pub fn success(self) -> bool {
        self.0 == 0
    }

    /// Return SUCCESS if the condition is true, FAILURE otherwise.
    pub fn from_bool(ok: bool) -> Self {
        if ok { Self::SUCCESS } else { Self::FAILURE }
    }

    /// Logical negation: SUCCESS becomes FAILURE and vice versa.
    pub fn inverted(self) -> Self {
        Self::from_bool(!self.success())
    }

    /// Create from a process wait status (as returned by waitpid).
    pub fn from_wait(status: i32) -> Self {
        if crate::sys::wifexited(status) {
            ExitStatus(crate::sys::wexitstatus(status))
        } else {
            ExitStatus(128 + crate::sys::wtermsig(status))
        }
    }

    /// Create from a signal number (128 + sig).
    pub fn from_signal(sig: i32) -> Self {
        ExitStatus(128 + sig)
    }
}

impl From<i32> for ExitStatus {
    fn from(n: i32) -> Self {
        ExitStatus(n)
    }
}

impl From<ExitStatus> for i32 {
    fn from(s: ExitStatus) -> i32 {
        s.0
    }
}

impl fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
