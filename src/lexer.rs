use crate::error::{ShellError, Span};

/// Token types produced by the lexer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// A word (command name, argument, filename, etc.)
    /// Contains the raw text; quoting/expansion is resolved later by the parser
    /// when building WordPart nodes.
    Word(String),

    /// An assignment word: `name=value` at the start of a simple command.
    /// The lexer recognizes these so the parser can distinguish assignments
    /// from ordinary arguments.
    Assignment { name: String, value: String },

    /// Operators
    Newline,       // \n (significant in shell grammar)
    Semi,          // ;
    Amp,           // &
    Pipe,          // |
    And,           // &&
    Or,            // ||
    SemiSemi,      // ;; (case)
    Less,          // <
    Great,         // >
    DLess,         // <<
    DGreat,        // >>
    LessAnd,      // <&
    GreatAnd,     // >&
    LessGreat,    // <>
    DLessDash,    // <<-
    Clobber,       // >|
    LParen,        // (
    RParen,        // )

    /// Reserved words (recognized when expected by grammar)
    If,
    Then,
    Else,
    Elif,
    Fi,
    Do,
    Done,
    Case,
    Esac,
    While,
    Until,
    For,
    In,
    Lbrace,  // {
    Rbrace,  // }
    Bang,    // !

    /// End of input
    Eof,
}

impl Token {
    /// Check if this is a redirection operator.
    pub fn is_redir(&self) -> bool {
        matches!(
            self,
            Token::Less
                | Token::Great
                | Token::DLess
                | Token::DGreat
                | Token::LessAnd
                | Token::GreatAnd
                | Token::LessGreat
                | Token::DLessDash
                | Token::Clobber
        )
    }
}

/// Pending here-document that needs its body read.
#[derive(Debug)]
pub struct PendingHereDoc {
    pub delimiter: String,
    pub strip_tabs: bool,
    pub quoted: bool,
}

/// Shell lexer / tokenizer.
///
/// Converts a source string into a stream of tokens. Handles:
/// - Operator recognition (|, &&, ||, ;, &, redirections)
/// - Reserved word recognition (when enabled)
/// - Quoting (single quotes, double quotes, backslash)
/// - Here-document delimiter collection
/// - Comment stripping
///
/// Does NOT handle:
/// - Word expansion (that's the parser + expander's job)
/// - Here-document body reading (parser calls `read_heredoc_body` after
///   seeing a complete command with heredoc redirections)
pub struct Lexer {
    src: Vec<char>,
    pos: usize,
    line: u32,
    col: u32,

    /// Pushback: if set, next `next_token` returns this instead of scanning.
    pushback: Option<(Token, Span)>,

    /// Whether the next word should be checked against reserved word list.
    /// Set by the parser depending on grammar context.
    pub recognize_reserved: bool,

    /// Pending here-documents waiting for body content.
    pub pending_heredocs: Vec<PendingHereDoc>,
}

impl Lexer {
    pub fn new(source: &str) -> Self {
        Lexer {
            src: source.chars().collect(),
            pos: 0,
            line: 1,
            col: 1,
            pushback: None,
            recognize_reserved: true,
            pending_heredocs: Vec::new(),
        }
    }

    /// Return the current source position.
    pub fn span(&self) -> Span {
        Span {
            offset: self.pos,
            line: self.line,
            col: self.col,
        }
    }

    /// Push a token back so it will be returned by the next `next_token` call.
    pub fn push_back(&mut self, tok: Token, span: Span) {
        debug_assert!(self.pushback.is_none(), "double pushback");
        self.pushback = Some((tok, span));
    }

    /// Peek at the next character without consuming it.
    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    /// Consume and return the next character, updating position tracking.
    fn advance(&mut self) -> Option<char> {
        let ch = self.src.get(self.pos).copied()?;
        self.pos += 1;
        if ch == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(ch)
    }

