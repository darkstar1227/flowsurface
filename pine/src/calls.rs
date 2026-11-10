//! Translation of Pine function calls: built-ins, inputs, outputs, and user functions.

use std::collections::HashMap;

use crate::Unsupported;
use crate::expr::{Arg, Expr};
use crate::stmt::{Kind, Param, Stmt};
use crate::translate::{
    ATOM, Frame, PlotRef, Tr, UNARY, Val, arg, contains_division, inline_limit_reached,
    is_atom_text, literal_number, paren, positional, quote, unquote,
};

const MUL: u8 = 6;
const ADD: u8 = 5;
const AND: u8 = 2;
const OR: u8 = 1;

pub(crate) fn named_color(name: &str) -> Option<(u8, u8, u8)> {
    let hex: u32 = match name {
        "color.aqua" => 0x00BCD4,
        "color.black" => 0x363A45,
        "color.blue" => 0x2196F3,
        "color.fuchsia" => 0xE040FB,
        "color.gray" => 0x787B86,
        "color.green" => 0x4CAF50,
        "color.lime" => 0x00E676,
        "color.maroon" => 0x880E4F,
        "color.navy" => 0x311B92,
        "color.olive" => 0x808000,
        "color.orange" => 0xFF9800,
        "color.purple" => 0x9C27B0,
        "color.red" => 0xF23645,
        "color.silver" => 0xB2B5BE,
        "color.teal" => 0x089981,
        "color.white" => 0xFFFFFF,
        "color.yellow" => 0xFDD835,
        _ => return None,
    };
    Some(((hex >> 16) as u8, (hex >> 8) as u8, hex as u8))
}

fn alpha_from_transparency(transparency: f64) -> u8 {
    (255.0 * (1.0 - transparency / 100.0))
        .round()
        .clamp(0.0, 255.0) as u8
}

fn color_text((r, g, b, a): (u8, u8, u8, u8)) -> String {
    if a == 255 {
        format!("color.rgb({r}, {g}, {b})")
    } else {
        format!("color.rgb({r}, {g}, {b}, {a})")
    }
}

fn literal_component(expr: &Expr) -> Option<u8> {
    literal_number(expr)?.parse::<u8>().ok()
}

/// An RGBA color when `expr` is fully known at translation time.
fn literal_color(expr: &Expr) -> Option<(u8, u8, u8, u8)> {
    match expr {
        Expr::Color(hex) => {
            let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
            let alpha = if hex.len() == 8 { byte(6)? } else { 255 };
            Some((byte(0)?, byte(2)?, byte(4)?, alpha))
        }
        Expr::Ident(name) => named_color(name).map(|(r, g, b)| (r, g, b, 255)),
        Expr::Call { name, args } if name == "color.new" => {
            let (r, g, b, _) = literal_color(arg(args, &["color", "transp"], "color")?)?;
            let transparency = literal_number(arg(args, &["color", "transp"], "transp")?)?
                .parse::<f64>()
                .ok()?;
            Some((r, g, b, alpha_from_transparency(transparency)))
        }
        Expr::Call { name, args } if name == "color.rgb" => {
            let keys = ["red", "green", "blue", "transp"];
            let r = literal_component(arg(args, &keys, "red")?)?;
            let g = literal_component(arg(args, &keys, "green")?)?;
            let b = literal_component(arg(args, &keys, "blue")?)?;
            let a = match arg(args, &keys, "transp") {
                Some(t) => alpha_from_transparency(literal_number(t)?.parse::<f64>().ok()?),
                None => 255,
            };
            Some((r, g, b, a))
        }
        _ => None,
    }
}

fn slug(text: &str, separator: char) -> String {
    let mut out = String::new();
    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            out.push(character.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with(separator) {
            out.push(separator);
        }
    }
    out.trim_end_matches(separator).to_owned()
}

pub(crate) fn id_slug(text: &str) -> String {
    slug(text, '-')
}

impl Tr {
    pub(crate) fn color_expr(&mut self, expr: &Expr) -> Result<String, String> {
        if let Some(color) = literal_color(expr) {
            return Ok(color_text(color));
        }
        match expr {
            Expr::Call { name, args } if name == "color.new" => {
                let keys = ["color", "transp"];
                let base = arg(args, &keys, "color").ok_or("`color.new` needs a color")?;
                if let Some(text) = self.faded_choice(base, arg(args, &keys, "transp"))? {
                    return Ok(text);
                }
                // A runtime base color keeps its own alpha; the transparency cannot be applied.
                self.unsupported.push(Unsupported {
                    line: self.line,
                    reason: "`color.new` on a runtime color: the transparency is dropped"
                        .to_owned(),
                });
                self.color_expr(base)
            }
            Expr::Call { name, args } if name == "color.rgb" => {
                let keys = ["red", "green", "blue", "transp"];
                let mut parts = Vec::new();
                for key in &keys[..3] {
                    let value =
                        arg(args, &keys, key).ok_or("`color.rgb` needs three components")?;
                    parts.push(self.one(value)?.0);
                }
                Ok(format!("color.rgb({})", parts.join(", ")))
            }
            _ => Ok(self.one(expr)?.0),
        }
    }

