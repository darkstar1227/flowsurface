//! Pine statements: parsing logical lines into a statement tree.

use crate::expr::{Expr, Parser};
use crate::lexer::{Token, tokenize};
use crate::lines::Node;

#[derive(Debug, Clone)]
pub struct Stmt {
    pub line: usize,
    /// Source text, one line per logical line; used for `// UNSUPPORTED:` comments.
    pub text: String,
    pub kind: Kind,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub default: Option<Expr>,
}

#[derive(Debug, Clone)]
pub enum Kind {
    Decl {
        var: bool,
        names: Vec<String>,
        value: Expr,
    },
    Assign {
        name: String,
        value: Expr,
    },
    If {
        cond: Expr,
        then: Vec<Stmt>,
        els: Vec<Stmt>,
    },
    For {
        var: String,
        from: Expr,
        to: Expr,
        body: Vec<Stmt>,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    Break,
    Continue,
    Expr(Expr),
    Fn {
        name: String,
        params: Vec<Param>,
        body: Vec<Stmt>,
    },
    Enum {
        name: String,
        fields: Vec<(String, String)>,
    },
    /// A construct that cannot be translated, with the reason.
    Bad(String),
}

const TYPE_WORDS: &[&str] = &[
    "int", "float", "bool", "string", "color", "series", "simple", "const",
];

fn first_word(text: &str) -> (&str, &str) {
    let end = text
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(text.len());
    (&text[..end], text[end..].trim_start())
}

fn bad(node: &Node, reason: impl Into<String>) -> Stmt {
    Stmt {
        line: node.line,
        text: node.flatten().join("\n"),
        kind: Kind::Bad(reason.into()),
    }
}

/// Parse sibling nodes into statements; an `else` node attaches to the preceding `if`.
pub fn parse_nodes(nodes: &[Node]) -> Vec<Stmt> {
    let mut statements = Vec::new();
    let mut index = 0;
    while index < nodes.len() {
        let node = &nodes[index];
        index += 1;
        let start = index;
        let mut statement = parse_node(node, nodes, &mut index);
        // Consumed `else` siblings belong to this statement when it is reported.
        for sibling in &nodes[start..index] {
            statement.text.push('\n');
            statement.text.push_str(&sibling.flatten().join("\n"));
        }
        statements.push(statement);
    }
    statements
}

/// `next` is the index of the following sibling; it advances past consumed `else` nodes.
fn parse_node(node: &Node, siblings: &[Node], next: &mut usize) -> Stmt {
    let text = node.text.as_str();
    let (word, rest) = first_word(text);
    match word {
        "if" if !rest.is_empty() => return parse_if(node, rest, siblings, next, 0),
        "else" => return bad(node, "`else` without a matching `if`"),
        "for" => return parse_for(node, rest),
        "while" if !rest.is_empty() => {
            return match expression(rest) {
                Ok(cond) => stmt(
                    node,
                    Kind::While {
                        cond,
                        body: parse_nodes(&node.children),
                    },
                ),
                Err(error) => bad(node, error),
            };
        }
        "break" if rest.is_empty() => return stmt(node, Kind::Break),
        "continue" if rest.is_empty() => return stmt(node, Kind::Continue),
        "switch" => {
            // A value-producing `switch`, e.g. the result line of a function body.
            let value =
                tokenize(rest).and_then(|header| block_value(node, true, &header, siblings, next));
            return match value {
                Ok(value) => stmt(node, Kind::Expr(value)),
                Err(error) => bad(node, error),
            };
        }
        "enum" => return parse_enum(node, rest),
        "import" => return bad(node, "`import` is not supported"),
        "export" | "method" => return bad(node, format!("`{word}` is not supported")),
        "type" if !rest.is_empty() && first_word(rest).1.is_empty() => {
            return bad(node, "user-defined types are not supported");
        }
        "varip" => return bad(node, "`varip` is not supported"),
        _ => {}
    }
    let tokens = match tokenize(text) {
        Ok(tokens) => tokens,
        Err(error) => return bad(node, error),
    };
    if let Some(function) = parse_function(node, &tokens) {
        return function;
    }
    match split_assignment(&tokens) {
        Some(position) => parse_assignment(node, &tokens, position, siblings, next),
        None => match Parser::parse_all(&tokens) {
            Ok(expr) => stmt(node, Kind::Expr(expr)),
            Err(error) => bad(node, error),
        },
    }
}

fn stmt(node: &Node, kind: Kind) -> Stmt {
    Stmt {
        line: node.line,
        text: node.flatten().join("\n"),
        kind,
    }
}

fn expression(text: &str) -> Result<Expr, String> {
    Parser::parse_all(&tokenize(text)?)
}

/// Longest `else if` chain translated; longer ones are reported.
const MAX_CHAIN: usize = 64;

fn parse_if(
    node: &Node,
    condition: &str,
    siblings: &[Node],
    next: &mut usize,
    chain: usize,
) -> Stmt {
    if chain > MAX_CHAIN {
        return bad(node, "the `else if` chain is too long");
    }
    let cond = match expression(condition) {
        Ok(cond) => cond,
        Err(error) => return bad(node, error),
    };
    let then = parse_nodes(&node.children);
    let mut els = Vec::new();
    if let Some(following) = siblings.get(*next) {
        let (word, rest) = first_word(&following.text);
        if word == "else" {
            *next += 1;
            let (inner, inner_rest) = first_word(rest);
            if rest.is_empty() {
                els = parse_nodes(&following.children);
            } else if inner == "if" && !inner_rest.is_empty() {
                // `else if` becomes a nested `if` in the else branch.
                let nested = Node {
                    line: following.line,
                    text: rest.to_owned(),
                    children: following.children.clone(),
                };
                els = vec![parse_if(&nested, inner_rest, siblings, next, chain + 1)];
            } else {
                els = vec![bad(following, "unexpected text after `else`")];
            }
        }
    }
    stmt(node, Kind::If { cond, then, els })
}

fn parse_for(node: &Node, rest: &str) -> Stmt {
    let tokens = match tokenize(rest) {
        Ok(tokens) => tokens,
        Err(error) => return bad(node, error),
    };
    let (Some(Token::Ident(var)), Some(Token::Assign)) = (tokens.first(), tokens.get(1)) else {
        return bad(node, "only `for i = a to b` loops are supported");
    };
    let to = tokens
        .iter()
        .position(|t| matches!(t, Token::Ident(word) if word == "to"));
    let Some(to) = to else {
        return bad(node, "`for` loop without `to`");
    };
    let by = tokens
        .iter()
        .position(|t| matches!(t, Token::Ident(word) if word == "by"));
    let end = by.unwrap_or(tokens.len());
    let (Ok(from), Ok(upper)) = (
        Parser::parse_all(&tokens[2..to]),
        Parser::parse_all(&tokens[to + 1..end.max(to + 1)]),
    ) else {
        return bad(node, "could not parse the `for` range");
    };
    if let Some(by) = by {
        match Parser::parse_all(&tokens[by + 1..]) {
            Ok(Expr::Number(step)) if step == "1" => {}
            _ => {
                return bad(
                    node,
                    "`for` loops with a step other than 1 are not supported",
                );
            }
        }
    }
    stmt(
        node,
        Kind::For {
            var: var.clone(),
            from,
            to: upper,
            body: parse_nodes(&node.children),
        },
    )
}

fn parse_enum(node: &Node, name: &str) -> Stmt {
    if name.is_empty() || !first_word(name).1.is_empty() {
        return bad(node, "malformed `enum`");
    }
    let mut fields = Vec::new();
    for child in &node.children {
        let Ok(tokens) = tokenize(&child.text) else {
            return bad(node, "malformed `enum` field");
        };
        match tokens.as_slice() {
            [Token::Ident(field)] => fields.push((field.clone(), field.clone())),
            [Token::Ident(field), Token::Assign, Token::Str(title)] => {
                fields.push((field.clone(), title.clone()));
            }
            _ => return bad(node, "malformed `enum` field"),
        }
    }
    stmt(
        node,
        Kind::Enum {
            name: name.to_owned(),
            fields,
        },
    )
}

fn matching_paren(tokens: &[Token], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        match token {
            Token::LParen | Token::LBracket => depth += 1,
            Token::RParen | Token::RBracket => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split at depth-0 commas.
fn split_commas(tokens: &[Token]) -> Vec<&[Token]> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::LParen | Token::LBracket => depth += 1,
            Token::RParen | Token::RBracket => depth -= 1,
            Token::Comma if depth == 0 => {
                parts.push(&tokens[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < tokens.len() {
        parts.push(&tokens[start..]);
    }
    parts
}

/// `name(params) => body`, one-line or block.
fn parse_function(node: &Node, tokens: &[Token]) -> Option<Stmt> {
    let (Some(Token::Ident(name)), Some(Token::LParen)) = (tokens.first(), tokens.get(1)) else {
        return None;
    };
    let close = matching_paren(tokens, 1)?;
    if tokens.get(close + 1) != Some(&Token::Arrow) {
        return None;
    }
    if name.contains('.') {
        return Some(bad(node, "malformed function name"));
    }
    let mut params = Vec::new();
    for part in split_commas(&tokens[2..close]) {
        let (head, default) = match part.iter().position(|t| *t == Token::Assign) {
            Some(position) => match Parser::parse_all(&part[position + 1..]) {
                Ok(default) => (&part[..position], Some(default)),
                Err(error) => return Some(bad(node, error)),
            },
            None => (part, None),
        };
        let mut words = head.iter();
        let mut name = None;
        for token in words.by_ref() {
            match token {
                Token::Ident(word) if TYPE_WORDS.contains(&word.as_str()) && name.is_none() => {}
                Token::Ident(word) if name.is_none() && !word.contains('.') => {
                    name = Some(word.clone());
                }
                _ => return Some(bad(node, "unsupported function parameter")),
            }
        }
        let Some(name) = name else {
            return Some(bad(node, "unsupported function parameter"));
        };
        params.push(Param { name, default });
    }
    let inline = &tokens[close + 2..];
    let body = if inline.is_empty() {
        parse_nodes(&node.children)
    } else {
        match Parser::parse_all(inline) {
            Ok(expr) => vec![stmt(node, Kind::Expr(expr))],
            Err(error) => return Some(bad(node, error)),
        }
    };
    if body.is_empty() {
        return Some(bad(node, "function without a body"));
    }
    Some(stmt(
        node,
        Kind::Fn {
            name: name.clone(),
            params,
            body,
        },
    ))
}

/// Position of the depth-0 assignment operator.
fn split_assignment(tokens: &[Token]) -> Option<usize> {
    let mut depth = 0i32;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::LParen | Token::LBracket => depth += 1,
            Token::RParen | Token::RBracket => depth -= 1,
            Token::Assign | Token::Reassign | Token::Compound(_) if depth == 0 => {
                return Some(index);
            }
            _ => {}
        }
    }
    None
}

fn parse_assignment(
    node: &Node,
    tokens: &[Token],
    position: usize,
    siblings: &[Node],
    next: &mut usize,
) -> Stmt {
    let left = &tokens[..position];
    let right = &tokens[position + 1..];
    match &tokens[position] {
        Token::Reassign | Token::Compound(_) => {
            let [Token::Ident(name)] = left else {
                return bad(node, "only a plain variable can be reassigned");
            };
            if name.contains('.') {
                return bad(node, "assigning to a field is not supported");
            }
            let value = match Parser::parse_all(right) {
                Ok(value) => value,
                Err(error) => return bad(node, error),
            };
            let value = match &tokens[position] {
                Token::Compound(op) => {
                    Expr::Binary(op, Box::new(Expr::Ident(name.clone())), Box::new(value))
                }
                _ => value,
            };
            stmt(
                node,
                Kind::Assign {
                    name: name.clone(),
                    value,
                },
            )
        }
        _ => parse_declaration(node, left, right, siblings, next),
    }
}

fn parse_declaration(
    node: &Node,
    left: &[Token],
    right: &[Token],
    siblings: &[Node],
    next: &mut usize,
) -> Stmt {
    let mut left = left;
    let mut var = false;
    if let Some(Token::Ident(word)) = left.first()
        && (word == "var" || word == "varip")
    {
        if word == "varip" {
            return bad(node, "`varip` is not supported");
        }
        var = true;
        left = &left[1..];
    }
    let mut names = Vec::new();
    if left.first() == Some(&Token::LBracket) {
        if var || left.last() != Some(&Token::RBracket) {
            return bad(node, "unsupported tuple declaration");
        }
        for part in split_commas(&left[1..left.len() - 1]) {
            let [Token::Ident(name)] = part else {
                return bad(node, "unsupported tuple declaration");
            };
            names.push(name.clone());
        }
        if names.len() < 2 {
            return bad(node, "unsupported tuple declaration");
        }
    } else {
        let mut words = Vec::new();
        for token in left {
            let Token::Ident(word) = token else {
                return bad(node, "unsupported assignment target");
            };
            words.push(word.as_str());
        }
        let Some((name, types)) = words.split_last() else {
            return bad(node, "assignment without a target");
        };
        if types.iter().any(|word| !TYPE_WORDS.contains(word)) {
            return bad(node, "unsupported declaration type");
        }
        if name.contains('.') {
            return bad(node, "assigning to a field is not supported");
        }
        names.push((*name).to_owned());
    }
    let value = match right.first() {
        Some(Token::Ident(word)) if word == "if" || word == "switch" => {
            match block_value(node, word == "switch", &right[1..], siblings, next) {
                Ok(value) => value,
                Err(error) => return bad(node, error),
            }
        }
        _ => match Parser::parse_all(right) {
            Ok(value) => value,
            Err(error) => return bad(node, error),
        },
    };
    stmt(node, Kind::Decl { var, names, value })
}

/// `x = switch ...` and `x = if ...` with one expression per arm.
fn block_value(
    node: &Node,
    is_switch: bool,
    header: &[Token],
    siblings: &[Node],
    next: &mut usize,
) -> Result<Expr, String> {
    if is_switch {
        let subject = if header.is_empty() {
            None
        } else {
            Some(Box::new(Parser::parse_all(header)?))
        };
        let mut arms = Vec::new();
        for child in &node.children {
            if !child.children.is_empty() {
                return Err("multi-line `switch` arms are not supported".to_owned());
            }
            let tokens = tokenize(&child.text)?;
            let mut depth = 0i32;
            let arrow = tokens.iter().position(|token| match token {
                Token::LParen | Token::LBracket => {
                    depth += 1;
                    false
                }
                Token::RParen | Token::RBracket => {
                    depth -= 1;
                    false
                }
                Token::Arrow => depth == 0,
                _ => false,
            });
            let Some(arrow) = arrow else {
                return Err("`switch` arm without `=>`".to_owned());
            };
            let case = if arrow == 0 {
                None
            } else {
                Some(Parser::parse_all(&tokens[..arrow])?)
            };
            if tokens.len() == arrow + 1 {
                return Err("multi-line `switch` arms are not supported".to_owned());
            }
            arms.push((case, Parser::parse_all(&tokens[arrow + 1..])?));
        }
        return Ok(Expr::Switch { subject, arms });
    }
    if_value(header, &node.children, siblings, next, 0)
}

/// `x = if c` with single-expression bodies; `else` siblings are consumed.
fn if_value(
    header: &[Token],
    children: &[Node],
    siblings: &[Node],
    next: &mut usize,
    chain: usize,
) -> Result<Expr, String> {
    if chain > MAX_CHAIN {
        return Err("the `else if` chain is too long".to_owned());
    }
    let cond = Parser::parse_all(header)?;
    let then = single_expression(children)?;
    let mut otherwise = Expr::Ident("na".to_owned());
    if let Some(following) = siblings.get(*next) {
        let (word, rest) = first_word(&following.text);
        if word == "else" {
            *next += 1;
            let (inner, inner_rest) = first_word(rest);
            if rest.is_empty() {
                otherwise = single_expression(&following.children)?;
            } else if inner == "if" && !inner_rest.is_empty() {
                otherwise = if_value(
                    &tokenize(inner_rest)?,
                    &following.children,
                    siblings,
                    next,
                    chain + 1,
                )?;
            } else {
                return Err("unexpected text after `else`".to_owned());
            }
        }
    }
    Ok(Expr::Ternary(
        Box::new(cond),
        Box::new(then),
        Box::new(otherwise),
    ))
}

fn single_expression(nodes: &[Node]) -> Result<Expr, String> {
    match parse_nodes(nodes).as_slice() {
        [
            Stmt {
                kind: Kind::Expr(expr),
                ..
            },
        ] => Ok(expr.clone()),
        _ => Err("only single-expression `if` bodies are supported here".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lines::parse_lines;

    fn parse(source: &str) -> Vec<Stmt> {
        parse_nodes(&parse_lines(source))
    }

    #[test]
    fn declarations_and_compound_assignment() {
        let statements = parse("var float x = 1.0\ny += 2\n[a, b] = f()\n");
        assert!(
            matches!(&statements[0].kind, Kind::Decl { var: true, names, .. } if names == &["x"])
        );
        assert!(
            matches!(&statements[1].kind, Kind::Assign { name, value: Expr::Binary("+", ..) } if name == "y")
        );
        assert!(matches!(&statements[2].kind, Kind::Decl { names, .. } if names.len() == 2));
    }

    #[test]
    fn else_if_chain_nests() {
        let statements = parse("if a\n    x = 1\nelse if b\n    x = 2\nelse\n    x = 3\n");
        let Kind::If { els, .. } = &statements[0].kind else {
            panic!("expected if");
        };
        assert!(matches!(&els[0].kind, Kind::If { els, .. } if els.len() == 1));
    }

    #[test]
    fn functions_in_both_forms() {
        let statements = parse("f(a, b = 2) => a + b\ng(x) =>\n    y = x * 2\n    [y, x]\n");
        assert!(
            matches!(&statements[0].kind, Kind::Fn { params, body, .. } if params.len() == 2 && body.len() == 1)
        );
        assert!(matches!(&statements[1].kind, Kind::Fn { body, .. } if body.len() == 2));
    }
}