    /// Skip whitespace (spaces and tabs, NOT newlines — those are tokens).
    fn skip_blanks(&mut self) {
        while let Some(ch) = self.peek() {
            if ch == ' ' || ch == '\t' {
                self.advance();
            } else if ch == '#' {
                // Comments run to end of line
                while let Some(c) = self.peek() {
                    if c == '\n' {
                        break;
                    }
                    self.advance();
                }
            } else {
                break;
            }
        }
    }

    /// Read the next token from the source.
    pub fn next_token(&mut self) -> std::result::Result<(Token, Span), ShellError> {
        if let Some((tok, span)) = self.pushback.take() {
            return Ok((tok, span));
        }

        self.skip_blanks();

        let span = self.span();

        let ch = match self.peek() {
            None => return Ok((Token::Eof, span)),
            Some(c) => c,
        };

        // Newline
        if ch == '\n' {
            self.advance();
            return Ok((Token::Newline, span));
        }

        // Operators (multi-character first)
        if let Some(tok) = self.try_operator(span)? {
            return Ok(tok);
        }

        // Word token (includes quoted strings, escapes, etc.)
        self.read_word(span)
    }

    /// Try to read an operator token. Returns None if the current char
    /// doesn't start an operator.
    fn try_operator(
        &mut self,
        span: Span,
    ) -> std::result::Result<Option<(Token, Span)>, ShellError> {
        let ch = match self.peek() {
            Some(c) => c,
            None => return Ok(None),
        };

        let tok = match ch {
            ';' => {
                self.advance();
                if self.peek() == Some(';') {
                    self.advance();
                    Token::SemiSemi
                } else {
                    Token::Semi
                }
            }
            '&' => {
                self.advance();
                if self.peek() == Some('&') {
                    self.advance();
                    Token::And
                } else {
                    Token::Amp
                }
            }
            '|' => {
                self.advance();
                if self.peek() == Some('|') {
                    self.advance();
                    Token::Or
                } else {
                    Token::Pipe
                }
            }
            '(' => {
                self.advance();
                Token::LParen
            }
            ')' => {
                self.advance();
                Token::RParen
            }
            '<' => {
                self.advance();
                match self.peek() {
                    Some('<') => {
                        self.advance();
                        if self.peek() == Some('-') {
                            self.advance();
                            Token::DLessDash
                        } else {
                            Token::DLess
                        }
                    }
                    Some('&') => {
                        self.advance();
                        Token::LessAnd
                    }
                    Some('>') => {
                        self.advance();
                        Token::LessGreat
                    }
                    _ => Token::Less,
                }
            }
            '>' => {
                self.advance();
                match self.peek() {
                    Some('>') => {
                        self.advance();
                        Token::DGreat
                    }
                    Some('&') => {
                        self.advance();
                        Token::GreatAnd
                    }
                    Some('|') => {
                        self.advance();
                        Token::Clobber
                    }
                    _ => Token::Great,
                }
            }
            _ => return Ok(None),
        };

        Ok(Some((tok, span)))
    }

