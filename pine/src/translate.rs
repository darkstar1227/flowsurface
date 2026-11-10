//! Translation of Pine statements and expressions into `.ind` source.

use std::collections::{HashMap, HashSet};

use crate::Unsupported;
use crate::expr::{Arg, Expr};
use crate::stmt::{Kind, Param, Stmt};

/// Operator precedence of a translated expression; atoms never need parentheses.
pub(crate) const ATOM: u8 = 9;
pub(crate) const UNARY: u8 = 7;
const MUL: u8 = 6;
const ADD: u8 = 5;
const REL: u8 = 4;
const EQ: u8 = 3;
const AND: u8 = 2;
const OR: u8 = 1;

const MAX_INLINE_DEPTH: usize = 16;

/// Names that cannot be used for user bindings in `.ind` sources.
const RESERVED: &[&str] = &[
    "break",
    "continue",
    "else",
    "enum",
    "fn",
    "for",
    "if",
    "in",
    "return",
    "state",
    "switch",
    "default",
    "to",
    "var",
    "varip",
    "while",
    "true",
    "false",
    "na",
    "nz",
    "coalesce",
    "select",
    "chart",
    "instrument",
    "open_interest",
    "bar_index",
    "barstate",
    "ta",
    "math",
    "str",
    "array",
    "record",
    "color",
    "input",
    "plot",
    "plotcandle",
    "plotohlc",
    "plotrange",
    "fill",
    "style",
    "mark",
    "display",
    "format",
    "indicator",
    "language",
    "segment",
    "region",
    "region_remove",
    "column",
    "readout",
    "broken_cloud",
];

/// A translated value: a single expression with its precedence, or a tuple.
#[derive(Debug, Clone)]
pub(crate) enum Val {
    One(String, u8),
    Many(Vec<String>),
}

#[derive(Default, Clone)]
pub(crate) struct Frame {
    /// Function parameters bound to already translated expressions.
    pub subst: HashMap<String, (String, u8)>,
    /// Function-local names mapped to their unique output names.
    pub rename: HashMap<String, String>,
}

#[derive(Default)]
pub(crate) struct Header {
    pub title: Option<String>,
    pub overlay: bool,
    pub precision: Option<u32>,
    pub max_bars_back: Option<u32>,
    pub seen: bool,
    pub dynamic_history: bool,
}

pub(crate) struct PlotRef {
    pub id: String,
    pub constant: bool,
}

pub(crate) struct Tr {
    pub unsupported: Vec<Unsupported>,
    /// Lossy-but-intentional conversions worth telling the user about.
    pub notes: Vec<Unsupported>,
    /// Source line of the top-level statement being translated.
    pub(crate) line: usize,
    /// Top-level `name = cond ? a : b` declarations, to resolve `color.new(name, t)`.
    pub(crate) color_defs: HashMap<String, Expr>,
    pub out: Vec<String>,
    pub tail: Vec<String>,
    pub header: Header,
    pub(crate) pre: Vec<String>,
    pub(crate) top_pre: Vec<String>,
    top_insert: usize,
    pub(crate) depth: usize,
    pub(crate) cond: u32,
    loops: usize,
    pub(crate) local_names: HashSet<String>,
    pub(crate) top_immut: HashSet<String>,
    pub(crate) mutable: HashSet<String>,
    mutable_out: HashSet<String>,
    pub(crate) simple: HashSet<String>,
    pub(crate) used_names: HashSet<String>,
    declared: HashSet<String>,
    renamed: HashMap<String, String>,
    pub(crate) functions: HashMap<String, (Vec<Param>, Vec<Stmt>)>,
    pub(crate) enums: HashMap<String, Vec<(String, String)>>,
    pub(crate) frames: Vec<Frame>,
    pub(crate) inline_depth: usize,
    pub(crate) plots: HashMap<String, PlotRef>,
    pub(crate) plot_ids: HashSet<String>,
    pub(crate) fill_count: usize,
    /// `var name_prev = na` declarations for history of reassigned variables.
    pub prelude: Vec<String>,
    /// `name_prev := name` updates appended after the last statement.
    pub prev_updates: Vec<String>,
    prev_vars: HashMap<String, String>,
}