    /// `color.new(cond ? a : b, t)` with static arms and a literal `t`: the transparency is
    /// applied to each arm. `base` may be the ternary itself or a top-level variable holding it.
    fn faded_choice(
        &mut self,
        base: &Expr,
        transparency: Option<&Expr>,
    ) -> Result<Option<String>, String> {
        let base = match base {
            Expr::Ident(name) if self.depth == 0 && self.frames.is_empty() => {
                self.color_defs.get(name).unwrap_or(base).clone()
            }
            other => other.clone(),
        };
        if !matches!(base, Expr::Ternary(..)) {
            return Ok(None);
        }
        let transparency = transparency
            .and_then(literal_number)
            .and_then(|t| t.parse::<f64>().ok());
        match transparency {
            Some(t) => self.fade(&base, alpha_from_transparency(t)),
            None => Ok(None),
        }
    }

    /// Apply `alpha` to a (possibly nested) ternary whose leaves are all static colors.
    fn fade(&mut self, expr: &Expr, alpha: u8) -> Result<Option<String>, String> {
        if let Some((r, g, b, _)) = literal_color(expr) {
            return Ok(Some(color_text((r, g, b, alpha))));
        }
        let Expr::Ternary(cond, when_true, when_false) = expr else {
            return Ok(None);
        };
        let (Some(a), Some(b)) = (self.fade(when_true, alpha)?, self.fade(when_false, alpha)?)
        else {
            return Ok(None);
        };
        let cond = self.one(cond)?.0;
        Ok(Some(format!("select({cond}, {a}, {b})")))
    }

    pub(crate) fn call(&mut self, name: &str, args: &[Arg]) -> Result<Val, String> {
        if let Some((params, body)) = self.functions.get(name).cloned() {
            return self.inline(name, &params, &body, args);
        }
        if name == "input" || name.starts_with("input.") {
            return self.input(name, args);
        }
        if let Some(function) = name.strip_prefix("ta.") {
            return self.ta(function, args);
        }
        let plain = args.iter().all(|a| a.name.is_none());
        match name {
            "na" => {
                let value = arg(args, &["x"], "x").ok_or("`na` needs one argument")?;
                Ok(Val::One(format!("na({})", self.one(value)?.0), ATOM))
            }
            "nz" => {
                let value = arg(args, &["source", "replacement"], "source")
                    .ok_or("`nz` needs an argument")?;
                let replacement = match arg(args, &["source", "replacement"], "replacement") {
                    Some(r) => self.one(r)?.0,
                    None => "0".to_owned(),
                };
                Ok(Val::One(
                    format!("nz({}, {replacement})", self.one(value)?.0),
                    ATOM,
                ))
            }
            "float" if args.len() == 1 => self.expr(&args[0].value),
            "math.abs" if plain && args.len() == 1 => Ok(Val::One(
                format!("math.abs({})", self.one(&args[0].value)?.0),
                ATOM,
            )),
            "math.min" | "math.max" if plain && args.len() >= 2 => {
                let mut parts = Vec::new();
                for a in args {
                    parts.push(self.one(&a.value)?.0);
                }
                Ok(Val::One(format!("{name}({})", parts.join(", ")), ATOM))
            }
            "math.avg" if plain && !args.is_empty() => {
                let mut parts = Vec::new();
                for a in args {
                    parts.push(self.operand(&a.value, ADD + 1)?);
                }
                let sum = parts.join(" + ");
                if parts.len() == 1 {
                    return Ok(Val::One(parts.remove(0), ATOM));
                }
                Ok(Val::One(format!("({sum}) / {}", parts.len()), MUL))
            }
            "color.new" | "color.rgb" => {
                let call = Expr::Call {
                    name: name.to_owned(),
                    args: args.to_vec(),
                };
                Ok(Val::One(self.color_expr(&call)?, ATOM))
            }
            "str.tostring" if !args.is_empty() => Ok(Val::One(
                format!("str.tostring({})", self.one(&args[0].value)?.0),
                ATOM,
            )),
            "int" => Err("`int()` casts are not supported".to_owned()),
            _ => Err(format!("`{name}` is not supported")),
        }
    }

    // ---- user functions -----------------------------------------------------------------