    /// Read a word token. Handles quoting (single, double, backslash),
    /// and IO_NUMBER detection (digits before < or >).
    fn read_word(
        &mut self,
        span: Span,
    ) -> std::result::Result<(Token, Span), ShellError> {
        let mut word = String::new();

        loop {
            match self.peek() {
                None => break,
                Some(ch) => match ch {
                    // Word delimiters
                    ' ' | '\t' | '\n' | ';' | '&' | '(' | ')' | '|' => break,

                    // Redirection operators end a word, BUT if the word so far
                    // is all digits, it's an IO_NUMBER — include it as part
                    // of the word so the parser can extract the fd number.
                    '<' | '>' => {
                        // If we haven't accumulated anything yet, this is an
                        // operator, not a word — but try_operator already
                        // handled that case. If we reach here, we have word
                        // content and hit a redirect.
                        break;
                    }

                    // Backslash escape
                    '\\' => {
                        self.advance();
                        // Line continuation: backslash-newline is consumed entirely
                        if self.peek() == Some('\n') {
                            self.advance();
                            continue;
                        }
                        word.push('\\');
                        if let Some(escaped) = self.advance() {
                            word.push(escaped);
                        }
                    }

                    // Single quotes — pass through as-is (parser interprets)
                    '\'' => {
                        word.push('\'');
                        self.advance();
                        loop {
                            match self.advance() {
                                None => {
                                    return Err(ShellError::Syntax {
                                        msg: "unterminated single quote".into(),
                                        span,
                                    });
                                }
                                Some('\'') => {
                                    word.push('\'');
                                    break;
                                }
                                Some(c) => word.push(c),
                            }
                        }
                    }

                    // Double quotes — pass through (parser handles expansions)
                    '"' => {
                        word.push('"');
                        self.advance();
                        self.read_double_quoted(&mut word, span)?;
                        word.push('"');
                    }

                    // Backtick — pass through for parser
                    '`' => {
                        word.push('`');
                        self.advance();
                        self.read_backtick(&mut word, span)?;
                        word.push('`');
                    }

                    // Dollar — could be $var, ${...}, $(...), $((...))
                    // Pass through raw for parser to interpret
                    '$' => {
                        word.push('$');
                        self.advance();
                        // Consume backslash-newline between $ and what follows
                        while self.peek() == Some('\\') {
                            if self.src.get(self.pos + 1).copied() == Some('\n') {
                                self.advance();
                                self.advance();
                            } else {
                                break;
                            }
                        }
                        self.read_dollar(&mut word, span)?;
                    }

                    // Comment (only valid at word boundary, but if we're
                    // mid-word we shouldn't see this — skip_blanks handles it)
                    '#' if word.is_empty() => break,

                    // Regular character
                    _ => {
                        word.push(ch);
                        self.advance();
                    }
                },
            }
        }

        if word.is_empty() {
            // Shouldn't happen: we checked for operators and EOF already
            return Err(ShellError::Syntax {
                msg: "unexpected character".into(),
                span,
            });
        }

        // Check for reserved words
        if self.recognize_reserved {
            if let Some(tok) = Self::reserved_word(&word) {
                return Ok((tok, span));
            }
        }

        // Check for assignment words: name=value where name is a valid
        // variable name (starts with letter/underscore, contains alnum/underscore)
        if let Some(eq_pos) = word.find('=') {
            let name = &word[..eq_pos];
            if !name.is_empty() && is_name(name) {
                return Ok((
                    Token::Assignment {
                        name: name.to_string(),
                        value: word[eq_pos + 1..].to_string(),
                    },
                    span,
                ));
            }
        }

        Ok((Token::Word(word), span))
    }

    /// Read inside double quotes until the closing `"`.
    /// Handles nested `$()`, `${}`, backticks, and backslash escapes.
    fn read_double_quoted(
        &mut self,
        word: &mut String,
        span: Span,
    ) -> std::result::Result<(), ShellError> {
        loop {
            match self.advance() {
                None => {
                    return Err(ShellError::Syntax {
                        msg: "unterminated double quote".into(),
                        span,
                    });
                }
                Some('"') => return Ok(()),
                Some('\\') => {
                    word.push('\\');
                    if let Some(c) = self.advance() {
                        word.push(c);
                    }
                }
                Some('$') => {
                    word.push('$');
                    self.read_dollar(word, span)?;
                }
                Some('`') => {
                    word.push('`');
                    self.read_backtick(word, span)?;
                    word.push('`');
                }
                Some(c) => word.push(c),
            }
        }
    }

