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
    Exit(i32),
    /// `return [n]` — return from function or dot-script
    Return(i32),
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
