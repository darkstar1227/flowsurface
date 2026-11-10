//! Logical Pine lines and their indentation tree.

use crate::lexer::bracket_depth;

/// One logical statement line: physical lines joined, comments removed.
#[derive(Debug, Clone)]
pub struct Node {
    /// 1-based number of the first physical line.
    pub line: usize,
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    /// The statement and everything nested under it, one string per logical line.
    pub fn flatten(&self) -> Vec<String> {
        let mut lines = Vec::new();
        self.flatten_into(0, &mut lines);
        lines
    }

    fn flatten_into(&self, depth: usize, lines: &mut Vec<String>) {
        lines.push(format!("{}{}", "    ".repeat(depth), self.text));
        for child in &self.children {
            child.flatten_into(depth + 1, lines);
        }
    }
}

struct Logical {
    line: usize,
    indent: usize,
    text: String,
}

/// Deeper nesting is flattened to keep recursion bounded.
const MAX_NESTING: usize = 64;

/// Remove a trailing `//` comment, leaving string literals alone.
pub fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    let mut escaped = false;
    let mut previous_slash = None;
    for (index, character) in line.char_indices() {
        match quote {
            Some(q) => {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == q {
                    quote = None;
                }
            }
            None => {
                if character == '"' || character == '\'' {
                    quote = Some(character);
                } else if character == '/' {
                    if let Some(start) = previous_slash {
                        return &line[..start];
                    }
                    previous_slash = Some(index);
                    continue;
                }
            }
        }
        previous_slash = None;
    }
    line
}

fn indentation(line: &str) -> usize {
    let mut width = 0;
    for character in line.chars() {
        match character {
            ' ' => width += 1,
            '\t' => width += 4,
            _ => break,
        }
    }
    width
}

fn ends_with_operator(text: &str) -> bool {
    let text = text.trim_end();
    if text.ends_with("=>") {
        return false;
    }
    text.ends_with([',', '?', ':', '+', '-', '*', '/', '%', '=', '<', '>'])
        || text.ends_with(" and")
        || text.ends_with(" or")
}

fn logical_lines(source: &str) -> Vec<Logical> {
    let mut logical: Vec<Logical> = Vec::new();
    let mut depth = 0i32;
    for (index, raw) in source.lines().enumerate() {
        let code = strip_comment(raw);
        let text = code.trim();
        if text.is_empty() {
            continue;
        }
        let indent = indentation(code);
        let continues = depth > 0
            || (!logical.is_empty()
                && (!indent.is_multiple_of(4)
                    || logical.last().is_some_and(|l| ends_with_operator(&l.text))));
        if continues && let Some(last) = logical.last_mut() {
            last.text.push(' ');
            last.text.push_str(text);
        } else {
            logical.push(Logical {
                line: index + 1,
                indent,
                text: text.to_owned(),
            });
        }
        depth = (depth + bracket_depth(text)).max(0);
    }
    logical
}

fn build(lines: &[Logical], position: &mut usize, indent: usize, nesting: usize) -> Vec<Node> {
    let mut nodes = Vec::new();
    while *position < lines.len() {
        let current = &lines[*position];
        if current.indent < indent {
            break;
        }
        *position += 1;
        let mut node = Node {
            line: current.line,
            text: current.text.clone(),
            children: Vec::new(),
        };
        if nesting < MAX_NESTING
            && let Some(next) = lines.get(*position)
            && next.indent > current.indent
        {
            node.children = build(lines, position, next.indent, nesting + 1);
        }
        nodes.push(node);
    }
    nodes
}

/// Split Pine source into an indentation tree of logical lines.
pub fn parse_lines(source: &str) -> Vec<Node> {
    let lines = logical_lines(source);
    let mut position = 0;
    let mut nodes = Vec::new();
    while position < lines.len() {
        let indent = lines[position].indent;
        let batch = build(&lines, &mut position, indent, 0);
        nodes.extend(batch);
    }
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_continuations_and_builds_a_tree() {
        let source = "a = f(1,\n  2)\nif a > 1\n\tb = 2\n    c = 3 +\n    4\nx = y\n  + z\n";
        let nodes = parse_lines(source);
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[0].text, "a = f(1, 2)");
        assert_eq!(nodes[1].children.len(), 2);
        assert_eq!(nodes[1].children[1].text, "c = 3 + 4");
        assert_eq!(nodes[2].text, "x = y + z");
        assert_eq!(nodes[2].line, 7);
    }

    #[test]
    fn comments_are_stripped_outside_strings() {
        assert_eq!(strip_comment("a = \"//x\" // note"), "a = \"//x\" ");
        assert_eq!(strip_comment("// all"), "");
    }
}