pub(crate) fn paren(text: &str, precedence: u8, needed: u8) -> String {
    if precedence < needed {
        format!("({text})")
    } else {
        text.to_owned()
    }
}

/// True for a bare identifier, field path, or number: safe to repeat or substitute.
pub(crate) fn is_atom_text(text: &str) -> bool {
    !text.is_empty()
        && (text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
            || (text.starts_with('"')
                && text.ends_with('"')
                && text.len() >= 2
                && !text[1..text.len() - 1].contains('"')))
}

/// Whether `text` mentions any of `names` as a bare identifier.
pub(crate) fn mentions_any(text: &str, names: &HashSet<String>) -> bool {
    if names.is_empty() {
        return false;
    }
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut in_string = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'"' {
            in_string = !in_string;
            index += 1;
        } else if in_string {
            if byte == b'\\' {
                index += 1;
            }
            index += 1;
        } else if byte.is_ascii_alphabetic() || byte == b'_' {
            let start = index;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            let field = start > 0 && bytes[start - 1] == b'.';
            if !field && names.contains(&text[start..index]) {
                return true;
            }
        } else if byte.is_ascii_digit() {
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'.')
            {
                index += 1;
            }
        } else {
            index += 1;
        }
    }
    false
}

/// Quote a Pine string literal body as a `.ind` string.
pub(crate) fn quote(body: &str) -> String {
    let mut out = String::from("\"");
    let mut chars = body.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\\' => match chars.next() {
                Some('\'') => out.push('\''),
                Some(next) => {
                    out.push('\\');
                    out.push(next);
                }
                None => out.push_str("\\\\"),
            },
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// The unquoted text of a Pine string literal body.
pub(crate) fn unquote(body: &str) -> String {
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(character) = chars.next() {
        if character == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(next) => out.push(next),
                None => {}
            }
        } else {
            out.push(character);
        }
    }
    out
}

