//! Pine expressions: parsing from tokens.

use crate::lexer::Token;

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Number(String),
    Str(String),
    Color(String),
    Ident(String),
    Call {
        name: String,
        args: Vec<Arg>,
    },
    Index(Box<Expr>, Box<Expr>),
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `[a, b]`: a tuple in Pine (array literals use `array.from`).
    Tuple(Vec<Expr>),
    /// A `switch` used as a value; an arm without a case is the default.
    Switch {
        subject: Option<Box<Expr>>,
        arms: Vec<(Option<Expr>, Expr)>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Arg {
    pub name: Option<String>,
    pub value: Expr,
}

/// Nesting and size bounds keep hostile input from overflowing the stack.
const MAX_DEPTH: usize = 100;
const MAX_NODES: usize = 600;

pub struct Parser<'a> {
    tokens: &'a [Token],
    position: usize,
    depth: usize,
    nodes: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self {
            tokens,
            position: 0,
            depth: 0,
            nodes: 0,
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn at_end(&self) -> bool {
        self.position >= self.tokens.len()
    }

    fn count_node(&mut self) -> Result<(), String> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err("expression is too large".to_owned());
        }
        Ok(())
    }

    fn next(&mut self) -> Option<&Token> {
        let token = self.tokens.get(self.position);
        self.position += 1;
        token
    }

    fn eat(&mut self, token: &Token) -> bool {
        if self.peek() == Some(token) {
            self.position += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: &Token) -> Result<(), String> {
        if self.eat(token) {
            Ok(())
        } else {
            Err(format!("expected {token:?}, found {:?}", self.peek()))
        }
    }

    /// Parse a complete expression and require that nothing follows it.
    pub fn parse_all(tokens: &[Token]) -> Result<Expr, String> {
        let mut parser = Parser::new(tokens);
        let expr = parser.expression()?;
        if !parser.at_end() {
            return Err(format!("unexpected {:?}", parser.peek()));
        }
        Ok(expr)
    }

    fn expression(&mut self) -> Result<Expr, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("expression is nested too deeply".to_owned());
        }
        let result = self.expression_inner();
        self.depth -= 1;
        result
    }

    fn expression_inner(&mut self) -> Result<Expr, String> {
        let condition = self.binary(0)?;
        if self.eat(&Token::Question) {
            let when_true = self.expression()?;
            self.expect(&Token::Colon)?;
            let when_false = self.expression()?;
            return Ok(Expr::Ternary(
                Box::new(condition),
                Box::new(when_true),
                Box::new(when_false),
            ));
        }
        Ok(condition)
    }

    fn binary_operator(&self) -> Option<(&'static str, u8)> {
        match self.peek()? {
            Token::Ident(word) if word == "or" => Some(("||", 1)),
            Token::Ident(word) if word == "and" => Some(("&&", 2)),
            Token::Op(op @ ("==" | "!=")) => Some((op, 3)),
            Token::Op(op @ ("<" | "<=" | ">" | ">=")) => Some((op, 4)),
            Token::Op(op @ ("+" | "-")) => Some((op, 5)),
            Token::Op(op @ ("*" | "/" | "%")) => Some((op, 6)),
            _ => None,
        }
    }

    fn binary(&mut self, min_precedence: u8) -> Result<Expr, String> {
        let mut left = self.unary()?;
        while let Some((op, precedence)) = self.binary_operator() {
            if precedence <= min_precedence {
                break;
            }
            self.position += 1;
            self.count_node()?;
            let right = self.binary(precedence)?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("expression is nested too deeply".to_owned());
        }
        let result = self.unary_inner();
        self.depth -= 1;
        result
    }

    fn unary_inner(&mut self) -> Result<Expr, String> {
        self.count_node()?;
        match self.peek() {
            Some(Token::Op("-")) => {
                self.position += 1;
                Ok(Expr::Unary("-", Box::new(self.unary()?)))
            }
            Some(Token::Op("+")) => {
                self.position += 1;
                self.unary()
            }
            Some(Token::Ident(word)) if word == "not" => {
                self.position += 1;
                Ok(Expr::Unary("!", Box::new(self.unary()?)))
            }
            _ => self.postfix(),
        }
    }

    fn postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.primary()?;
        while self.eat(&Token::LBracket) {
            let offset = self.expression()?;
            self.expect(&Token::RBracket)?;
            expr = Expr::Index(Box::new(expr), Box::new(offset));
        }
        Ok(expr)
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.next().cloned() {
            Some(Token::Number(number)) => Ok(Expr::Number(number)),
            Some(Token::Str(text)) => Ok(Expr::Str(text)),
            Some(Token::Color(hex)) => Ok(Expr::Color(hex)),
            Some(Token::LParen) => {
                let expr = self.expression()?;
                self.expect(&Token::RParen)?;
                Ok(expr)
            }
            Some(Token::LBracket) => {
                let items = self.list(&Token::RBracket)?;
                Ok(Expr::Tuple(items))
            }
            Some(Token::Ident(name)) => {
                // A type-qualified generic call such as `array.new<float>(...)` is not supported.
                if self.eat(&Token::LParen) {
                    let args = self.arguments()?;
                    Ok(Expr::Call { name, args })
                } else {
                    Ok(Expr::Ident(name))
                }
            }
            other => Err(format!("unexpected {other:?}")),
        }
    }

    fn list(&mut self, close: &Token) -> Result<Vec<Expr>, String> {
        let mut items = Vec::new();
        if self.eat(close) {
            return Ok(items);
        }
        loop {
            items.push(self.expression()?);
            if self.eat(close) {
                return Ok(items);
            }
            self.expect(&Token::Comma)?;
        }
    }

    fn arguments(&mut self) -> Result<Vec<Arg>, String> {
        let mut args = Vec::new();
        if self.eat(&Token::RParen) {
            return Ok(args);
        }
        loop {
            let name = match (self.peek(), self.tokens.get(self.position + 1)) {
                (Some(Token::Ident(name)), Some(Token::Assign)) => {
                    let name = name.clone();
                    self.position += 2;
                    Some(name)
                }
                _ => None,
            };
            let value = self.expression()?;
            args.push(Arg { name, value });
            if self.eat(&Token::RParen) {
                return Ok(args);
            }
            self.expect(&Token::Comma)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;

    fn parse(text: &str) -> Expr {
        Parser::parse_all(&tokenize(text).unwrap()).unwrap()
    }

    #[test]
    fn precedence_and_ternary() {
        let expr = parse("a + b * c > 1 and not d ? x : y");
        let Expr::Ternary(condition, _, _) = expr else {
            panic!("expected a ternary");
        };
        let Expr::Binary("&&", left, right) = *condition else {
            panic!("expected &&");
        };
        assert!(matches!(*left, Expr::Binary(">", _, _)));
        assert!(matches!(*right, Expr::Unary("!", _)));
    }

    #[test]
    fn calls_with_named_arguments_and_history() {
        let expr = parse("ta.sma(close[1], length = 14)");
        let Expr::Call { name, args } = expr else {
            panic!("expected a call");
        };
        assert_eq!(name, "ta.sma");
        assert!(matches!(args[0].value, Expr::Index(_, _)));
        assert_eq!(args[1].name.as_deref(), Some("length"));
    }
}