    fn inline(
        &mut self,
        name: &str,
        params: &[Param],
        body: &[Stmt],
        args: &[Arg],
    ) -> Result<Val, String> {
        if inline_limit_reached(self.inline_depth) {
            return Err(format!("`{name}` is recursive or nested too deeply"));
        }
        let given = positional(args);
        if given.len() > params.len() {
            return Err(format!("too many arguments for `{name}`"));
        }
        for named in args.iter().filter_map(|a| a.name.as_deref()) {
            if !params.iter().any(|p| p.name == named) {
                return Err(format!("`{name}` has no parameter `{named}`"));
            }
        }
        let mut subst = HashMap::new();
        for (index, param) in params.iter().enumerate() {
            let named = args
                .iter()
                .find(|a| a.name.as_deref() == Some(param.name.as_str()))
                .map(|a| &a.value);
            let expr = named
                .or_else(|| given.get(index).copied())
                .or(param.default.as_ref())
                .ok_or_else(|| format!("missing argument `{}` for `{name}`", param.name))?;
            let (text, precedence) = self.one(expr)?;
            let trivial = is_atom_text(&text)
                || (text.starts_with('-')
                    && text[1..].chars().all(|c| c.is_ascii_digit() || c == '.'));
            let bound = if trivial {
                (text, precedence)
            } else {
                (self.hoist_local(&text), ATOM)
            };
            subst.insert(param.name.clone(), bound);
        }
        self.frames.push(Frame {
            subst,
            rename: HashMap::new(),
        });
        self.inline_depth += 1;
        let result = self.inline_body(name, body);
        self.inline_depth -= 1;
        self.frames.pop();
        result
    }

    fn inline_body(&mut self, function: &str, body: &[Stmt]) -> Result<Val, String> {
        let Some((last, init)) = body.split_last() else {
            return Err("function without a body".to_owned());
        };
        for statement in init {
            match &statement.kind {
                Kind::Decl {
                    var: false,
                    names,
                    value,
                } => {
                    let value = self.expr(value)?;
                    let outs = names
                        .iter()
                        .map(|n| self.fresh(&format!("{function}_{n}")))
                        .collect::<Vec<_>>();
                    match (value, outs.as_slice()) {
                        (Val::One(text, _), [out]) => {
                            let immutable = !self.mutable.contains(&names[0]);
                            self.bind_pre(out, &text, immutable);
                        }
                        (Val::Many(items), outs) if outs.len() == items.len() => {
                            for ((name, out), item) in names.iter().zip(outs).zip(&items) {
                                let immutable = !self.mutable.contains(name);
                                self.bind_pre(out, item, immutable);
                            }
                        }
                        _ => return Err("the number of names does not match the value".to_owned()),
                    }
                    if let Some(frame) = self.frames.last_mut() {
                        for (name, out) in names.iter().zip(outs) {
                            frame.rename.insert(name.clone(), out);
                        }
                    }
                }
                Kind::Assign { name, value } => {
                    let target = self
                        .frames
                        .last()
                        .and_then(|f| f.rename.get(name).cloned())
                        .ok_or_else(|| format!("cannot assign to `{name}` inside a function"))?;
                    let (text, _) = self.one(value)?;
                    self.pre.push(format!("{target} := {text}"));
                    self.top_immut.remove(&target);
                    self.mark_mutable(&target);
                }
                _ => return Err("unsupported statement inside a function".to_owned()),
            }
        }
        match &last.kind {
            Kind::Expr(expr) => self.expr(expr),
            Kind::Decl {
                var: false,
                names,
                value,
            } if names.len() == 1 => self.expr(value),
            _ => Err("a function must end with an expression".to_owned()),
        }
    }

    // ---- ta.* ---------------------------------------------------------------------------

    /// A stateful `.ind` call over already translated arguments.
    fn st(&mut self, function: &str, args: &[&str]) -> Result<String, String> {
        Ok(self.stateful(format!("{function}({})", args.join(", ")))?.0)
    }

    fn window(&mut self, expr: Option<&Expr>) -> Result<String, String> {
        let expr = expr.ok_or("missing window length")?;
        if contains_division(expr) {
            return Err("division inside a window length is not supported".to_owned());
        }
        Ok(self.one(expr)?.0)
    }

    fn source(&mut self, expr: Option<&Expr>, default: &str) -> Result<String, String> {
        match expr {
            Some(expr) => Ok(self.one(expr)?.0),
            None => Ok(default.to_owned()),
        }
    }

    /// `(value, value[1])` with the value bound for history when necessary.
    fn history_pair(&mut self, expr: Option<&Expr>) -> Result<(String, String), String> {
        let expr = expr.ok_or("missing argument")?;
        if let Some(number) = literal_number(expr) {
            return Ok((number.clone(), number));
        }
        let (text, _) = self.one(expr)?;
        let cur = self.history_cur(text)?;
        let previous = self.history_at(&cur, "1")?;
        Ok((cur, previous))
    }

    fn change_offset(&mut self, expr: Option<&Expr>) -> Result<String, String> {
        match expr {
            None => Ok("1".to_owned()),
            Some(expr) => match literal_number(expr) {
                Some(n) if n.chars().all(|c| c.is_ascii_digit()) && n != "0" => Ok(n),
                Some(_) => Err("history offsets must be positive integers".to_owned()),
                None => {
                    if contains_division(expr) {
                        return Err("division inside a history offset is not supported".to_owned());
                    }
                    self.header.dynamic_history = true;
                    Ok(self.one(expr)?.0)
                }
            },
        }
    }

