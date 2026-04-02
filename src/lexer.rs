use crate::ast::{Command, ParamExpr, ParamOp, WordPart};
use crate::error::{ShellError, Span};

/// Token types produced by the lexer.
#[derive(Debug, Clone)]
pub enum Token {
    /// A word (command name, argument, filename, etc.)
    /// Contains pre-parsed WordPart nodes — no re-parsing needed.
    /// `had_quoting` is true if any quoting (', ", \) was present in the source.
    Word(Vec<WordPart>, bool),

    /// An assignment word: `name=value` at the start of a simple command.
    /// The lexer recognizes these so the parser can distinguish assignments
    /// from ordinary arguments.
    Assignment {
        name: String,
        value: Vec<WordPart>,
    },

    /// Operators
    Newline, // \n (significant in shell grammar)
    Semi,      // ;
    Amp,       // &
    Pipe,      // |
    And,       // &&
    Or,        // ||
    SemiSemi,  // ;; (case)
    Less,      // <
    Great,     // >
    DLess,     // <<
    DGreat,    // >>
    LessAnd,   // <&
    GreatAnd,  // >&
    LessGreat, // <>
    DLessDash, // <<-
    Clobber,   // >|
    LParen,    // (
    RParen,    // )

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
    Lbrace, // {
    Rbrace, // }
    Bang,   // !

    /// End of input
    Eof,
}

impl PartialEq for Token {
    fn eq(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }
}

