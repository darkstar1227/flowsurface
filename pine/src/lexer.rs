//! Tokens for one logical Pine line.

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// An identifier, possibly dotted (`ta.sma`, `plot.style_line`).
    Ident(String),
    Number(String),
    Str(String),
    /// A `#RRGGBB` or `#RRGGBBAA` color literal, without the `#`.
    Color(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Question,
    Colon,
    /// `=`
    Assign,
    /// `:=`
    Reassign,
    /// `=>`
    Arrow,
    /// `+=`, `-=`, `*=`, `/=`, `%=`: the operator without `=`.
    Compound(&'static str),
    /// Arithmetic and comparison operators.
    Op(&'static str),
}

pub fn tokenize(line: &str) -> Result<Vec<Token>, String> {
    let chars = line.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let character = chars[index];
        match character {
            ' ' | '\t' | '\r' => index += 1,
            '/' if chars.get(index + 1) == Some(&'/') => break,
            '"' | '\'' => {
                let quote = character;
                let mut text = String::new();
                index += 1;
                loop {
                    match chars.get(index) {
                        None => return Err("unterminated string".to_owned()),
                        Some(&c) if c == quote => {
                            index += 1;
                            break;
                        }
                        Some('\\') => {
                            if let Some(&escaped) = chars.get(index + 1) {
                                text.push('\\');
                                text.push(escaped);
                            }
                            index += 2;
                        }
                        Some(&c) => {
                            text.push(c);
                            index += 1;
                        }
                    }
                }
                tokens.push(Token::Str(text));
            }
            '#' => {
                let start = index + 1;
                index = start;
                while chars.get(index).is_some_and(char::is_ascii_hexdigit) {
                    index += 1;
                }
                let hex = chars[start..index].iter().collect::<String>();
                if hex.len() != 6 && hex.len() != 8 {
                    return Err(format!("invalid color literal #{hex}"));
                }
                tokens.push(Token::Color(hex));
            }
            c if c.is_ascii_digit()
                || (c == '.' && chars.get(index + 1).is_some_and(char::is_ascii_digit)) =>
            {
                let start = index;
                while chars
                    .get(index)
                    .is_some_and(|c| c.is_ascii_digit() || *c == '.')
                {
                    index += 1;
                }
                if chars.get(index).is_some_and(|c| *c == 'e' || *c == 'E') {
                    index += 1;
                    if chars.get(index).is_some_and(|c| *c == '-' || *c == '+') {
                        index += 1;
                    }
                    while chars.get(index).is_some_and(char::is_ascii_digit) {
                        index += 1;
                    }
                }
                let mut number = chars[start..index].iter().collect::<String>();
                if number.starts_with('.') {
                    number.insert(0, '0');
                }
                if number.ends_with('.') {
                    number.push('0');
                }
                tokens.push(Token::Number(number));
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = index;
                while chars
                    .get(index)
                    .is_some_and(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
                {
                    index += 1;
                }
                let ident = chars[start..index].iter().collect::<String>();
                tokens.push(Token::Ident(ident.trim_end_matches('.').to_owned()));
            }
            _ => {
                let next = chars.get(index + 1).copied();
                let (token, width) = match (character, next) {
                    (':', Some('=')) => (Token::Reassign, 2),
                    ('=', Some('>')) => (Token::Arrow, 2),
                    ('=', Some('=')) => (Token::Op("=="), 2),
                    ('!', Some('=')) => (Token::Op("!="), 2),
                    ('<', Some('=')) => (Token::Op("<="), 2),
                    ('>', Some('=')) => (Token::Op(">="), 2),
                    ('+', Some('=')) => (Token::Compound("+"), 2),
                    ('-', Some('=')) => (Token::Compound("-"), 2),
                    ('*', Some('=')) => (Token::Compound("*"), 2),
                    ('/', Some('=')) => (Token::Compound("/"), 2),
                    ('%', Some('=')) => (Token::Compound("%"), 2),
                    ('=', _) => (Token::Assign, 1),
                    ('<', _) => (Token::Op("<"), 1),
                    ('>', _) => (Token::Op(">"), 1),
                    ('+', _) => (Token::Op("+"), 1),
                    ('-', _) => (Token::Op("-"), 1),
                    ('*', _) => (Token::Op("*"), 1),
                    ('/', _) => (Token::Op("/"), 1),
                    ('%', _) => (Token::Op("%"), 1),
                    ('(', _) => (Token::LParen, 1),
                    (')', _) => (Token::RParen, 1),
                    ('[', _) => (Token::LBracket, 1),
                    (']', _) => (Token::RBracket, 1),
                    (',', _) => (Token::Comma, 1),
                    ('?', _) => (Token::Question, 1),
                    (':', _) => (Token::Colon, 1),
                    _ => return Err(format!("unexpected character '{character}'")),
                };
                tokens.push(token);
                index += width;
            }
        }
    }
    Ok(tokens)
}

/// Net bracket depth of a line, ignoring strings and comments; used to join continuation lines.
pub fn bracket_depth(line: &str) -> i32 {
    let mut depth = 0;
    let mut quote = None;
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        match quote {
            Some(q) => {
                if character == '\\' {
                    chars.next();
                } else if character == q {
                    quote = None;
                }
            }
            None => match character {
                '"' | '\'' => quote = Some(character),
                '/' if chars.peek() == Some(&'/') => break,
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                _ => {}
            },
        }
    }
    depth
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_operators_strings_and_colors() {
        let tokens = tokenize("x := a >= 1.5 ? \"up\" : #ff0000 // note").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Ident("x".into()),
                Token::Reassign,
                Token::Ident("a".into()),
                Token::Op(">="),
                Token::Number("1.5".into()),
                Token::Question,
                Token::Str("up".into()),
                Token::Colon,
                Token::Color("ff0000".into()),
            ]
        );
    }

    #[test]
    fn depth_ignores_brackets_in_strings() {
        assert_eq!(bracket_depth("plot(x, \"a (b\""), 1);
        assert_eq!(bracket_depth("f(a[1]) // (("), 0);
    }
}