    /// After consuming `$`, read what follows: `{...}`, `((...))`, `(...)`,
    /// or a variable name. Appends raw text to `word`.
    fn read_dollar(
        &mut self,
        word: &mut String,
        span: Span,
    ) -> std::result::Result<(), ShellError> {
        match self.peek() {
            Some('{') => {
                word.push('{');
                self.advance();
                self.read_brace_param(word, span)?;
                word.push('}');
            }
            Some('(') => {
                self.advance();
                // Check for $(( — arithmetic
                if self.peek() == Some('(') {
                    self.advance();
                    word.push('(');
                    word.push('(');
                    self.read_arith(word, span)?;
                    word.push(')');
                    word.push(')');
                } else {
                    word.push('(');
                    self.read_cmd_subst(word, span)?;
                    word.push(')');
                }
            }
            // Special parameters: $@, $*, $#, $?, $-, $$, $!, $0-$9
            Some(c @ ('@' | '*' | '#' | '?' | '-' | '$' | '!' | '0'..='9')) => {
                word.push(c);
                self.advance();
            }
            // Variable name
            Some(c) if c == '_' || c.is_ascii_alphabetic() => {
                word.push(c);
                self.advance();
                while let Some(c) = self.peek() {
                    if c == '_' || c.is_ascii_alphanumeric() {
                        word.push(c);
                        self.advance();
                    } else {
                        break;
                    }
                }
            }
            // Bare $ — literal
            _ => {}
        }
        Ok(())
    }