impl Eq for Token {}

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
/// - Single-pass word tokenization into Vec<WordPart>
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

    /// Peek at the next character without consuming it (raw, no backslash-newline eating).
    pub(crate) fn peek_raw(&self) -> Option<char> {
        self.src.get(self.pos).copied()
    }

    /// Peek at the next character, transparently consuming any `\<newline>` sequences.
    /// Mirrors dash's `pgetc_eatbnl()` — used in most contexts except single quotes
    /// and heredoc body reading.
    fn peek(&mut self) -> Option<char> {
        loop {
            let ch = self.src.get(self.pos).copied()?;
            if ch == '\\' && self.src.get(self.pos + 1).copied() == Some('\n') {
                // Consume the backslash-newline continuation
                self.pos += 2;
                self.line += 1;
                self.col = 1;
                continue;
            }
            return Some(ch);
        }
    }

    /// Consume and return the next character, updating position tracking (raw).
    pub(crate) fn advance_raw(&mut self) -> Option<char> {
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

    /// Consume and return the next character, eating `\<newline>` continuations.
    fn advance(&mut self) -> Option<char> {
        loop {
            let ch = self.src.get(self.pos).copied()?;
            if ch == '\\' && self.src.get(self.pos + 1).copied() == Some('\n') {
                self.pos += 2;
                self.line += 1;
                self.col = 1;
                continue;
            }
            self.pos += 1;
            if ch == '\n' {
                self.line += 1;
                self.col = 1;
            } else {
                self.col += 1;
            }
            return Some(ch);
        }
    }

    /// Skip whitespace (spaces and tabs, NOT newlines — those are tokens).
    fn skip_blanks(&mut self) {
        while let Some(ch) = self.peek() {
            if ch == ' ' || ch == '\t' {
                self.advance();
            } else if ch == '#' {
                // Comments run to end of line (use raw — \<newline> doesn't continue comments)
                while let Some(c) = self.peek_raw() {
                    if c == '\n' {
                        break;
                    }
                    self.advance_raw();
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

    /// Read a word token. Produces Vec<WordPart> directly (single-pass).
    fn read_word(&mut self, span: Span) -> std::result::Result<(Token, Span), ShellError> {
        let (parts, had_quoting) = self.read_word_parts(false, span)?;

        if parts.is_empty() {
            return Err(ShellError::Syntax {
                msg: "unexpected character".into(),
                span,
            });
        }

        // Check for reserved words: single Literal that matches a reserved word
        if self.recognize_reserved
            && let Some(text) = single_literal_text(&parts)
            && let Some(tok) = Self::reserved_word(text)
        {
            return Ok((tok, span));
        }

        // Check for assignment words: name=value
        // The parts start with a Literal containing "name=..." — split it
        if let Some((name, value_parts)) = try_split_assignment(&parts) {
            return Ok((Token::Assignment { name, value: value_parts }, span));
        }

        // IO_NUMBER detection: if word is 1-2 digits, no quoting, and next char
        // is < or >, treat as a redirect fd number (not a word). Push the word
        // text back as context for the redirect token that follows.
        if !had_quoting
            && let Some(text) = single_literal_text(&parts)
            && text.len() <= 2
            && text.chars().all(|c| c.is_ascii_digit())
            && matches!(self.peek_raw(), Some('<' | '>'))
        {
            return Ok((Token::Word(parts, had_quoting), span));
        }

        Ok((Token::Word(parts, had_quoting), span))
    }

    /// Main recursive word-part builder. Reads characters and produces WordPart nodes.
    /// `in_dquote`: whether we're inside double quotes (affects quoting rules).
    /// Stops at word delimiters in unquoted context.
    /// Returns (parts, had_quoting) — had_quoting is true if any quoting was encountered
    /// (single quotes, double quotes, backslash escapes).
    fn read_word_parts(
        &mut self,
        in_dquote: bool,
        span: Span,
    ) -> std::result::Result<(Vec<WordPart>, bool), ShellError> {
        let mut parts = Vec::new();
        let mut literal = String::new();
        let at_start = true;
        let mut had_quoting = in_dquote; // if already in dquote, quoting occurred

        loop {
            let ch = if in_dquote {
                // Inside double quotes, we use advance() which eats \<newline>
                self.peek()
            } else {
                self.peek()
            };

            match ch {
                None => break,
                Some(ch) => {
                    if !in_dquote {
                        // Word delimiters in unquoted context
                        match ch {
                            ' ' | '\t' | '\n' | ';' | '&' | '(' | ')' | '|' | '<' | '>' => break,
                            '#' if parts.is_empty() && literal.is_empty() => break,
                            _ => {}
                        }
                    }

                    match ch {
                        '"' if !in_dquote => {
                            // Double quote: collect inner parts
                            if !literal.is_empty() {
                                parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                            }
                            self.advance(); // consume opening "
                            let inner = self.read_dquote_parts(span)?;
                            had_quoting = true;                            parts.push(WordPart::DoubleQuoted(inner));
                        }
                        '"' if in_dquote => {
                            // Closing double quote — stop collecting
                            break;
                        }
                        '\'' if !in_dquote => {
                            // Single quote: read until closing '
                            if !literal.is_empty() {
                                parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                            }
                            self.advance_raw(); // consume opening '
                            let mut content = String::new();
                            loop {
                                match self.advance_raw() {
                                    None => {
                                        return Err(ShellError::Syntax {
                                            msg: "unterminated single quote".into(),
                                            span,
                                        });
                                    }
                                    Some('\'') => break,
                                    Some(c) => content.push(c),
                                }
                            }
                            had_quoting = true;                            parts.push(WordPart::SingleQuoted(content));
                        }
                        '\\' if in_dquote => {
                            had_quoting = true; self.advance(); // consume backslash
                            if let Some(c) = self.advance() {
                                // In double quotes, backslash only escapes $, `, ", \, and newline
                                if matches!(c, '$' | '`' | '"' | '\\' | '\n') {
                                    if c != '\n' {
                                        literal.push(c);
                                    }
                                } else {
                                    literal.push('\\');
                                    literal.push(c);
                                }
                            }
                        }
                        '\\' => {
                            // Unquoted backslash — preserve \ for glob chars so
                            // fnmatch can distinguish \? (literal) from ? (glob).
                            // Quote removal strips these during normal expansion.
                            had_quoting = true; self.advance(); // consume backslash
                            if let Some(escaped) = self.advance() {
                                if matches!(escaped, '*' | '?' | '[' | ']') {
                                    literal.push('\\');
                                }
                                literal.push(escaped);
                            }
                        }
                        '$' => {
                            if !literal.is_empty() {
                                parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                            }
                            self.advance(); // consume $
                            if let Some(part) = self.read_dollar(in_dquote, span)? {
                                parts.push(part);
                            } else {
                                // Bare $
                                literal.push('$');
                            }
                        }
                        '`' => {
                            if !literal.is_empty() {
                                parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                            }
                            self.advance(); // consume opening `
                            let part = self.read_backtick_part(span)?;
                            parts.push(part);
                        }
                        '~' if !in_dquote && parts.is_empty() && literal.is_empty() && at_start => {
                            // Tilde expansion at word start
                            self.advance(); // consume ~
                            let mut user = String::new();
                            while let Some(c) = self.peek() {
                                if c == '/' || c == ':' || c.is_whitespace()
                                    || matches!(c, ';' | '&' | '|' | '<' | '>' | '(' | ')')
                                {
                                    break;
                                }
                                user.push(c);
                                self.advance();
                            }
                            parts.push(WordPart::Tilde(user));
                        }
                        _ => {
                            literal.push(ch);
                            self.advance();
                        }
                    }
                }
            }
        }

        if !literal.is_empty() {
            parts.push(WordPart::Literal(literal));
        }

        Ok((coalesce_literals(parts), had_quoting))
    }

    /// Read parts inside double quotes until closing `"`.
    fn read_dquote_parts(
        &mut self,
        span: Span,
    ) -> std::result::Result<Vec<WordPart>, ShellError> {
        let (parts, _) = self.read_word_parts(true, span)?;
        // Consume the closing "
        match self.peek() {
            Some('"') => {
                self.advance();
            }
            _ => {
                return Err(ShellError::Syntax {
                    msg: "unterminated double quote".into(),
                    span,
                });
            }
        }
        Ok(parts)
    }

    /// After consuming `$`, read what follows and produce a WordPart.
    fn read_dollar(
        &mut self,
        in_dquote: bool,
        span: Span,
    ) -> std::result::Result<Option<WordPart>, ShellError> {
        match self.peek() {
            Some('{') => {
                self.advance(); // consume {
                let part = self.read_brace_param(in_dquote, span)?;
                Ok(Some(part))
            }
            Some('(') => {
                self.advance(); // consume first (
                if self.peek() == Some('(') {
                    self.advance(); // consume second (
                    let part = self.read_arith_expansion(span)?;
                    Ok(Some(part))
                } else {
                    let part = self.read_cmd_subst(span)?;
                    Ok(Some(part))
                }
            }
            // Special parameters
            Some(c @ ('@' | '*' | '#' | '?' | '-' | '$' | '!')) => {
                self.advance();
                Ok(Some(WordPart::Param(ParamExpr {
                    name: c.to_string(),
                    op: ParamOp::Normal,
                    span: Span::default(),
                })))
            }
            // Positional parameters $0-$9
            Some(c @ '0'..='9') => {
                self.advance();
                Ok(Some(WordPart::Param(ParamExpr {
                    name: c.to_string(),
                    op: ParamOp::Normal,
                    span: Span::default(),
                })))
            }
            // Variable name
            Some(c) if c == '_' || c.is_ascii_alphabetic() => {
                let mut name = String::new();
                name.push(c);
                self.advance();
                while let Some(c) = self.peek() {
                    if c == '_' || c.is_ascii_alphanumeric() {
                        name.push(c);
                        self.advance();
                    } else {
                        break;
                    }
                }
                Ok(Some(WordPart::Param(ParamExpr {
                    name,
                    op: ParamOp::Normal,
                    span: Span::default(),
                })))
            }
            // Bare $
            _ => Ok(None),
        }
    }

    /// Read `${...}` parameter expansion after `${` has been consumed.
    fn read_brace_param(
        &mut self,
        in_dquote: bool,
        span: Span,
    ) -> std::result::Result<WordPart, ShellError> {
        // ${#var} — length prefix. But ${##pattern} is $# with trim, not length of #.
        // Mirrors dash parsesub lines 1400-1418.
        let mut length = false;
        if self.peek() == Some('#') {
            let next = self.src.get(self.pos + 1).copied();
            if let Some(n) = next {
                if n == '_' || n.is_ascii_alphabetic() {
                    // ${#name} — length of variable
                    length = true;
                } else if n == '}' {
                    // ${#} — value of $# (not length)
                    length = false;
                } else if n == '#' || n == '?' || n == '-' || n == '!' || n == '$' || n == '@' || n == '*' {
                    // ${##...} ${#?} etc — check if it's ${#X} or ${X op}
                    // If char after the special param is }, it's length. Otherwise it's a param+op.
                    let after = self.src.get(self.pos + 2).copied();
                    if after == Some('}') {
                        // ${#?} = length of $?
                        length = true;
                    } else {
                        // ${##pat} = $# with trim
                        length = false;
                    }
                } else if n.is_ascii_digit() {
                    // ${#1} — length of $1
                    length = true;
                }
            }
        }
        if length {
            self.advance(); // consume #
        }

        // Read variable name
        let name = self.read_param_name();

        if length {
            // Skip to closing }
            self.skip_to_close_brace(span)?;
            return Ok(WordPart::Param(ParamExpr {
                name,
                op: ParamOp::Length,
                span: Span::default(),
            }));
        }

        // Check for operator or closing }
        match self.peek() {
            Some('}') => {
                self.advance();
                return Ok(WordPart::Param(ParamExpr {
                    name,
                    op: ParamOp::Normal,
                    span: Span::default(),
                }));
            }
            None => {
                return Err(ShellError::Syntax {
                    msg: "unterminated ${".into(),
                    span,
                });
            }
            _ => {}
        }

        let op_char = self.peek().unwrap();

        // Validate operator character
        if !matches!(op_char, ':' | '-' | '=' | '?' | '+' | '%' | '#') {
            // Bad substitution
            let bad_name = format!("{}{}", name, op_char);
            self.advance(); // consume bad char
            self.skip_to_close_brace(span)?;
            return Ok(WordPart::Param(ParamExpr {
                name: bad_name,
                op: ParamOp::BadSubst,
                span: Span::default(),
            }));
        }

        self.advance(); // consume op_char

        let op = match op_char {
            ':' => {
                match self.peek() {
                    Some(op2 @ ('-' | '=' | '?' | '+')) => {
                        self.advance(); // consume op2
                        let word = self.read_brace_word_parts(in_dquote, span)?;
                        match op2 {
                            '-' => ParamOp::Default { colon: true, word },
                            '=' => ParamOp::Assign { colon: true, word },
                            '?' => ParamOp::Error { colon: true, word },
                            '+' => ParamOp::Alternative { colon: true, word },
                            _ => unreachable!(),
                        }
                    }
                    _ => {
                        // Just colon with no valid op2 — treat as Normal
                        self.skip_to_close_brace(span)?;
                        ParamOp::Normal
                    }
                }
            }
            '-' => {
                let word = self.read_brace_word_parts(in_dquote, span)?;
                ParamOp::Default { colon: false, word }
            }
            '=' => {
                let word = self.read_brace_word_parts(in_dquote, span)?;
                ParamOp::Assign { colon: false, word }
            }
            '?' => {
                let word = self.read_brace_word_parts(in_dquote, span)?;
                ParamOp::Error { colon: false, word }
            }
            '+' => {
                let word = self.read_brace_word_parts(in_dquote, span)?;
                ParamOp::Alternative { colon: false, word }
            }
            // Trim ops: ALWAYS use BASESYNTAX (single quotes are quoting)
            // This matches dash's parsesub() which forces newsyn=BASESYNTAX for % and #
            '%' => {
                if self.peek() == Some('%') {
                    self.advance();
                    let word = self.read_brace_word_parts(false, span)?;
                    ParamOp::TrimSuffixLarge(word)
                } else {
                    let word = self.read_brace_word_parts(false, span)?;
                    ParamOp::TrimSuffixSmall(word)
                }
            }
            '#' => {
                if self.peek() == Some('#') {
                    self.advance();
                    let word = self.read_brace_word_parts(false, span)?;
                    ParamOp::TrimPrefixLarge(word)
                } else {
                    let word = self.read_brace_word_parts(false, span)?;
                    ParamOp::TrimPrefixSmall(word)
                }
            }
            _ => {
                self.skip_to_close_brace(span)?;
                ParamOp::Normal
            }
        };

        Ok(WordPart::Param(ParamExpr {
            name,
            op,
            span: Span::default(),
        }))
    }

    /// Read the variable name portion of a ${...} expansion.
    fn read_param_name(&mut self) -> String {
        let mut name = String::new();
        match self.peek() {
            // Special params
            Some(c @ ('@' | '*' | '#' | '?' | '-' | '$' | '!')) => {
                self.advance();
                name.push(c);
            }
            // Positional
            Some(c) if c.is_ascii_digit() => {
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() {
                        name.push(c);
                        self.advance();
                    } else {
                        break;
                    }
                }
            }
            // Regular variable name
            _ => {
                while let Some(c) = self.peek() {
                    if c == '_' || c.is_ascii_alphanumeric() {
                        name.push(c);
                        self.advance();
                    } else {
                        break;
                    }
                }
            }
        }
        name
    }

    /// Skip to closing `}` for error recovery in brace params.
    fn skip_to_close_brace(&mut self, span: Span) -> std::result::Result<(), ShellError> {
        loop {
            match self.advance() {
                Some('}') => return Ok(()),
                None => {
                    return Err(ShellError::Syntax {
                        msg: "unterminated ${".into(),
                        span,
                    });
                }
                _ => {}
            }
        }
    }

    /// Read word parts inside `${var<op>...}` until the closing `}`.
    /// Tracks brace depth for nested `${...}`.
    /// Read parts inside inner double quotes within `${...}` when already in dquote.
    /// This is dash's innerdq toggle — content is effectively unquoted until closing ".
    fn read_brace_dquote_toggle_parts(
        &mut self,
        span: Span,
    ) -> std::result::Result<Vec<WordPart>, ShellError> {
        let mut parts = Vec::new();
        let mut literal = String::new();

        loop {
            match self.peek() {
                None => {
                    return Err(ShellError::Syntax {
                        msg: "unterminated inner double quote in ${}".into(),
                        span,
                    });
                }
                Some('"') => {
                    self.advance(); // consume closing inner "
                    break;
                }
                Some('$') => {
                    self.advance();
                    if !literal.is_empty() {
                        parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                    }
                    if let Some(part) = self.read_dollar(false, span)? {
                        parts.push(part);
                    } else {
                        literal.push('$');
                    }
                }
                Some('\\') => {
                    self.advance();
                    if let Some(c) = self.advance() {
                        literal.push(c);
                    }
                }
                Some('`') => {
                    if !literal.is_empty() {
                        parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                    }
                    self.advance();
                    parts.push(self.read_backtick_part(span)?);
                }
                Some(c) => {
                    literal.push(c);
                    self.advance();
                }
            }
        }

        if !literal.is_empty() {
            parts.push(WordPart::Literal(literal));
        }
        Ok(parts)
    }

    fn read_brace_word_parts(
        &mut self,
        in_dquote: bool,
        span: Span,
    ) -> std::result::Result<Vec<WordPart>, ShellError> {
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut depth = 1u32;

        loop {
            match self.peek() {
                None => {
                    return Err(ShellError::Syntax {
                        msg: "unterminated ${".into(),
                        span,
                    });
                }
                Some('}') => {
                    depth -= 1;
                    if depth == 0 {
                        self.advance(); // consume closing }
                        break;
                    }
                    literal.push('}');
                    self.advance();
                }
                // Single quotes: quoting in unquoted context (BASESYNTAX for trim ops)
                Some('\'') if !in_dquote => {
                    if !literal.is_empty() {
                        parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                    }
                    self.advance_raw(); // consume opening '
                    let mut content = String::new();
                    loop {
                        match self.advance_raw() {
                            None => {
                                return Err(ShellError::Syntax {
                                    msg: "unterminated single quote".into(),
                                    span,
                                });
                            }
                            Some('\'') => break,
                            Some(c) => content.push(c),
                        }
                    }
                            parts.push(WordPart::SingleQuoted(content));
                }
                Some('"') if !in_dquote => {
                    if !literal.is_empty() {
                        parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                    }
                    self.advance(); // consume opening "
                    let inner = self.read_dquote_parts(span)?;
                            parts.push(WordPart::DoubleQuoted(inner));
                }
                Some('"') if in_dquote => {
                    // Inner double quote inside "${...}" toggles context (dash's innerdq).
                    // Content between inner quotes is effectively unquoted.
                    if !literal.is_empty() {
                        parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                    }
                    self.advance(); // consume opening inner "
                    // Read until matching " with unquoted rules
                    let inner = self.read_brace_dquote_toggle_parts(span)?;
                    parts.extend(inner);
                }
                Some('$') => {
                    self.advance(); // consume $
                    // Don't increment depth — read_dollar handles nested ${} internally
                    if !literal.is_empty() {
                        parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                    }
                    if let Some(part) = self.read_dollar(in_dquote, span)? {
                        parts.push(part);
                    } else {
                        literal.push('$');
                    }
                }
                Some('\\') => {
                    self.advance(); // consume backslash
                    if let Some(c) = self.advance() {
                        if in_dquote && matches!(c, '$' | '`' | '"' | '\\' | '\n') {
                            if c != '\n' {
                                literal.push(c);
                            }
                        } else if in_dquote {
                            literal.push('\\');
                            literal.push(c);
                        } else {
                            literal.push(c);
                        }
                    }
                }
                Some('`') => {
                    if !literal.is_empty() {
                        parts.push(WordPart::Literal(std::mem::take(&mut literal)));
                    }
                    self.advance(); // consume `
                    let part = self.read_backtick_part(span)?;
                    parts.push(part);
                }
                Some(c) => {
                    literal.push(c);
                    self.advance();
                }
            }
        }

        if !literal.is_empty() {
            parts.push(WordPart::Literal(literal));
        }

        Ok(coalesce_literals(parts))
    }

    /// Read `$(...)` command substitution after `$(` has been consumed.
    /// Collects raw text, then recursively parses it.
    fn read_cmd_subst(&mut self, span: Span) -> std::result::Result<WordPart, ShellError> {
        let content = self.read_cmd_subst_raw(span)?;
        let cmd = parse_cmdsubst_content(&content);
        Ok(WordPart::CmdSubst(Box::new(cmd)))
    }

    /// Read raw text of command substitution until matching `)`.
    fn read_cmd_subst_raw(&mut self, span: Span) -> std::result::Result<String, ShellError> {
        let mut content = String::new();
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
                        return Ok(content);
                    }
                    content.push(')');
                }
                Some('(') => {
                    depth += 1;
                    content.push('(');
                }
                Some('\'') => {
                    content.push('\'');
                    loop {
                        match self.advance() {
                            None => {
                                return Err(ShellError::Syntax {
                                    msg: "unterminated single quote in $()".into(),
                                    span,
                                });
                            }
                            Some('\'') => {
                                content.push('\'');
                                break;
                            }
                            Some(c) => content.push(c),
                        }
                    }
                }
                Some('"') => {
                    content.push('"');
                    self.read_double_quoted_raw(&mut content, span)?;
                    content.push('"');
                }
                Some('\\') => {
                    content.push('\\');
                    if let Some(c) = self.advance() {
                        content.push(c);
                    }
                }
                Some('$') => {
                    content.push('$');
                    self.read_dollar_raw(&mut content, span)?;
                }
                Some('`') => {
                    content.push('`');
                    self.read_backtick_raw(&mut content, span)?;
                    content.push('`');
                }
                Some('#') => {
                    // Comments inside $() are valid
                    content.push('#');
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        content.push(c);
                        self.advance();
                    }
                }
                Some(c) => content.push(c),
            }
        }
    }

    /// Read `$((expr))` arithmetic expansion after `$((` has been consumed.
    fn read_arith_expansion(&mut self, span: Span) -> std::result::Result<WordPart, ShellError> {
        let mut content = String::new();
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
                    let inner_parts = crate::parser::parse_word_parts(&content);
                    return Ok(WordPart::Arith(inner_parts));
                }
                Some(')') => {
                    depth -= 1;
                    content.push(')');
                }
                Some('(') => {
                    depth += 1;
                    content.push('(');
                }
                Some('$') => {
                    content.push('$');
                    self.read_dollar_raw(&mut content, span)?;
                }
                Some(c) => content.push(c),
            }
        }
    }

    /// Read backtick command substitution after opening `` ` `` consumed.
    fn read_backtick_part(&mut self, span: Span) -> std::result::Result<WordPart, ShellError> {
        let mut content = String::new();
        loop {
            match self.advance() {
                None => {
                    return Err(ShellError::Syntax {
                        msg: "unterminated backtick".into(),
                        span,
                    });
                }
                Some('`') => {
                    let cmd = parse_cmdsubst_content(&content);
                    return Ok(WordPart::Backtick(Box::new(cmd)));
                }
                Some('\\') => {
                    // In backticks, backslash only escapes $, `, and \
                    if let Some(c) = self.advance() {
                        if matches!(c, '$' | '`' | '\\') {
                            content.push(c);
                        } else {
                            content.push('\\');
                            content.push(c);
                        }
                    }
                }
                Some(c) => content.push(c),
            }
        }
    }

    /// Read inside double quotes, appending raw text to `word`.
    /// Used for raw helpers reading nested constructs inside $().
    fn read_double_quoted_raw(
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
                    self.read_dollar_raw(word, span)?;
                }
                Some('`') => {
                    word.push('`');
                    self.read_backtick_raw(word, span)?;
                    word.push('`');
                }
                Some(c) => word.push(c),
            }
        }
    }

    /// After consuming `$`, read what follows, appending raw text to `word`.
    fn read_dollar_raw(
        &mut self,
        word: &mut String,
        span: Span,
    ) -> std::result::Result<(), ShellError> {
        match self.peek() {
            Some('{') => {
                word.push('{');
                self.advance();
                self.read_brace_param_raw(word, span)?;
                word.push('}');
            }
            Some('(') => {
                self.advance();
                if self.peek() == Some('(') {
                    self.advance();
                    word.push('(');
                    word.push('(');
                    self.read_arith_raw(word, span)?;
                    word.push(')');
                    word.push(')');
                } else {
                    word.push('(');
                    self.read_cmd_subst_nested_raw(word, span)?;
                    word.push(')');
                }
            }
            Some(c @ ('@' | '*' | '#' | '?' | '-' | '$' | '!' | '0'..='9')) => {
                word.push(c);
                self.advance();
            }
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
            _ => {}
        }
        Ok(())
    }

    /// Read `${...}` raw content after the opening `{`.
    fn read_brace_param_raw(
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
                    self.read_dollar_raw(word, span)?;
                }
                Some('\'') => {
                    word.push('\'');
                }
                Some('"') => {
                    word.push('"');
                    self.read_double_quoted_raw(word, span)?;
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
                    self.read_backtick_raw(word, span)?;
                    word.push('`');
                }
                Some(c) => word.push(c),
            }
        }
    }

    /// Read `$(...)` raw content (nested inside another raw read).
    fn read_cmd_subst_nested_raw(
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
                    self.read_double_quoted_raw(word, span)?;
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
                    self.read_dollar_raw(word, span)?;
                }
                Some('`') => {
                    word.push('`');
                    self.read_backtick_raw(word, span)?;
                    word.push('`');
                }
                Some('#') => {
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

    /// Read `$((...))` raw content.
    fn read_arith_raw(
        &mut self,
        word: &mut String,
        span: Span,
    ) -> std::result::Result<(), ShellError> {
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
                    self.read_dollar_raw(word, span)?;
                }
                Some(c) => word.push(c),
            }
        }
    }

    /// Read backtick content, appending raw text to `word`.
    fn read_backtick_raw(
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
    pub fn read_heredoc_body(
        &mut self,
        heredoc: &PendingHereDoc,
    ) -> std::result::Result<String, ShellError> {
        let mut body = String::new();
        loop {
            let mut line = String::new();

            // Strip leading tabs if <<-
            if heredoc.strip_tabs {
                while self.peek_raw() == Some('\t') {
                    self.advance_raw();
                }
            }
            loop {
                match self.advance_raw() {
                    None => {
                        // EOF before newline — check if this line IS the delimiter
                        if line == heredoc.delimiter {
                            return Ok(body);
                        }
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

            // For unquoted heredocs, \<newline> is continuation — if line ends
            // with \, strip the backslash and join with the next physical line.
            // The joined result is checked as one logical line for delimiter matching.
            if !heredoc.quoted && line.ends_with('\\') {
                line.pop(); // remove trailing backslash
                // Don't break — keep accumulating into `line` by reading another line
                // Strip leading tabs again if <<-
                if heredoc.strip_tabs {
                    while self.peek_raw() == Some('\t') {
                        self.advance_raw();
                    }
                }
                // Read the continuation into the same line buffer
                loop {
                    match self.advance_raw() {
                        None => break,
                        Some('\n') => break,
                        Some(c) => line.push(c),
                    }
                }
                // Check again for continuation (could be multi-line)
                if !heredoc.quoted && line.ends_with('\\') {
                    // Recursive continuation — handle by looping
                    // For simplicity, just store and continue the outer loop
                    line.pop();
                    body.push_str(&line);
                    continue;
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

/// Extract the text from a word that is a single Literal part.
fn single_literal_text(parts: &[WordPart]) -> Option<&str> {
    if parts.len() == 1
        && let WordPart::Literal(s) = &parts[0]
    {
        return Some(s);
    }
    None
}

/// Try to split a word's parts into an assignment (name=value).
/// Returns Some((name, value_parts)) if the first Literal starts with `name=`.
fn try_split_assignment(parts: &[WordPart]) -> Option<(String, Vec<WordPart>)> {
    if let Some(WordPart::Literal(first)) = parts.first()
        && let Some(eq_pos) = first.find('=')
    {
        let name = &first[..eq_pos];
        if !name.is_empty() && is_name(name) {
            let name = name.to_string();
            let rest_of_first = &first[eq_pos + 1..];
            let mut value_parts = Vec::new();
            if !rest_of_first.is_empty() {
                value_parts.push(WordPart::Literal(rest_of_first.to_string()));
            }
            for part in &parts[1..] {
                value_parts.push(part.clone());
            }
            return Some((name, value_parts));
        }
    }
    None
}

/// Coalesce adjacent Literal parts.
fn coalesce_literals(parts: Vec<WordPart>) -> Vec<WordPart> {
    let mut result = Vec::with_capacity(parts.len());
    for part in parts {
        if let WordPart::Literal(ref s) = part
            && let Some(WordPart::Literal(prev)) = result.last_mut()
        {
            prev.push_str(s);
            continue;
        }
        result.push(part);
    }
    result
}

/// Extract the plain text from word parts (for heredoc delimiter, func name, for-var, etc.).
/// Only extracts from Literal and SingleQuoted parts.
pub fn parts_to_text(parts: &[WordPart]) -> String {
    let mut s = String::new();
    for part in parts {
        match part {
            WordPart::Literal(t) => s.push_str(t),
            WordPart::SingleQuoted(t) => s.push_str(t),
            WordPart::DoubleQuoted(inner) => {
                s.push_str(&parts_to_text(inner));
            }
            WordPart::Param(p) => {
                // Reconstruct source text for unexpanded params (used by heredoc delimiters)
                s.push('$');
                s.push_str(&p.name);
            }
            WordPart::Tilde(user) => {
                s.push('~');
                s.push_str(user);
            }
            _ => {} // CmdSubst, Backtick, Arith ignored
        }
    }
    s
}

/// Check if any part contains quoting (for heredoc quoted-delimiter detection).
pub fn parts_have_quoting(parts: &[WordPart]) -> bool {
    for part in parts {
        match part {
            WordPart::SingleQuoted(_) | WordPart::DoubleQuoted(_) => return true,
            WordPart::Literal(s) if s.contains('\\') => return true,
            // Any non-Literal part indicates quoting/expansion happened
            WordPart::Param(_)
            | WordPart::CmdSubst(_)
            | WordPart::Backtick(_)
            | WordPart::Arith(_)
            | WordPart::Tilde(_) => return true,
            _ => {}
        }
    }
    false
}

/// Parse command substitution content by recursively invoking the parser.
fn parse_cmdsubst_content(content: &str) -> Command {
    match crate::parser::Parser::new(content).parse() {
        Ok(program) => {
            if program.commands.is_empty() {
                Command::Simple {
                    assigns: Vec::new(),
                    args: Vec::new(),
                    redirs: Vec::new(),
                    span: Span::default(),
                }
            } else if program.commands.len() == 1 {
                program.commands.into_iter().next().unwrap()
            } else {
                let mut iter = program.commands.into_iter();
                let mut result = iter.next().unwrap();
                for cmd in iter {
                    result = Command::Sequence(Box::new(result), Box::new(cmd));
                }
                result
            }
        }
        Err(_) => Command::Simple {
            assigns: Vec::new(),
            args: Vec::new(),
            redirs: Vec::new(),
            span: Span::default(),
        },
    }
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

    /// Helper: check that a token is Word with a single Literal part.
    fn is_word(tok: &Token, expected: &str) -> bool {
        match tok {
            Token::Word(parts, _) => single_literal_text(parts) == Some(expected),
            _ => false,
        }
    }

    #[test]
    fn simple_command() {
        let toks = tokens("echo hello");
        assert_eq!(toks.len(), 2);
        assert!(is_word(&toks[0], "echo"));
        assert!(is_word(&toks[1], "hello"));
    }

    #[test]
    fn pipeline() {
        let toks = tokens("ls | grep foo");
        assert_eq!(toks.len(), 4);
        assert!(is_word(&toks[0], "ls"));
        assert_eq!(toks[1], Token::Pipe);
        assert!(is_word(&toks[2], "grep"));
        assert!(is_word(&toks[3], "foo"));
    }

    #[test]
    fn and_or() {
        let toks = tokens("a && b || c");
        assert_eq!(toks.len(), 5);
        assert!(is_word(&toks[0], "a"));
        assert_eq!(toks[1], Token::And);
        assert!(is_word(&toks[2], "b"));
        assert_eq!(toks[3], Token::Or);
        assert!(is_word(&toks[4], "c"));
    }

    #[test]
    fn redirections() {
        let toks = tokens("cat < in > out 2>&1");
        assert_eq!(toks.len(), 8);
        assert!(is_word(&toks[0], "cat"));
        assert_eq!(toks[1], Token::Less);
        assert_eq!(toks[2], Token::In);
        assert_eq!(toks[3], Token::Great);
        assert!(is_word(&toks[4], "out"));
        assert!(is_word(&toks[5], "2"));
        assert_eq!(toks[6], Token::GreatAnd);
        assert!(is_word(&toks[7], "1"));
    }

    #[test]
    fn redir_filename() {
        let toks = tokens("cat < input.txt");
        assert_eq!(toks.len(), 3);
        assert!(is_word(&toks[0], "cat"));
        assert_eq!(toks[1], Token::Less);
        assert!(is_word(&toks[2], "input.txt"));
    }

    #[test]
    fn single_quotes() {
        let toks = tokens("echo 'hello world'");
        assert_eq!(toks.len(), 2);
        assert!(is_word(&toks[0], "echo"));
        match &toks[1] {
            Token::Word(parts, _) => {
                assert_eq!(parts.len(), 1);
                assert!(matches!(&parts[0], WordPart::SingleQuoted(s) if s == "hello world"));
            }
            other => panic!("expected Word, got {other:?}"),
        }
    }

    #[test]
    fn double_quotes() {
        let toks = tokens(r#"echo "hello $name""#);
        assert_eq!(toks.len(), 2);
        assert!(is_word(&toks[0], "echo"));
        match &toks[1] {
            Token::Word(parts, _) => {
                assert_eq!(parts.len(), 1);
                match &parts[0] {
                    WordPart::DoubleQuoted(inner) => {
                        assert_eq!(inner.len(), 2);
                        assert!(matches!(&inner[0], WordPart::Literal(s) if s == "hello "));
                        assert!(matches!(&inner[1], WordPart::Param(p) if p.name == "name"));
                    }
                    other => panic!("expected DoubleQuoted, got {other:?}"),
                }
            }
            other => panic!("expected Word, got {other:?}"),
        }
    }

    #[test]
    fn command_substitution() {
        let toks = tokens("echo $(date)");
        assert_eq!(toks.len(), 2);
        assert!(is_word(&toks[0], "echo"));
        match &toks[1] {
            Token::Word(parts, _) => {
                assert_eq!(parts.len(), 1);
                assert!(matches!(&parts[0], WordPart::CmdSubst(_)));
            }
            other => panic!("expected Word, got {other:?}"),
        }
    }

    #[test]
    fn assignment() {
        let toks = tokens("FOO=bar");
        assert_eq!(toks.len(), 1);
        match &toks[0] {
            Token::Assignment { name, value } => {
                assert_eq!(name, "FOO");
                assert_eq!(value.len(), 1);
                assert!(matches!(&value[0], WordPart::Literal(s) if s == "bar"));
            }
            other => panic!("expected Assignment, got {other:?}"),
        }
    }

    #[test]
    fn reserved_words() {
        let toks = tokens("if true; then echo yes; fi");
        assert_eq!(toks.len(), 8);
        assert_eq!(toks[0], Token::If);
        assert!(is_word(&toks[1], "true"));
        assert_eq!(toks[2], Token::Semi);
        assert_eq!(toks[3], Token::Then);
        assert!(is_word(&toks[4], "echo"));
        assert!(is_word(&toks[5], "yes"));
        assert_eq!(toks[6], Token::Semi);
        assert_eq!(toks[7], Token::Fi);
    }

    #[test]
    fn background() {
        let toks = tokens("sleep 10 &");
        assert_eq!(toks.len(), 3);
        assert!(is_word(&toks[0], "sleep"));
        assert!(is_word(&toks[1], "10"));
        assert_eq!(toks[2], Token::Amp);
    }

    #[test]
    fn case_tokens() {
        assert_eq!(tokens(";;"), vec![Token::SemiSemi]);
    }

    #[test]
    fn dollar_brace() {
        let toks = tokens("${var:-default}");
        assert_eq!(toks.len(), 1);
        match &toks[0] {
            Token::Word(parts, _) => {
                assert_eq!(parts.len(), 1);
                match &parts[0] {
                    WordPart::Param(p) => {
                        assert_eq!(p.name, "var");
                        assert!(matches!(p.op, ParamOp::Default { colon: true, .. }));
                    }
                    other => panic!("expected Param, got {other:?}"),
                }
            }
            other => panic!("expected Word, got {other:?}"),
        }
    }

    #[test]
    fn arithmetic() {
        let toks = tokens("$((1+2))");
        assert_eq!(toks.len(), 1);
        match &toks[0] {
            Token::Word(parts, _) => {
                assert_eq!(parts.len(), 1);
                assert!(matches!(&parts[0], WordPart::Arith(_)));
            }
            other => panic!("expected Word, got {other:?}"),
        }
    }

    #[test]
    fn heredoc_operator() {
        let toks = tokens("cat << EOF");
        assert_eq!(toks.len(), 3);
        assert!(is_word(&toks[0], "cat"));
        assert_eq!(toks[1], Token::DLess);
        assert!(is_word(&toks[2], "EOF"));
    }

    #[test]
    fn comments() {
        let toks = tokens("echo hello # comment");
        assert_eq!(toks.len(), 2);
        assert!(is_word(&toks[0], "echo"));
        assert!(is_word(&toks[1], "hello"));
    }

    #[test]
    fn backslash_newline() {
        let toks = tokens("echo hel\\\nlo");
        assert_eq!(toks.len(), 2);
        assert!(is_word(&toks[0], "echo"));
        assert!(is_word(&toks[1], "hello"));
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