    fn ta(&mut self, function: &str, args: &[Arg]) -> Result<Val, String> {
        let one = |text: String| Ok(Val::One(text, ATOM));
        let base = |p: &[&str], key: &str| arg(args, p, key);
        match function {
            "sma" | "ema" | "rma" | "rsi" | "stdev" | "dev" => {
                let keys = ["source", "length"];
                let source = self.source(base(&keys, "source"), "")?;
                if source.is_empty() {
                    return Err(format!("`ta.{function}` needs a source"));
                }
                let length = self.window(base(&keys, "length"))?;
                one(self.st(&format!("ta.{function}"), &[&source, &length])?)
            }
            "vwma" => {
                let keys = ["source", "length"];
                let source = self.source(base(&keys, "source"), "chart.close")?;
                let length = self.window(base(&keys, "length"))?;
                one(self.st("ta.vwma", &[&source, "chart.volume", &length])?)
            }
            "highest" | "lowest" | "highestbars" | "lowestbars" => {
                let keys = ["source", "length"];
                let (first, second) = (base(&keys, "source"), base(&keys, "length"));
                let default = if function.starts_with("highest") {
                    "chart.high"
                } else {
                    "chart.low"
                };
                let (source, length) = match (first, second) {
                    (Some(source), Some(length)) => {
                        (self.one(source)?.0, self.window(Some(length))?)
                    }
                    (Some(length), None) | (None, Some(length)) => {
                        (default.to_owned(), self.window(Some(length))?)
                    }
                    (None, None) => return Err(format!("`ta.{function}` needs a length")),
                };
                let call = self.st(&format!("ta.{function}"), &[&source, &length])?;
                if function.ends_with("bars") {
                    // Pine reports bars back as zero or negative; `.ind` as a non-negative offset.
                    Ok(Val::One(format!("-{call}"), UNARY))
                } else {
                    one(call)
                }
            }
            "pivothigh" | "pivotlow" => {
                let keys = ["source", "leftbars", "rightbars"];
                let (a, b, c) = (
                    base(&keys, "source"),
                    base(&keys, "leftbars"),
                    base(&keys, "rightbars"),
                );
                let default = if function == "pivothigh" {
                    "chart.high"
                } else {
                    "chart.low"
                };
                let (source, left, right) = match (a, b, c) {
                    (Some(source), Some(left), Some(right)) => (self.one(source)?.0, left, right),
                    (Some(left), Some(right), None) => (default.to_owned(), left, right),
                    _ => return Err(format!("`ta.{function}` needs left and right lengths")),
                };
                let left = self.window(Some(left))?;
                let right = self.window(Some(right))?;
                one(self.st(&format!("ta.{function}"), &[&source, &left, &right])?)
            }
            "cum" => {
                let source = self.source(base(&["source"], "source"), "")?;
                if source.is_empty() {
                    return Err("`ta.cum` needs a source".to_owned());
                }
                one(self.st("ta.cum", &[&source])?)
            }
            "tr" => one(self.st("ta.tr", &["chart.high", "chart.low", "chart.close"])?),
            "atr" => {
                let length = self.window(base(&["length"], "length"))?;
                let tr = self.st("ta.tr", &["chart.high", "chart.low", "chart.close"])?;
                one(self.st("ta.rma", &[&tr, &length])?)
            }
            "range" => {
                let keys = ["source", "length"];
                let source = self.source(base(&keys, "source"), "")?;
                if source.is_empty() {
                    return Err("`ta.range` needs a source".to_owned());
                }
                let length = self.window(base(&keys, "length"))?;
                let high = self.st("ta.highest", &[&source, &length])?;
                let low = self.st("ta.lowest", &[&source, &length])?;
                Ok(Val::One(format!("{high} - {low}"), ADD))
            }
            "change" | "mom" | "roc" => {
                let keys = ["source", "length"];
                let source = base(&keys, "source");
                let (text, _) = self.one(source.ok_or("missing source")?)?;
                let source = self.history_cur(text)?;
                let offset = match (function, base(&keys, "length")) {
                    ("change", length) => self.change_offset(length)?,
                    (_, length) => {
                        if length.is_none() {
                            return Err(format!("`ta.{function}` needs a length"));
                        }
                        self.change_offset(length)?
                    }
                };
                let previous = self.history_at(&source, &offset)?;
                if function == "roc" {
                    Ok(Val::One(
                        format!("100 * ({source} - {previous}) / {previous}"),
                        MUL,
                    ))
                } else {
                    Ok(Val::One(format!("{source} - {previous}"), ADD))
                }
            }
            "crossover" | "crossunder" | "cross" => {
                let keys = ["source1", "source2"];
                let (a, a1) = self.history_pair(base(&keys, "source1"))?;
                let (b, b1) = self.history_pair(base(&keys, "source2"))?;
                let over = format!("{a} > {b} && {a1} <= {b1}");
                let under = format!("{a} < {b} && {a1} >= {b1}");
                match function {
                    "crossover" => Ok(Val::One(over, AND)),
                    "crossunder" => Ok(Val::One(under, AND)),
                    _ => Ok(Val::One(format!("{over} || {under}"), OR)),
                }
            }
            "stoch" => {
                let keys = ["source", "high", "low", "length"];
                let source = self.source(base(&keys, "source"), "")?;
                let high = self.source(base(&keys, "high"), "")?;
                let low = self.source(base(&keys, "low"), "")?;
                if source.is_empty() || high.is_empty() || low.is_empty() {
                    return Err("`ta.stoch` needs source, high, low, and length".to_owned());
                }
                let length = self.window(base(&keys, "length"))?;
                let highest = self.st("ta.highest", &[&high, &length])?;
                let highest = self.atom(highest);
                let lowest = self.st("ta.lowest", &[&low, &length])?;
                let lowest = self.atom(lowest);
                let source = paren(&source, ATOM, ADD);
                Ok(Val::One(
                    format!("100 * ({source} - {lowest}) / ({highest} - {lowest})"),
                    MUL,
                ))
            }
            "cci" => {
                let keys = ["source", "length"];
                let source = self.source(base(&keys, "source"), "")?;
                if source.is_empty() {
                    return Err("`ta.cci` needs a source".to_owned());
                }
                let length = self.window(base(&keys, "length"))?;
                let source = self.atom(source);
                let average = self.st("ta.sma", &[&source, &length])?;
                let deviation = self.st("ta.dev", &[&source, &length])?;
                Ok(Val::One(
                    format!("({source} - {average}) / (0.015 * {deviation})"),
                    MUL,
                ))
            }
            "bb" => {
                let keys = ["series", "length", "mult"];
                let source = self.source(base(&keys, "series"), "")?;
                if source.is_empty() {
                    return Err("`ta.bb` needs a source".to_owned());
                }
                let length = self.window(base(&keys, "length"))?;
                let multiplier = match base(&keys, "mult") {
                    Some(m) => self.operand(m, MUL + 1)?,
                    None => return Err("`ta.bb` needs a multiplier".to_owned()),
                };
                let basis = self.st("ta.sma", &[&source, &length])?;
                let basis = self.atom(basis);
                let deviation = self.st("ta.stdev", &[&source, &length])?;
                let deviation = self.atom(format!("{multiplier} * {deviation}"));
                Ok(Val::Many(vec![
                    basis.clone(),
                    format!("{basis} + {deviation}"),
                    format!("{basis} - {deviation}"),
                ]))
            }
            "macd" => {
                let keys = ["source", "fastlen", "slowlen", "siglen"];
                let source = self.source(base(&keys, "source"), "")?;
                if source.is_empty() {
                    return Err("`ta.macd` needs a source".to_owned());
                }
                let fast = self.window(base(&keys, "fastlen"))?;
                let slow = self.window(base(&keys, "slowlen"))?;
                let signal = self.window(base(&keys, "siglen"))?;
                let fast = self.st("ta.ema", &[&source, &fast])?;
                let slow = self.st("ta.ema", &[&source, &slow])?;
                let macd = self.atom(format!("{fast} - {slow}"));
                let signal = self.st("ta.ema", &[&macd, &signal])?;
                let signal = self.atom(signal);
                Ok(Val::Many(vec![
                    macd.clone(),
                    signal.clone(),
                    format!("{macd} - {signal}"),
                ]))
            }
            "dmi" => {
                let keys = ["diLength", "adxSmoothing"];
                let di = self.window(base(&keys, "diLength"))?;
                let smoothing = self.window(base(&keys, "adxSmoothing"))?;
                let up = self.atom("chart.high - chart.high[1]".to_owned());
                let down = self.atom("chart.low[1] - chart.low".to_owned());
                let plus_move = self.atom(format!(
                    "select(na({up}), na, select({up} > {down} && {up} > 0, {up}, 0))"
                ));
                let minus_move = self.atom(format!(
                    "select(na({down}), na, select({down} > {up} && {down} > 0, {down}, 0))"
                ));
                let tr = self.st("ta.tr", &["chart.high", "chart.low", "chart.close"])?;
                let range = self.st("ta.rma", &[&tr, &di])?;
                let range = self.atom(range);
                let plus_avg = self.st("ta.rma", &[&plus_move, &di])?;
                let plus = self.atom(format!("100 * {plus_avg} / {range}"));
                let minus_avg = self.st("ta.rma", &[&minus_move, &di])?;
                let minus = self.atom(format!("100 * {minus_avg} / {range}"));
                let ratio = format!(
                    "math.abs({plus} - {minus}) / select({plus} + {minus} == 0, 1, {plus} + {minus})"
                );
                let adx = self.st("ta.rma", &[&ratio, &smoothing])?;
                Ok(Val::Many(vec![plus, minus, adx]))
            }
            "vwap" => {
                if args.len() > 1 {
                    return Err("`ta.vwap` anchors and bands are not supported".to_owned());
                }
                let source = self.source(base(&["src"], "src"), "chart.hlc3")?;
                one(self.st("ta.avwap", &[&source, "chart.volume", "\"Day\""])?)
            }
            "obv" => {
                let change = "chart.close - chart.close[1]";
                let text = format!(
                    "select({change} > 0, chart.volume, select({change} < 0, -chart.volume, 0))"
                );
                one(self.st("ta.cum", &[&text])?)
            }
            "valuewhen" => {
                let keys = ["condition", "source", "occurrence"];
                let occurrence = base(&keys, "occurrence").and_then(literal_number);
                if occurrence.as_deref() != Some("0") {
                    return Err("`ta.valuewhen` is only supported with occurrence 0".to_owned());
                }
                let condition = base(&keys, "condition").ok_or("missing condition")?;
                let source = base(&keys, "source").ok_or("missing source")?;
                let (text, _) = self.one(condition)?;
                let condition = self.atom(text);
                let (text, _) = self.one(source)?;
                let source = self.atom(text);
                let previous =
                    self.hoist_top(&format!("ta.previous_when({condition}, {source})"))?;
                one(format!("select({condition}, {source}, {previous})"))
            }
            "barssince" => {
                let condition = base(&["condition"], "condition").ok_or("missing condition")?;
                let (text, _) = self.one(condition)?;
                let condition = self.atom(text);
                let name = self.fresh("bars");
                self.push_top_lines(
                    vec![
                        format!("var {name} = na"),
                        format!("{name} := select({condition}, 0, {name} + 1)"),
                    ],
                    &condition,
                )?;
                one(name)
            }
            _ => Err(format!("`ta.{function}` is not available")),
        }
    }