    /// Read `${...}` content after the opening `{`. Handles nested expansions.
    fn read_brace_param(
        &mut self,
        word: &mut String,
        span: Span,
    ) -> std::result::Result<(), ShellError> {
        let mut depth = 1u32;
        loop {
            match self.advance() {
                None => {
                    return Err(ShellError::Syntax {
                        msg: "unterminated ${".into(),
                        span,
                    });
                }
                Some('}') => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                    word.push('}');
                }
                Some('$') => {
                    word.push('$');
                    self.read_dollar(word, span)?;
                }
                Some('\'') => {
                    word.push('\'');
                    // Inside ${}, single quotes are literal in POSIX
                    // (they don't quote). Just read the char.
                }
                Some('"') => {
                    word.push('"');
                    self.read_double_quoted(word, span)?;
                    word.push('"');
                }
                Some('\\') => {
                    word.push('\\');
                    if let Some(c) = self.advance() {
                        word.push(c);
                    }
                }
                Some('`') => {
                    word.push('`');
                    self.read_backtick(word, span)?;
                    word.push('`');
                }
                Some(c) => word.push(c),
            }
        }
    }

    /// Read `$(...)` command substitution content after the opening `(`.
    fn read_cmd_subst(
        &mut self,
        word: &mut String,
        span: Span,
    ) -> std::result::Result<(), ShellError> {
        let mut depth = 1u32;
        loop {
            match self.advance() {
                None => {
                    return Err(ShellError::Syntax {
                        msg: "unterminated $(".into(),
                        span,
                    });
                }
                Some(')') => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                    word.push(')');
                }
                Some('(') => {
                    depth += 1;
                    word.push('(');
                }
                Some('\'') => {
                    word.push('\'');
                    loop {
                        match self.advance() {
                            None => {
                                return Err(ShellError::Syntax {
                                    msg: "unterminated single quote in $()".into(),
                                    span,
                                });
                            }
                            Some('\'') => {
                                word.push('\'');
                                break;
                            }
                            Some(c) => word.push(c),
                        }
                    }
                }
                Some('"') => {
                    word.push('"');
                    self.read_double_quoted(word, span)?;
                    word.push('"');
                }
                Some('\\') => {
                    word.push('\\');
                    if let Some(c) = self.advance() {
                        word.push(c);
                    }
                }
                Some('$') => {
                    word.push('$');
                    self.read_dollar(word, span)?;
                }
                Some('`') => {
                    word.push('`');
                    self.read_backtick(word, span)?;
                    word.push('`');
                }
                Some('#') => {
                    // Comments inside $() are valid
                    word.push('#');
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        word.push(c);
                        self.advance();
                    }
                }
                Some(c) => word.push(c),
            }
        }
    }

    /// Read `$((...))` arithmetic content after the opening `((`.
    fn read_arith(
        &mut self,
        word: &mut String,
        span: Span,
    ) -> std::result::Result<(), ShellError> {
        // Need to find matching ))
        let mut depth = 1u32;
        loop {
            match self.advance() {
                None => {
                    return Err(ShellError::Syntax {
                        msg: "unterminated $((".into(),
                        span,
                    });
                }
                Some(')') if self.peek() == Some(')') && depth == 1 => {
                    self.advance();
                    return Ok(());
                }
                Some(')') => {
                    depth -= 1;
                    word.push(')');
                }
                Some('(') => {
                    depth += 1;
                    word.push('(');
                }
                Some('$') => {
                    word.push('$');
                    self.read_dollar(word, span)?;
                }
                Some(c) => word.push(c),
            }
        }
    }

    /// Read backtick command substitution content after the opening `` ` ``.
    fn read_backtick(
        &mut self,
        word: &mut String,
        span: Span,
    ) -> std::result::Result<(), ShellError> {
        loop {
            match self.advance() {
                None => {
                    return Err(ShellError::Syntax {
                        msg: "unterminated backtick".into(),
                        span,
                    });
                }
                Some('`') => return Ok(()),
                Some('\\') => {
                    // In backticks, backslash only escapes $, `, and \
                    word.push('\\');
                    if let Some(c) = self.advance() {
                        word.push(c);
                    }
                }
                Some(c) => word.push(c),
            }
        }
    }

    /// Read a here-document body. Called by the parser after it has seen
    /// a complete command line containing `<<` or `<<-` redirections.
    ///
    /// Reads lines until the delimiter is found alone on a line.
    /// If `strip_tabs`, leading tabs are removed from each line.
    pub fn read_heredoc_body(&mut self, heredoc: &PendingHereDoc) -> std::result::Result<String, ShellError> {
        let mut body = String::new();
        loop {
            // Read a line
            let mut line = String::new();

            // Strip leading tabs if <<-
            if heredoc.strip_tabs {
                while self.peek() == Some('\t') {
                    self.advance();
                }
            }
            loop {
                match self.advance() {
                    None => {
                        // EOF before delimiter — the collected body is
                        // what we have (POSIX says this is an error, but
                        // many shells tolerate it)
                        if !line.is_empty() {
                            body.push_str(&line);
                        }
                        return Ok(body);
                    }
                    Some('\n') => {
                        break;
                    }
                    Some(c) => {
                        line.push(c);
                    }
                }
            }

            if line == heredoc.delimiter {
                return Ok(body);
            }

            body.push_str(&line);
            body.push('\n');
        }
    }

    /// Check if a word is a reserved word, returning the corresponding token.
    fn reserved_word(word: &str) -> Option<Token> {
        match word {
            "if" => Some(Token::If),
            "then" => Some(Token::Then),
            "else" => Some(Token::Else),
            "elif" => Some(Token::Elif),
            "fi" => Some(Token::Fi),
            "do" => Some(Token::Do),
            "done" => Some(Token::Done),
            "case" => Some(Token::Case),
            "esac" => Some(Token::Esac),
            "while" => Some(Token::While),
            "until" => Some(Token::Until),
            "for" => Some(Token::For),
            "in" => Some(Token::In),
            "{" => Some(Token::Lbrace),
            "}" => Some(Token::Rbrace),
            "!" => Some(Token::Bang),
            _ => None,
        }
    }
}