/// A numeric literal, including a negated one.
pub(crate) fn literal_number(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Number(number) => Some(number.clone()),
        Expr::Unary("-", inner) => match inner.as_ref() {
            Expr::Number(number) => Some(format!("-{number}")),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn contains_division(expr: &Expr) -> bool {
    match expr {
        Expr::Binary("/", ..) => true,
        Expr::Binary(_, left, right) => contains_division(left) || contains_division(right),
        Expr::Unary(_, inner) => contains_division(inner),
        Expr::Ternary(a, b, c) => {
            contains_division(a) || contains_division(b) || contains_division(c)
        }
        Expr::Call { args, .. } => args.iter().any(|arg| contains_division(&arg.value)),
        Expr::Index(a, b) => contains_division(a) || contains_division(b),
        Expr::Tuple(items) => items.iter().any(contains_division),
        _ => false,
    }
}

/// Collect names that are reassigned or declared with `var`.
fn collect_mutable(statements: &[Stmt], names: &mut HashSet<String>) {
    for statement in statements {
        match &statement.kind {
            Kind::Decl {
                var: true,
                names: declared,
                ..
            } => names.extend(declared.iter().cloned()),
            Kind::Assign { name, .. } => {
                names.insert(name.clone());
            }
            Kind::If { then, els, .. } => {
                collect_mutable(then, names);
                collect_mutable(els, names);
            }
            Kind::For { body, .. } | Kind::While { body, .. } | Kind::Fn { body, .. } => {
                collect_mutable(body, names);
            }
            _ => {}
        }
    }
}

impl Tr {
    pub(crate) fn new(statements: &[Stmt], used_names: HashSet<String>) -> Self {
        let mut mutable = HashSet::new();
        collect_mutable(statements, &mut mutable);
        let mut tr = Self {
            unsupported: Vec::new(),
            notes: Vec::new(),
            line: 0,
            color_defs: HashMap::new(),
            out: Vec::new(),
            tail: Vec::new(),
            header: Header::default(),
            pre: Vec::new(),
            top_pre: Vec::new(),
            top_insert: 0,
            depth: 0,
            cond: 0,
            loops: 0,
            local_names: HashSet::new(),
            top_immut: HashSet::new(),
            mutable,
            mutable_out: HashSet::new(),
            simple: HashSet::new(),
            used_names,
            declared: HashSet::new(),
            renamed: HashMap::new(),
            functions: HashMap::new(),
            enums: HashMap::new(),
            frames: Vec::new(),
            inline_depth: 0,
            plots: HashMap::new(),
            plot_ids: HashSet::new(),
            fill_count: 0,
            prelude: Vec::new(),
            prev_updates: Vec::new(),
            prev_vars: HashMap::new(),
        };
        for statement in statements {
            if let Kind::Decl {
                var: false,
                names,
                value: value @ Expr::Ternary(..),
            } = &statement.kind
                && let [name] = names.as_slice()
                && !tr.mutable.contains(name)
            {
                tr.color_defs.insert(name.clone(), value.clone());
            }
        }
        let names = tr.mutable.iter().cloned().collect::<Vec<_>>();
        for name in names {
            let out = tr.out_name(&name);
            tr.mutable_out.insert(out);
        }
        tr
    }

    pub(crate) fn run(&mut self, statements: &[Stmt]) {
        for statement in statements {
            self.top_insert = self.out.len();
            self.top_pre.clear();
            self.stmt(statement);
            if !self.top_pre.is_empty() {
                let at = self.top_insert.min(self.out.len());
                let lines = std::mem::take(&mut self.top_pre);
                self.out.splice(at..at, lines);
            }
        }
    }

    // ---- naming -------------------------------------------------------------------------

    /// The `.ind` name for a Pine name: sanitized, with keyword clashes suffixed by `_`.
    pub(crate) fn out_name(&mut self, name: &str) -> String {
        if let Some(existing) = self.renamed.get(name) {
            return existing.clone();
        }
        let mut out = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>();
        if out.is_empty() || out.starts_with(|c: char| c.is_ascii_digit()) {
            out.insert(0, '_');
        }
        while RESERVED.contains(&out.as_str()) {
            out.push('_');
        }
        if out != name {
            while self.used_names.contains(&out) && out != name {
                out.push('_');
            }
            self.used_names.insert(out.clone());
        }
        self.renamed.insert(name.to_owned(), out.clone());
        out
    }

    /// A fresh identifier that appears nowhere in the Pine source.
    pub(crate) fn fresh(&mut self, base: &str) -> String {
        let base = base
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>();
        let mut number = 1;
        loop {
            let name = format!("{base}_{number}");
            if self.used_names.insert(name.clone()) {
                return name;
            }
            number += 1;
        }
    }

    // ---- emission helpers ---------------------------------------------------------------

    fn indent(&self) -> String {
        "    ".repeat(self.depth)
    }

    pub(crate) fn push_line(&mut self, line: &str) {
        let indent = self.indent();
        self.out.push(format!("{indent}{line}"));
    }

    pub(crate) fn flush_pre(&mut self) {
        let lines = std::mem::take(&mut self.pre);
        for line in lines {
            self.push_line(&line);
        }
        if self.depth == 0 {
            self.top_insert = self.out.len();
        }
    }

    pub(crate) fn register_binding(&mut self, name: &str, immutable: bool, top: bool) {
        if top {
            if immutable {
                self.top_immut.insert(name.to_owned());
            }
            self.local_names.remove(name);
        } else {
            self.local_names.insert(name.to_owned());
        }
    }

    /// Bind `text` to a fresh temporary right before the current statement.
    pub(crate) fn hoist_local(&mut self, text: &str) -> String {
        let name = self.fresh("tmp");
        self.bind_pre(&name, text, true);
        name
    }

    /// Emit `name = text` before the current statement. Pure values go to the top level
    /// when nothing block-local or mutable is involved.
    pub(crate) fn bind_pre(&mut self, name: &str, text: &str, immutable: bool) {
        let line = format!("{name} = {text}");
        if self.depth > 0
            && !mentions_any(text, &self.local_names)
            && !mentions_any(text, &self.mutable_out)
        {
            self.top_pre.push(line);
            self.register_binding(name, immutable, true);
        } else {
            self.pre.push(line);
            let top = self.depth == 0;
            self.register_binding(name, immutable, top);
        }
    }

    /// Bind `text` to a temporary at the top level (required for stateful calculations and
    /// for values that need retained history).
    pub(crate) fn hoist_top(&mut self, text: &str) -> Result<String, String> {
        let name = self.fresh("tmp");
        self.push_top_lines(vec![format!("{name} = {text}")], text)?;
        self.top_immut.insert(name.clone());
        Ok(name)
    }

    /// Push lines to the top level; `deps` is the text they depend on.
    pub(crate) fn push_top_lines(&mut self, lines: Vec<String>, deps: &str) -> Result<(), String> {
        if self.depth == 0 {
            self.pre.extend(lines);
            return Ok(());
        }
        if mentions_any(deps, &self.local_names) {
            return Err(
                "a calculation that needs the top level depends on a value defined inside a block"
                    .to_owned(),
            );
        }
        self.top_pre.extend(lines);
        Ok(())
    }

    /// A stateful `.ind` call: hoisted out of conditional positions.
    pub(crate) fn stateful(&mut self, text: String) -> Result<(String, u8), String> {
        if self.cond == 0 {
            return Ok((text, ATOM));
        }
        Ok((self.hoist_top(&text)?, ATOM))
    }

    /// `text` as something safe to repeat.
    pub(crate) fn atom(&mut self, text: String) -> String {
        if is_atom_text(&text) {
            text
        } else {
            self.hoist_local(&text)
        }
    }

    pub(crate) fn is_hist_safe(&self, text: &str) -> bool {
        (text.starts_with("chart.")
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.'))
            || text == "bar_index"
            || text == "open_interest"
            || self.top_immut.contains(text)
    }

    /// An expression whose history can be read.
    pub(crate) fn history_base(&mut self, text: String) -> Result<String, String> {
        if self.is_hist_safe(&text) {
            Ok(text)
        } else {
            self.hoist_top(&text)
        }
    }

    pub(crate) fn mark_mutable(&mut self, name: &str) {
        self.mutable_out.insert(name.to_owned());
    }

    /// The current value of `text`, ready for history reads.
    pub(crate) fn history_cur(&mut self, text: String) -> Result<String, String> {
        if self.mutable_out.contains(&text) {
            Ok(text)
        } else {
            self.history_base(text)
        }
    }

    /// `cur[offset]` for a `cur` from [`Self::history_cur`]. A reassigned variable cannot
    /// retain history, so its end-of-bar value is carried in a `var` updated after the
    /// last statement.
    pub(crate) fn history_at(&mut self, cur: &str, offset: &str) -> Result<String, String> {
        if !self.mutable_out.contains(cur) {
            return Ok(format!("{cur}[{offset}]"));
        }
        if self.local_names.contains(cur) {
            return Err("history of a variable defined inside a block is not supported".to_owned());
        }
        let previous = match self.prev_vars.get(cur) {
            Some(name) => name.clone(),
            None => {
                let name = self.fresh(&format!("{cur}_prev"));
                self.prelude.push(format!("var {name} = na"));
                self.prev_updates.push(format!("{name} := {cur}"));
                self.prev_vars.insert(cur.to_owned(), name.clone());
                name
            }
        };
        if offset == "1" {
            return Ok(previous);
        }
        match offset.parse::<u32>() {
            Ok(bars) if bars > 1 => {
                let base = self.hoist_top(&previous)?;
                Ok(format!("{base}[{}]", bars - 1))
            }
            _ => {
                Err("a dynamic history offset on a reassigned variable is not supported".to_owned())
            }
        }
    }

    // ---- statements ---------------------------------------------------------------------

    fn mark_unsupported(&mut self, statement: &Stmt, reason: String) {
        for line in statement.text.lines() {
            self.push_line(&format!("// UNSUPPORTED: {line}"));
        }
        self.unsupported.push(Unsupported {
            line: statement.line,
            reason,
        });
    }

    pub(crate) fn stmts(&mut self, statements: &[Stmt]) {
        for statement in statements {
            self.stmt(statement);
        }
    }

    /// Run `f` as a conditionally evaluated position.
    pub(crate) fn lazily<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.cond += 1;
        let result = f(self);
        self.cond -= 1;
        result
    }

    fn stmt(&mut self, statement: &Stmt) {
        if self.depth == 0 {
            self.line = statement.line;
        }
        let snapshot = (self.out.len(), self.pre.len(), self.top_pre.len());
        let scope = (
            self.depth,
            self.cond,
            self.loops,
            self.frames.len(),
            self.inline_depth,
            self.local_names.clone(),
        );
        if let Err(reason) = self.stmt_inner(statement) {
            self.out.truncate(snapshot.0);
            self.pre.truncate(snapshot.1);
            self.top_pre.truncate(snapshot.2);
            self.depth = scope.0;
            self.cond = scope.1;
            self.loops = scope.2;
            self.frames.truncate(scope.3);
            self.inline_depth = scope.4;
            self.local_names = scope.5;
            self.mark_unsupported(statement, reason);
        }
    }

    fn block(&mut self, statements: &[Stmt], loops: bool) {
        let saved = self.local_names.clone();
        self.depth += 1;
        self.cond += 1;
        self.loops += usize::from(loops);
        self.stmts(statements);
        self.loops -= usize::from(loops);
        self.cond -= 1;
        self.depth -= 1;
        self.local_names = saved;
    }

    /// Allocate the output name for a declaration and register it.
    fn declare(&mut self, name: &str, immutable: bool) -> String {
        let out = self.out_name(name);
        self.declared.insert(name.to_owned());
        let top = self.depth == 0;
        self.register_binding(&out, immutable && !self.mutable.contains(name), top);
        out
    }

    fn stmt_inner(&mut self, statement: &Stmt) -> Result<(), String> {
        match &statement.kind {
            Kind::Bad(reason) => Err(reason.clone()),
            Kind::Break | Kind::Continue => {
                if self.loops == 0 {
                    return Err("`break`/`continue` outside a loop".to_owned());
                }
                let word = if matches!(statement.kind, Kind::Break) {
                    "break"
                } else {
                    "continue"
                };
                self.push_line(word);
                Ok(())
            }
            Kind::Enum { name, fields } => {
                self.enums.insert(name.clone(), fields.clone());
                Ok(())
            }
            Kind::Fn { name, params, body } => {
                if self.depth > 0 {
                    return Err("functions must be declared at the top level".to_owned());
                }
                self.functions
                    .insert(name.clone(), (params.clone(), body.clone()));
                Ok(())
            }
            Kind::Expr(expr) => self.expression_statement(expr),
            Kind::Decl { var, names, value } => self.declaration(*var, names, value),
            Kind::Assign { name, value } => {
                let target = match self.frames.last().map(|f| f.rename.get(name).cloned()) {
                    Some(Some(renamed)) => renamed,
                    Some(None) => return Err(format!("cannot assign to `{name}` here")),
                    None => {
                        if !self.declared.contains(name) {
                            return Err(format!("`{name}` is not defined"));
                        }
                        self.out_name(name)
                    }
                };
                let (text, _) = self.one(value)?;
                self.flush_pre();
                self.push_line(&format!("{target} := {text}"));
                Ok(())
            }
            Kind::If { cond, then, els } => {
                let (text, _) = self.one(cond)?;
                self.flush_pre();
                self.push_line(&format!("if {text} {{"));
                self.block(then, false);
                if els.is_empty() {
                    self.push_line("}");
                } else {
                    self.push_line("} else {");
                    self.block(els, false);
                    self.push_line("}");
                }
                Ok(())
            }
            Kind::For {
                var,
                from,
                to,
                body,
            } => {
                if let (Some(a), Some(b)) = (literal_number(from), literal_number(to))
                    && let (Ok(a), Ok(b)) = (a.parse::<f64>(), b.parse::<f64>())
                    && a > b
                {
                    return Err("descending `for` loops are not supported".to_owned());
                }
                let (start, _) = self.one(from)?;
                let (end, _) = self.one(to)?;
                self.flush_pre();
                let saved = self.local_names.clone();
                let was_declared = self.declared.contains(var);
                let name = self.out_name(var);
                self.push_line(&format!("for {name} = {start} to {end} {{"));
                self.declared.insert(var.clone());
                self.local_names.insert(name);
                self.block(body, true);
                self.push_line("}");
                self.local_names = saved;
                if !was_declared {
                    self.declared.remove(var);
                }
                Ok(())
            }
            Kind::While { cond, body } => {
                let mark = self.pre.len();
                let top_mark = self.top_pre.len();
                let (text, _) = self.lazily(|s| s.one(cond))?;
                if self.pre.len() != mark || self.top_pre.len() != top_mark {
                    return Err(
                        "a `while` condition that needs temporaries is not supported".to_owned(),
                    );
                }
                self.push_line(&format!("while {text} {{"));
                self.block(body, true);
                self.push_line("}");
                Ok(())
            }
        }
    }

    fn declaration(&mut self, var: bool, names: &[String], value: &Expr) -> Result<(), String> {
        if let [name] = names
            && let Expr::Call { name: callee, args } = value
            && matches!(callee.as_str(), "plot" | "hline")
        {
            return self.output_call(callee, args, Some(name));
        }
        if var && self.depth > 0 {
            return Err("`var` declarations inside blocks are not supported".to_owned());
        }
        let value_out = if var {
            self.lazily(|s| s.expr(value))?
        } else {
            self.expr(value)?
        };
        match (names, value_out) {
            ([name], Val::One(text, _)) => {
                let is_input = text.starts_with("input.");
                let simple = is_input && !text.starts_with("input.source");
                self.flush_pre();
                let out = self.declare(name, true);
                self.push_line(&if var {
                    format!("var {out} = {text}")
                } else {
                    format!("{out} = {text}")
                });
                if simple {
                    self.simple.insert(out);
                }
                Ok(())
            }
            (names, Val::Many(items)) if names.len() == items.len() && names.len() > 1 => {
                // Separate bindings, not a destructuring: destructured names cannot retain history.
                self.flush_pre();
                for (name, item) in names.iter().zip(&items) {
                    let out = self.declare(name, true);
                    self.push_line(&format!("{out} = {item}"));
                }
                Ok(())
            }
            _ => Err("the number of names does not match the value".to_owned()),
        }
    }

    // ---- expressions --------------------------------------------------------------------

    pub(crate) fn one(&mut self, expr: &Expr) -> Result<(String, u8), String> {
        match self.expr(expr)? {
            Val::One(text, precedence) => Ok((text, precedence)),
            Val::Many(_) => Err("a tuple cannot be used here".to_owned()),
        }
    }

    /// Translate and parenthesize for use as an operand needing `needed` precedence.
    pub(crate) fn operand(&mut self, expr: &Expr, needed: u8) -> Result<String, String> {
        let (text, precedence) = self.one(expr)?;
        Ok(paren(&text, precedence, needed))
    }

    /// Whether `expr` depends only on literals and simple inputs.
    pub(crate) fn is_simple(&self, expr: &Expr) -> bool {
        match expr {
            Expr::Number(_) | Expr::Str(_) | Expr::Color(_) => true,
            Expr::Ident(name) => {
                !self.mutable.contains(name)
                    && !self
                        .frames
                        .last()
                        .is_some_and(|f| f.subst.contains_key(name))
                    && self
                        .simple
                        .contains(&self.renamed.get(name).cloned().unwrap_or_default())
            }
            Expr::Unary(_, inner) => self.is_simple(inner),
            Expr::Binary(_, a, b) => self.is_simple(a) && self.is_simple(b),
            Expr::Ternary(a, b, c) => self.is_simple(a) && self.is_simple(b) && self.is_simple(c),
            _ => false,
        }
    }

    pub(crate) fn expr(&mut self, expr: &Expr) -> Result<Val, String> {
        match expr {
            Expr::Number(number) => Ok(Val::One(number.clone(), ATOM)),
            Expr::Str(text) => Ok(Val::One(quote(text), ATOM)),
            Expr::Color(_) => Ok(Val::One(self.color_expr(expr)?, ATOM)),
            Expr::Ident(name) => self.ident(name),
            Expr::Call { name, args } => self.call(name, args),
            Expr::Index(base, offset) => self.index(base, offset),
            Expr::Unary(op, inner) => {
                let text = self.operand(inner, UNARY)?;
                let text = if *op == "-" && text.starts_with('-') {
                    format!("({text})")
                } else {
                    text
                };
                Ok(Val::One(format!("{op}{text}"), UNARY))
            }
            Expr::Binary(op, left, right) => self.binary(op, left, right),
            Expr::Ternary(cond, when_true, when_false) => {
                let simple = self.is_simple(cond);
                let (c, _) = self.one(cond)?;
                let (a, b) = if simple {
                    (self.one(when_true)?.0, self.one(when_false)?.0)
                } else {
                    self.lazily(|s| Ok::<_, String>((s.one(when_true)?.0, s.one(when_false)?.0)))?
                };
                Ok(Val::One(format!("select({c}, {a}, {b})"), ATOM))
            }
            Expr::Tuple(items) => {
                let mut texts = Vec::new();
                for item in items {
                    texts.push(self.one(item)?.0);
                }
                Ok(Val::Many(texts))
            }
            Expr::Switch { subject, arms } => self.switch(subject.as_deref(), arms),
        }
    }

    fn binary(&mut self, op: &'static str, left: &Expr, right: &Expr) -> Result<Val, String> {
        if matches!(op, "==" | "!=") {
            let is_na = |e: &Expr| matches!(e, Expr::Ident(name) if name == "na");
            let other = if is_na(right) {
                Some(left)
            } else if is_na(left) {
                Some(right)
            } else {
                None
            };
            if let Some(other) = other {
                let (text, _) = self.one(other)?;
                return Ok(if op == "==" {
                    Val::One(format!("na({text})"), ATOM)
                } else {
                    Val::One(format!("!na({text})"), UNARY)
                });
            }
        }
        let precedence = match op {
            "*" | "/" | "%" => MUL,
            "+" | "-" => ADD,
            "<" | "<=" | ">" | ">=" => REL,
            "==" | "!=" => EQ,
            "&&" => AND,
            _ => OR,
        };
        let l = self.operand(left, precedence)?;
        let lazy = matches!(op, "&&" | "||") && !self.is_simple(left);
        let r = if lazy {
            self.lazily(|s| s.operand(right, precedence + 1))?
        } else {
            self.operand(right, precedence + 1)?
        };
        Ok(Val::One(format!("{l} {op} {r}"), precedence))
    }

    fn index(&mut self, base: &Expr, offset: &Expr) -> Result<Val, String> {
        let literal = literal_number(offset);
        let offset_text = match &literal {
            Some(number) if number.contains('.') || number.starts_with('-') => {
                return Err("history offsets must be non-negative integers".to_owned());
            }
            Some(number) => number.clone(),
            None => {
                if contains_division(offset) {
                    return Err("division inside a history offset is not supported".to_owned());
                }
                self.header.dynamic_history = true;
                self.one(offset)?.0
            }
        };
        if offset_text == "0" {
            return self.expr(base);
        }
        if matches!(base, Expr::Number(_)) {
            return self.expr(base);
        }
        let (text, _) = self.one(base)?;
        let cur = self.history_cur(text)?;
        Ok(Val::One(self.history_at(&cur, &offset_text)?, ATOM))
    }

    fn switch(
        &mut self,
        subject: Option<&Expr>,
        arms: &[(Option<Expr>, Expr)],
    ) -> Result<Val, String> {
        let default = arms.iter().find(|(case, _)| case.is_none()).map(|(_, v)| v);
        let cases = arms
            .iter()
            .filter_map(|(case, value)| case.as_ref().map(|case| (case, value)))
            .collect::<Vec<_>>();
        if let Some(subject) = subject {
            let simple = self.is_simple(subject);
            let (head, _) = self.one(subject)?;
            let body = if simple {
                self.switch_arms(&cases, default)?
            } else {
                self.lazily(|s| s.switch_arms(&cases, default))?
            };
            return Ok(Val::One(format!("switch {head} {{ {body} }}"), ATOM));
        }
        // Without a subject, each arm is a condition: nested `select`.
        let mut parts = Vec::new();
        for (index, (case, value)) in cases.iter().enumerate() {
            let c = if index == 0 {
                self.one(case)?.0
            } else {
                self.lazily(|s| s.one(case))?.0
            };
            let v = self.lazily(|s| s.one(value))?.0;
            parts.push((c, v));
        }
        let mut text = match default {
            Some(value) => self.lazily(|s| s.one(value))?.0,
            None => "na".to_owned(),
        };
        for (c, v) in parts.into_iter().rev() {
            text = format!("select({c}, {v}, {text})");
        }
        Ok(Val::One(text, ATOM))
    }

    fn switch_arms(
        &mut self,
        cases: &[(&Expr, &Expr)],
        default: Option<&Expr>,
    ) -> Result<String, String> {
        let mut parts = Vec::new();
        for (case, value) in cases {
            let (c, _) = self.one(case)?;
            let (v, _) = self.one(value)?;
            parts.push(format!("{c} => {v}"));
        }
        let fallback = match default {
            Some(value) => self.one(value)?.0,
            None => "na".to_owned(),
        };
        parts.push(format!("default => {fallback}"));
        Ok(parts.join(", "))
    }

    fn ident(&mut self, name: &str) -> Result<Val, String> {
        if let Some(frame) = self.frames.last() {
            if let Some((text, precedence)) = frame.subst.get(name) {
                return Ok(Val::One(text.clone(), *precedence));
            }
            if let Some(renamed) = frame.rename.get(name) {
                return Ok(Val::One(renamed.clone(), ATOM));
            }
        }
        if self.declared.contains(name) {
            return Ok(Val::One(self.out_name(name), ATOM));
        }
        let text = match name {
            "open" | "high" | "low" | "close" | "volume" | "hl2" | "hlc3" | "ohlc4" | "hlcc4"
            | "time" => format!("chart.{name}"),
            "bar_index"
            | "na"
            | "true"
            | "false"
            | "barstate.isfirst"
            | "barstate.islast"
            | "barstate.isconfirmed"
            | "barstate.isrealtime" => name.to_owned(),
            "syminfo.mintick" => "instrument.min_tick".to_owned(),
            "math.pi" => "3.141592653589793".to_owned(),
            "math.e" => "2.718281828459045".to_owned(),
            _ => {
                if crate::calls::named_color(name).is_some() {
                    return Ok(Val::One(
                        self.color_expr(&Expr::Ident(name.to_owned()))?,
                        ATOM,
                    ));
                }
                if let Some((enum_name, field)) = name.split_once('.')
                    && let Some(fields) = self.enums.get(enum_name)
                {
                    return match fields.iter().find(|(f, _)| f == field) {
                        Some((_, title)) => Ok(Val::One(quote(title), ATOM)),
                        None => Err(format!("`{name}` is not a field of enum `{enum_name}`")),
                    };
                }
                if name.starts_with("ta.") {
                    return self.call(name, &[]);
                }
                return Err(format!("`{name}` is not supported or not defined"));
            }
        };
        Ok(Val::One(text, ATOM))
    }
}

/// Positional and named argument lookup shared by call translations.
pub(crate) fn arg<'a>(args: &'a [Arg], positions: &[&str], key: &str) -> Option<&'a Expr> {
    if let Some(named) = args.iter().find(|a| a.name.as_deref() == Some(key)) {
        return Some(&named.value);
    }
    let index = positions.iter().position(|p| *p == key)?;
    args.iter()
        .filter(|a| a.name.is_none())
        .nth(index)
        .map(|a| &a.value)
}

pub(crate) fn positional(args: &[Arg]) -> Vec<&Expr> {
    args.iter()
        .filter(|a| a.name.is_none())
        .map(|a| &a.value)
        .collect()
}

pub(crate) fn inline_limit_reached(depth: usize) -> bool {
    depth >= MAX_INLINE_DEPTH
}