    // ---- inputs -------------------------------------------------------------------------

    pub(crate) fn input(&mut self, name: &str, args: &[Arg]) -> Result<Val, String> {
        if self.depth > 0 || !self.frames.is_empty() {
            return Err("inputs must be declared at the top level".to_owned());
        }
        let layout: &[&str] = match name {
            "input.int" | "input.float" => {
                let third_is_options = positional(args)
                    .get(2)
                    .is_some_and(|e| matches!(e, Expr::Tuple(_)));
                if third_is_options || args.iter().any(|a| a.name.as_deref() == Some("options")) {
                    &[
                        "defval", "title", "options", "tooltip", "inline", "group", "confirm",
                    ]
                } else {
                    &[
                        "defval", "title", "minval", "maxval", "step", "tooltip", "inline",
                        "group", "confirm",
                    ]
                }
            }
            "input.string" | "input.enum" => &[
                "defval", "title", "options", "tooltip", "inline", "group", "confirm",
            ],
            "input.bool" | "input.color" | "input.source" | "input.time" | "input.price"
            | "input" => &["defval", "title", "tooltip", "inline", "group", "confirm"],
            other => return Err(format!("`{other}` is not supported")),
        };
        let get = |key: &str| arg(args, layout, key);
        let default = get("defval").ok_or("an input needs a default value")?;

        let kind = match name {
            "input.int" => "int",
            "input.float" | "input.price" => "float",
            "input.bool" => "bool",
            "input.string" => "string",
            "input.source" => "source",
            "input.color" => "color",
            "input.time" => "time",
            "input.enum" => "enum",
            _ => self.infer_input_kind(default)?,
        };
        let mut parts = Vec::new();
        let mut named = Vec::new();
        match kind {
            "int" | "time" => {
                let number = literal_number(default).ok_or("input defaults must be literals")?;
                if number.contains('.') || number.contains('e') {
                    return Err("an integer input needs an integer default".to_owned());
                }
                parts.push(number);
            }
            "float" => {
                let mut number =
                    literal_number(default).ok_or("input defaults must be literals")?;
                if !number.contains('.') && !number.contains('e') {
                    number.push_str(".0");
                }
                parts.push(number);
            }
            "bool" => match default {
                Expr::Ident(word) if word == "true" || word == "false" => parts.push(word.clone()),
                _ => return Err("input defaults must be literals".to_owned()),
            },
            "string" => match default {
                Expr::Str(text) => parts.push(quote(text)),
                _ => return Err("input defaults must be literals".to_owned()),
            },
            "color" => {
                let color = literal_color(default).ok_or("input colors must be literals")?;
                parts.push(color_text(color));
            }
            "source" => {
                let (text, _) = self.one(default)?;
                let allowed =
                    text == "open_interest" || (text.starts_with("chart.") && text != "chart.time");
                if !allowed {
                    return Err(
                        "an input source must default to a price field or volume".to_owned()
                    );
                }
                parts.push(text);
            }
            _ => {
                // enum
                let Expr::Ident(path) = default else {
                    return Err("an enum input needs an enum member default".to_owned());
                };
                let (enum_name, field) =
                    path.split_once('.').ok_or("an enum input needs a member")?;
                let fields = self.enums.get(enum_name).cloned().ok_or("unknown enum")?;
                let chosen = fields
                    .iter()
                    .find(|(f, _)| f == field)
                    .ok_or("unknown enum member")?;
                parts.push(quote(&chosen.1));
                let titles = match get("options") {
                    Some(Expr::Tuple(items)) => {
                        let mut titles = Vec::new();
                        for item in items {
                            let Expr::Ident(path) = item else {
                                return Err("enum options must be enum members".to_owned());
                            };
                            let member = path
                                .split_once('.')
                                .and_then(|(_, f)| fields.iter().find(|(name, _)| name == f));
                            titles.push(quote(&member.ok_or("unknown enum member")?.1));
                        }
                        titles
                    }
                    _ => fields.iter().map(|(_, t)| quote(t)).collect(),
                };
                named.push(format!("options = [{}]", titles.join(", ")));
            }
        }
        if let Some(Expr::Str(label)) = get("title") {
            parts.push(quote(label));
        }
        if kind == "int" || kind == "float" {
            if let Some(Expr::Tuple(items)) = get("options") {
                let numbers = items
                    .iter()
                    .filter_map(literal_number)
                    .filter_map(|n| n.parse::<f64>().ok())
                    .collect::<Vec<_>>();
                if numbers.len() != items.len() || numbers.is_empty() {
                    return Err("numeric `options` must be literals".to_owned());
                }
                return Err("numeric `options` lists are not supported".to_owned());
            }
            for (pine, ind) in [("minval", "min"), ("maxval", "max"), ("step", "step")] {
                if let Some(number) = get(pine).and_then(literal_number) {
                    named.push(format!("{ind} = {number}"));
                }
            }
        }
        if kind == "string"
            && let Some(options) = get("options")
        {
            let Expr::Tuple(items) = options else {
                return Err("string `options` must be a literal list".to_owned());
            };
            let mut texts = Vec::new();
            for item in items {
                let Expr::Str(text) = item else {
                    return Err("string `options` must be a literal list".to_owned());
                };
                texts.push(quote(text));
            }
            named.push(format!("options = [{}]", texts.join(", ")));
        }
        for key in ["tooltip", "inline", "group"] {
            if let Some(Expr::Str(text)) = get(key) {
                named.push(format!("{key} = {}", quote(text)));
            }
        }
        let output_kind = if kind == "enum" { "string" } else { kind };
        parts.extend(named);
        Ok(Val::One(
            format!("input.{output_kind}({})", parts.join(", ")),
            ATOM,
        ))
    }