/// Check if `s` is a valid shell variable name.
pub fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(src: &str) -> Vec<Token> {
        let mut lex = Lexer::new(src);
        let mut toks = Vec::new();
        loop {
            let (tok, _) = lex.next_token().unwrap();
            if tok == Token::Eof {
                break;
            }
            toks.push(tok);
        }
        toks
    }

    #[test]
    fn simple_command() {
        assert_eq!(tokens("echo hello"), vec![
            Token::Word("echo".into()),
            Token::Word("hello".into()),
        ]);
    }

    #[test]
    fn pipeline() {
        assert_eq!(tokens("ls | grep foo"), vec![
            Token::Word("ls".into()),
            Token::Pipe,
            Token::Word("grep".into()),
            Token::Word("foo".into()),
        ]);
    }

    #[test]
    fn and_or() {
        assert_eq!(tokens("a && b || c"), vec![
            Token::Word("a".into()),
            Token::And,
            Token::Word("b".into()),
            Token::Or,
            Token::Word("c".into()),
        ]);
    }

    #[test]
    fn redirections() {
        // "in" is a reserved word, so the lexer returns Token::In here.
        // The parser handles context: after a redir operator, it accepts
        // reserved words as filenames.
        assert_eq!(tokens("cat < in > out 2>&1"), vec![
            Token::Word("cat".into()),
            Token::Less,
            Token::In,
            Token::Great,
            Token::Word("out".into()),
            Token::Word("2".into()),
            Token::GreatAnd,
            Token::Word("1".into()),
        ]);
    }

    #[test]
    fn redir_filename() {
        // Non-reserved filenames work fine
        assert_eq!(tokens("cat < input.txt"), vec![
            Token::Word("cat".into()),
            Token::Less,
            Token::Word("input.txt".into()),
        ]);
    }

    #[test]
    fn single_quotes() {
        assert_eq!(tokens("echo 'hello world'"), vec![
            Token::Word("echo".into()),
            Token::Word("'hello world'".into()),
        ]);
    }

    #[test]
    fn double_quotes() {
        assert_eq!(tokens(r#"echo "hello $name""#), vec![
            Token::Word("echo".into()),
            Token::Word("\"hello $name\"".into()),
        ]);
    }

    #[test]
    fn command_substitution() {
        assert_eq!(tokens("echo $(date)"), vec![
            Token::Word("echo".into()),
            Token::Word("$(date)".into()),
        ]);
    }

    #[test]
    fn assignment() {
        let toks = tokens("FOO=bar");
        assert_eq!(toks, vec![
            Token::Assignment { name: "FOO".into(), value: "bar".into() },
        ]);
    }

    #[test]
    fn reserved_words() {
        assert_eq!(tokens("if true; then echo yes; fi"), vec![
            Token::If,
            Token::Word("true".into()),
            Token::Semi,
            Token::Then,
            Token::Word("echo".into()),
            Token::Word("yes".into()),
            Token::Semi,
            Token::Fi,
        ]);
    }

    #[test]
    fn background() {
        assert_eq!(tokens("sleep 10 &"), vec![
            Token::Word("sleep".into()),
            Token::Word("10".into()),
            Token::Amp,
        ]);
    }

    #[test]
    fn case_tokens() {
        assert_eq!(tokens(";;"), vec![Token::SemiSemi]);
    }

    #[test]
    fn dollar_brace() {
        assert_eq!(tokens("${var:-default}"), vec![
            Token::Word("${var:-default}".into()),
        ]);
    }

    #[test]
    fn arithmetic() {
        assert_eq!(tokens("$((1+2))"), vec![
            Token::Word("$((1+2))".into()),
        ]);
    }

    #[test]
    fn heredoc_operator() {
        assert_eq!(tokens("cat << EOF"), vec![
            Token::Word("cat".into()),
            Token::DLess,
            Token::Word("EOF".into()),
        ]);
    }

    #[test]
    fn comments() {
        assert_eq!(tokens("echo hello # comment"), vec![
            Token::Word("echo".into()),
            Token::Word("hello".into()),
        ]);
    }

    #[test]
    fn backslash_newline() {
        assert_eq!(tokens("echo hel\\\nlo"), vec![
            Token::Word("echo".into()),
            Token::Word("hello".into()),
        ]);
    }

    #[test]
    fn empty_input() {
        assert_eq!(tokens(""), Vec::<Token>::new());
    }

    #[test]
    fn unterminated_single_quote() {
        let mut lex = Lexer::new("echo 'unterminated");
        // First token is fine
        let _ = lex.next_token().unwrap();
        // Second token should error
        assert!(lex.next_token().is_err());
    }
}