    fn infer_input_kind(&mut self, default: &Expr) -> Result<&'static str, String> {
        match default {
            Expr::Number(number) if number.contains('.') => Ok("float"),
            Expr::Number(_) => Ok("int"),
            Expr::Unary("-", inner) => self.infer_input_kind(inner),
            Expr::Str(_) => Ok("string"),
            Expr::Color(_) => Ok("color"),
            Expr::Ident(word) if word == "true" || word == "false" => Ok("bool"),
            Expr::Ident(word) if named_color(word).is_some() => Ok("color"),
            Expr::Call { name, .. } if name == "color.new" || name == "color.rgb" => Ok("color"),
            Expr::Ident(word)
                if matches!(
                    word.as_str(),
                    "open"
                        | "high"
                        | "low"
                        | "close"
                        | "volume"
                        | "hl2"
                        | "hlc3"
                        | "ohlc4"
                        | "hlcc4"
                ) =>
            {
                Ok("source")
            }
            _ => Err("cannot infer the input type".to_owned()),
        }
    }

    // ---- statement-level calls ----------------------------------------------------------

    pub(crate) fn expression_statement(&mut self, expr: &Expr) -> Result<(), String> {
        let Expr::Call { name, args } = expr else {
            return Err("this expression has no effect".to_owned());
        };
        match name.as_str() {
            "indicator" => self.indicator(args),
            "plot" | "hline" => self.output_call(name, args, None),
            "fill" => self.fill(args),
            "strategy" | "library" => Err(format!("`{name}` scripts are not supported")),
            "alertcondition" | "alert" => Err("alerts are not available".to_owned()),
            _ => Err(format!("`{name}` is not supported")),
        }
    }

    fn indicator(&mut self, args: &[Arg]) -> Result<(), String> {
        if self.depth > 0 || self.header.seen {
            return Err("`indicator` must appear once at the top level".to_owned());
        }
        let keys = [
            "title",
            "shorttitle",
            "overlay",
            "format",
            "precision",
            "scale",
            "max_bars_back",
        ];
        match arg(args, &keys, "title") {
            Some(Expr::Str(title)) => self.header.title = Some(title.clone()),
            _ => return Err("`indicator` needs a literal title".to_owned()),
        }
        self.header.overlay =
            matches!(arg(args, &keys, "overlay"), Some(Expr::Ident(word)) if word == "true");
        self.header.precision = arg(args, &keys, "precision")
            .and_then(literal_number)
            .and_then(|n| n.parse::<u32>().ok())
            .filter(|n| *n <= 18);
        self.header.max_bars_back = arg(args, &keys, "max_bars_back")
            .and_then(literal_number)
            .and_then(|n| n.parse::<u32>().ok())
            .filter(|n| (1..=50_000).contains(n));
        self.header.seen = true;
        Ok(())
    }

    fn plot_id(&mut self, title: Option<&str>, variable: Option<&str>) -> String {
        let mut base = title.map(|t| slug(t, '_')).unwrap_or_default();
        if base.is_empty() {
            base = variable.map(|v| slug(v, '_')).unwrap_or_default();
        }
        if base.is_empty() {
            base = "plot".to_owned();
        }
        let mut id = base.clone();
        let mut number = 2;
        while !self.plot_ids.insert(id.clone()) {
            id = format!("{base}_{number}");
            number += 1;
        }
        id
    }

    pub(crate) fn output_call(
        &mut self,
        name: &str,
        args: &[Arg],
        variable: Option<&str>,
    ) -> Result<(), String> {
        if self.depth > 0 || !self.frames.is_empty() {
            return Err("outputs must be declared at the top level".to_owned());
        }
        let hline = name == "hline";
        if hline {
            self.notes.push(Unsupported {
                line: self.line,
                reason: "hline is rendered as a constant plot line".to_owned(),
            });
        }
        let keys: &[&str] = if hline {
            &[
                "price",
                "title",
                "color",
                "linestyle",
                "linewidth",
                "editable",
                "display",
            ]
        } else {
            &[
                "series",
                "title",
                "color",
                "linewidth",
                "style",
                "trackprice",
                "histbase",
                "offset",
                "join",
                "editable",
                "show_last",
                "display",
            ]
        };
        let series = arg(args, keys, if hline { "price" } else { "series" })
            .ok_or_else(|| format!("`{name}` needs a value"))?;
        let (value, _) = self.one(series)?;
        let title_text = match arg(args, keys, "title") {
            Some(Expr::Str(text)) => Some(text.clone()),
            _ => None,
        };
        let id = self.plot_id(title_text.as_deref().map(unquote).as_deref(), variable);
        let title = title_text
            .map(|t| quote(&t))
            .unwrap_or_else(|| quote(variable.unwrap_or("Plot")));
        let mut parts = vec![value, format!("id = \"{id}\""), format!("title = {title}")];
        if let Some(color) = arg(args, keys, "color") {
            parts.push(format!("color = {}", self.color_expr(color)?));
        }
        if let Some(width) = arg(args, keys, "linewidth").and_then(literal_number)
            && width.chars().all(|c| c.is_ascii_digit())
            && width != "0"
        {
            parts.push(format!("width = {width}"));
        }
        if !hline && let Some(Expr::Ident(style)) = arg(args, keys, "style") {
            let style = match style.as_str() {
                "plot.style_stepline" | "plot.style_stepline_diamond" | "plot.style_steplinebr" => {
                    "step"
                }
                "plot.style_histogram" | "plot.style_columns" => "histogram",
                "plot.style_area" | "plot.style_areabr" => "area",
                _ => "line",
            };
            if style != "line" {
                parts.push(format!("style = \"{style}\""));
            }
        }
        if !hline
            && let Some(offset) = arg(args, keys, "offset").and_then(literal_number)
            && !offset.contains('.')
        {
            parts.push(format!("offset = {offset}"));
        }
        if self.header.overlay {
            parts.push("pane = \"main\"".to_owned());
        }
        self.flush_pre();
        self.push_line(&format!("plot({})", parts.join(", ")));
        if let Some(variable) = variable {
            self.plots.insert(
                variable.to_owned(),
                PlotRef {
                    id,
                    constant: hline,
                },
            );
        }
        Ok(())
    }

    fn fill(&mut self, args: &[Arg]) -> Result<(), String> {
        let keys = ["plot1", "plot2", "color", "title"];
        let lookup = |tr: &Tr, key: &str| match arg(args, &keys, key) {
            Some(Expr::Ident(name)) => tr.plots.get(name).map(|p| (p.id.clone(), p.constant)),
            _ => None,
        };
        let (Some((upper, upper_constant)), Some((lower, lower_constant))) =
            (lookup(self, "plot1"), lookup(self, "plot2"))
        else {
            return Err("`fill` needs two plot or hline variables".to_owned());
        };
        let opacity = match arg(args, &keys, "color") {
            Some(Expr::Call {
                name,
                args: color_args,
            }) if name == "color.new" => arg(color_args, &["color", "transp"], "transp")
                .and_then(literal_number)
                .and_then(|t| t.parse::<f64>().ok())
                .map(|t| ((100.0 - t).clamp(0.0, 100.0) * 10.0).round() / 1000.0),
            _ => None,
        }
        .unwrap_or(0.12);
        self.fill_count += 1;
        let slot = format!("fill{}", self.fill_count);
        let geometry = if upper_constant && lower_constant {
            ""
        } else {
            ", geometry = \"series_band\""
        };
        self.tail.push(format!(
            "style(\"{slot}\", title = \"Fill {}\", role = \"primary\", shade = \"faint\")",
            self.fill_count
        ));
        self.tail.push(format!(
            "fill(upper = \"{upper}\", lower = \"{lower}\", color_slot = \"{slot}\"{geometry}, opacity = {opacity})"
        ));
        Ok(())
    }
}
