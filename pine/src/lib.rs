//! Translate a subset of Pine Script v5/v6 into Flowsurface `.ind` indicator sources.
//!
//! Anything that cannot be translated is kept in the output as a `// UNSUPPORTED:` comment
//! and reported in [`Transpiled::unsupported`].

mod calls;
mod expr;
mod lexer;
mod lines;
mod stmt;
mod translate;

use std::collections::HashSet;

use lexer::{Token, tokenize};
use lines::Node;
use translate::{Tr, quote, unquote};

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Indicator ID (`namespace.name`); defaults to `pine.<slugified title>`.
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    /// 1-based Pine source line.
    pub line: usize,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct Transpiled {
    pub source: String,
    pub id: String,
    pub unsupported: Vec<Unsupported>,
    /// Conversions that work but are not one-to-one (e.g. `hline` becoming a plot).
    pub notes: Vec<Unsupported>,
}

fn collect_names(nodes: &[Node], names: &mut HashSet<String>) {
    for node in nodes {
        if let Ok(tokens) = tokenize(&node.text) {
            for token in tokens {
                if let Token::Ident(name) = token {
                    names.extend(name.split('.').map(str::to_owned));
                    names.insert(name);
                }
            }
        }
        collect_names(&node.children, names);
    }
}

/// The `//@version=N` annotation: its line and number.
fn find_version(source: &str) -> Option<(usize, String)> {
    for (index, line) in source.lines().enumerate() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("//@version")
            && let Some(value) = rest.trim_start().strip_prefix('=')
        {
            return Some((index + 1, value.trim().to_owned()));
        }
    }
    None
}

fn fixed_step(digits: u32) -> String {
    if digits == 0 {
        "1".to_owned()
    } else {
        format!("0.{}1", "0".repeat(digits as usize - 1))
    }
}

/// Translate Pine source into an `.ind` source. Never panics on malformed input.
pub fn transpile(source: &str, options: &Options) -> Transpiled {
    let mut unsupported = Vec::new();
    match find_version(source) {
        Some((_, version)) if version == "5" || version == "6" => {}
        Some((line, version)) => unsupported.push(Unsupported {
            line,
            reason: format!("Pine version {version} is not supported (only 5 and 6)"),
        }),
        None => unsupported.push(Unsupported {
            line: 1,
            reason: "missing //@version=5 or //@version=6 annotation; assumed version 5".to_owned(),
        }),
    }

    let nodes = lines::parse_lines(source);
    let statements = stmt::parse_nodes(&nodes);
    let mut names = HashSet::new();
    collect_names(&nodes, &mut names);

    let mut tr = Tr::new(&statements, names);
    tr.run(&statements);
    unsupported.append(&mut tr.unsupported);
    let notes = std::mem::take(&mut tr.notes);

    let raw_title = tr.header.title.clone();
    let title = raw_title
        .clone()
        .unwrap_or_else(|| "Pine script".to_owned());
    let name = calls::id_slug(&unquote(&title));
    let id = options
        .id
        .clone()
        .unwrap_or_else(|| format!("pine.{}", if name.is_empty() { "script" } else { &name }));
    let overlay = tr.header.overlay;
    let precision = match tr.header.precision {
        Some(digits) => format!("fixed_step:{}", fixed_step(digits)),
        None if overlay => "base_ticker_min_tick".to_owned(),
        None => "fixed_step:0.01".to_owned(),
    };
    let max_bars_back = match (tr.header.max_bars_back, tr.header.dynamic_history) {
        (Some(bars), _) => format!(", max_bars_back = {bars}"),
        (None, true) => ", max_bars_back = 5000".to_owned(),
        (None, false) => String::new(),
    };
    let mut output = vec![
        "language(1)".to_owned(),
        format!("indicator({}, id = \"{id}\"{max_bars_back})", quote(&title)),
        format!(
            "display(view = \"plot\", attachment = \"{}\")",
            if overlay { "overlay" } else { "panel" }
        ),
        format!("format(precision = \"{precision}\")"),
    ];
    output.append(&mut tr.prelude);
    output.append(&mut tr.out);
    output.append(&mut tr.prev_updates);
    output.append(&mut tr.tail);
    let mut source = output.join("\n");
    source.push('\n');
    Transpiled {
        source,
        id,
        unsupported,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translate(source: &str) -> Transpiled {
        transpile(source, &Options::default())
    }

    fn compiled(out: &Transpiled, name: &str) {
        if let Err(errors) = indicator_engine::compile(&out.source, format!("{name}.ind")) {
            let messages = errors
                .iter()
                .map(|e| e.message.clone())
                .collect::<Vec<_>>()
                .join("\n");
            panic!(
                "{name} does not compile:\n{messages}\n--- source ---\n{}",
                out.source
            );
        }
    }

    /// With `PINE_DUMP_DIR` set, write translated sources for a `flowsurface-indicator test` run.
    fn dump(name: &str, out: &Transpiled) {
        if let Ok(dir) = std::env::var("PINE_DUMP_DIR") {
            std::fs::write(format!("{dir}/{name}.ind"), &out.source).expect("dump source");
        }
    }

    fn fixture(name: &str, source: &str, expected: &[&str]) -> Transpiled {
        let out = transpile(
            source,
            &Options {
                id: Some(format!("pine.{name}")),
            },
        );
        let reasons = out
            .unsupported
            .iter()
            .map(|u| u.reason.as_str())
            .collect::<Vec<_>>();
        assert_eq!(reasons, expected, "{name}: unsupported items");
        dump(name, &out);
        compiled(&out, name);
        out
    }

    #[test]
    fn ma_crossover_fixture() {
        let out = fixture(
            "ma-crossover",
            include_str!("../tests/fixtures/ma-crossover.pine"),
            &["alerts are not available"],
        );
        assert!(out.source.contains("// UNSUPPORTED: alertcondition"));
        assert!(out.source.contains("attachment = \"overlay\""));
        assert!(out.source.contains("pane = \"main\""));
    }

    #[test]
    fn color_new_keeps_transparency_on_static_choices() {
        let out = translate(
            "//@version=5\nindicator(\"T\", overlay = true)\nt = close > open ? color.green : color.red\nplot(close, color = color.new(t, 50))\nplot(open, color = color.new(close > open ? color.lime : color.maroon, 100))\n",
        );
        assert!(out.unsupported.is_empty(), "{:?}", out.unsupported);
        assert!(
            out.source.contains(
                "select(chart.close > chart.open, color.rgb(76, 175, 80, 128), color.rgb(242, 54, 69, 128))"
            ),
            "{}",
            out.source
        );
        assert!(out.source.contains(", 0), color.rgb("), "{}", out.source);
    }

    #[test]
    fn color_new_on_runtime_color_reports_dropped_transparency() {
        let out = translate(
            "//@version=5\nindicator(\"T\")\nc = color.rgb(close > open ? 255 : 0, 0, 0)\nplot(close, color = color.new(c, 50))\n",
        );
        assert_eq!(out.unsupported.len(), 1, "{:?}", out.unsupported);
        assert_eq!(out.unsupported[0].line, 4);
        assert!(
            out.unsupported[0]
                .reason
                .contains("transparency is dropped")
        );
    }

    #[test]
    fn hline_is_noted_and_fill_opacity_follows_color() {
        let out = translate(
            "//@version=5\nindicator(\"T\")\nh1 = hline(70)\nh2 = hline(30)\nfill(h1, h2, color = color.new(color.blue, 90))\nfill(h1, h2)\nplot(close)\n",
        );
        assert!(out.unsupported.is_empty());
        let lines = out.notes.iter().map(|n| n.line).collect::<Vec<_>>();
        assert_eq!(lines, [3, 4]);
        assert!(out.notes[0].reason.contains("constant plot line"));
        assert!(out.source.contains("opacity = 0.1)"), "{}", out.source);
        assert!(out.source.contains("opacity = 0.12)"), "{}", out.source);
    }

    #[test]
    fn rsi_macd_fixture() {
        let out = fixture(
            "rsi-macd-panel",
            include_str!("../tests/fixtures/rsi-macd-panel.pine"),
            &[],
        );
        assert!(out.source.contains("fixed_step:0.01"));
        assert_eq!(out.source.matches("\nfill(").count(), 2);
    }

    #[test]
    fn function_switch_fixture() {
        fixture(
            "function-switch",
            include_str!("../tests/fixtures/function-switch.pine"),
            &[],
        );
    }

    #[test]
    fn enum_state_fixture() {
        let out = fixture(
            "enum-state",
            include_str!("../tests/fixtures/enum-state.pine"),
            &[],
        );
        assert!(
            out.source
                .contains("options = [\"Fast\", \"Slow\", \"balanced\"]")
        );
    }

    #[test]
    fn for_loop_fixture() {
        let out = fixture(
            "for-loop",
            include_str!("../tests/fixtures/for-loop.pine"),
            &[],
        );
        assert!(out.source.contains("max_bars_back = 5000"));
        assert!(out.source.contains("fixed_step:0.1"));
    }

    /// One minimal snippet per supported construct: all must translate and compile.
    #[test]
    fn supported_constructs_translate_and_compile() {
        let snippets: &[(&str, &str)] = &[
            ("dmi", "[p, m, a] = ta.dmi(14, 14)\nplot(a)\nplot(p - m)"),
            ("stoch", "plot(ta.stoch(close, high, low, 14))"),
            ("cci", "plot(ta.cci(hlc3, 20))"),
            (
                "bb",
                "[b, u, l] = ta.bb(close, 20, 2.0)\nplot(b)\nplot(u)\nplot(l)",
            ),
            (
                "macd",
                "[m, s, h] = ta.macd(close, 12, 26, 9)\nplot(m)\nplot(s)\nplot(h)",
            ),
            ("obv", "plot(ta.obv)"),
            ("vwap", "plot(ta.vwap)\nplot(ta.vwap(hlc3))"),
            ("tr", "plot(ta.tr)\nplot(ta.tr(true))\nplot(ta.atr(14))"),
            (
                "highest",
                "plot(ta.highest(5))\nplot(ta.lowest(5))\nplot(ta.highest(close, 5))",
            ),
            (
                "bars",
                "plot(ta.highestbars(high, 5))\nplot(ta.lowestbars(5))",
            ),
            (
                "pivot",
                "plot(ta.pivothigh(3, 3))\nplot(ta.pivotlow(close, 3, 3))",
            ),
            (
                "change",
                "plot(ta.change(close))\nplot(ta.change(close, 3))\nplot(ta.mom(close, 4))\nplot(ta.roc(close, 4))",
            ),
            (
                "cross",
                "plot(ta.cross(close, open) ? 1 : 0)\nplot(ta.crossover(close, ta.sma(close, 5)) ? 1 : 0)",
            ),
            ("valuewhen", "plot(ta.valuewhen(close > open, close, 0))"),
            ("barssince", "plot(ta.barssince(close > open))"),
            (
                "misc-ta",
                "plot(ta.vwma(close, 5))\nplot(ta.cum(volume))\nplot(ta.stdev(close, 5))\nplot(ta.dev(close, 5))\nplot(ta.rsi(close, 14))",
            ),
            (
                "math",
                "plot(math.max(close, open, 1))\nplot(math.abs(close - open))\nplot(math.avg(high, low))\nplot(math.min(1, 2))",
            ),
            (
                "nz",
                "plot(nz(close[1]))\nplot(nz(close[1], 5))\nplot(na(close) ? 1 : 0)",
            ),
            (
                "na-compare",
                "x = close[1]\nplot(x == na ? 0 : 1)\nplot(x != na ? 0 : 1)",
            ),
            (
                "history-hoist",
                "plot(ta.sma(close, 3)[1])\nplot((high + low)[2])",
            ),
            (
                "compound",
                "var float t = 0.0\nt += close\nt -= 1\nt *= 2\nt /= 3\nt %= 4\nplot(t)",
            ),
            (
                "ternary-ta",
                "plot(close > open ? ta.sma(close, 5) : ta.ema(close, 5))",
            ),
            (
                "and-ta",
                "c = close > open and ta.rsi(close, 14) > 50\nplot(c ? 1 : 0)",
            ),
            (
                "if-ta",
                "var float x = na\nif close > open\n    x := ta.sma(close, 5)\nplot(x)",
            ),
            (
                "if-expr",
                "x = if close > open\n    1\nelse if close < open\n    -1\nelse\n    0\nplot(x)",
            ),
            (
                "switch-expr",
                "m = input.string(\"a\", \"M\", options = [\"a\", \"b\"])\nx = switch m\n    \"a\" => 1\n    \"b\" => 2\nplot(x)",
            ),
            (
                "switch-plain",
                "x = switch\n    close > open => 1\n    close < open => -1\n    => 0\nplot(x)",
            ),
            ("while", "n = 0\nwhile n < 3\n    n += 1\nplot(n)"),
            (
                "for-break",
                "s = 0.0\nfor i = 0 to 4\n    if i == 3\n        break\n    s += i\nplot(s)",
            ),
            (
                "one-line-fn",
                "f(a, b = 2) => a * b\nplot(f(close))\nplot(f(close, 3))\nplot(f(b = 4, a = open))",
            ),
            (
                "fn-ta",
                "avg(src, len) => ta.sma(src, len) + ta.ema(src, len)\nplot(avg(close, 5))\nplot(avg(open, 7))",
            ),
            (
                "fn-tuple",
                "pair(x) =>\n    a = x + 1\n    b = x - 1\n    [a, b]\n[u, d] = pair(close)\nplot(u)\nplot(d)",
            ),
            (
                "fn-history",
                "slope(src) => src - src[1]\nplot(slope(close))\nplot(slope(close * 2))",
            ),
            (
                "fn-in-if",
                "var float x = 0.0\navg(src, len) => ta.sma(src, len)\nif close > open\n    x := avg(close, 3)\nplot(x)",
            ),
            (
                "reserved-names",
                "state = close\nfn = open\nselect = high\ndefault = low\nplot(state + fn + select + default)",
            ),
            ("keyword-var", "in = 1\nreturn = 2\nplot(in + return)"),
            (
                "typed-decl",
                "int n = 3\nfloat f = 1.5\nbool b = true\nstring s = \"x\"\nseries float q = close\nplot(n + f + q)",
            ),
            (
                "barstate",
                "plot(barstate.isfirst ? 1 : 0)\nplot(barstate.islast ? 1 : 0)\nplot(barstate.isconfirmed ? 1 : 0)\nplot(barstate.isrealtime ? 1 : 0)",
            ),
            (
                "mintick",
                "plot(close + syminfo.mintick)\nplot(bar_index)\nplot(time > 0 ? 1 : 0)",
            ),
            ("derived", "plot(hl2 + hlc3 + ohlc4 + hlcc4)"),
            (
                "inputs",
                "a = input.int(5, \"A\", 1, 10, 1)\nb = input.float(2, \"B\", minval = 0.5, maxval = 9, step = 0.5, tooltip = \"t\", inline = \"i\", group = \"g\")\nc = input.bool(false, \"C\")\nd = input.string(\"x\", \"D\", [\"x\", \"y\"])\ne = input.source(hlc3, \"E\")\nf = input.color(color.red, \"F\")\ng = input.time(0, \"G\")\nh = input.price(1.0, \"H\")\ni = input(3, \"I\")\nj = input(1.5, \"J\")\nk = input(true, \"K\")\nl = input(\"s\", \"L\")\nm = input(close, \"M\")\nn = input(color.blue, \"N\")\nplot(a + b + e + h + i + j + m)",
            ),
            (
                "input-options-named",
                "s = input.string(defval = \"b\", title = \"S\", options = [\"a\", \"b\"], confirm = true)\nx = s == \"a\" ? 1 : 2\nplot(x)",
            ),
            (
                "colors",
                "plot(close, color = #FF000080)\nplot(open, color = color.new(#00FF00, 50))\nplot(high, color = color.rgb(1, 2, 3, 40))\nplot(low, color = color.aqua)\nc = close > open ? color.lime : color.maroon\nplot(hl2, color = color.new(c, 30))\nplot(ohlc4, color = color.rgb(close > open ? 255 : 0, 0, 0))",
            ),
            (
                "plot-styles",
                "plot(close, \"a\", style = plot.style_line)\nplot(open, \"b\", style = plot.style_linebr)\nplot(high, \"c\", style = plot.style_stepline)\nplot(low, \"d\", style = plot.style_histogram)\nplot(hl2, \"e\", style = plot.style_columns)\nplot(hlc3, \"f\", style = plot.style_area)\nplot(ohlc4, \"g\", style = plot.style_circles)\nplot(close, \"h\", offset = -1, linewidth = 3, trackprice = true, editable = false, join = true, show_last = 5, histbase = 0, display = display.all)",
            ),
            (
                "duplicate-titles",
                "plot(close, \"Same\")\nplot(open, \"Same\")\nplot(high)\nplot(low)",
            ),
            (
                "fill-hline",
                "h1 = hline(70)\nh2 = hline(30, \"Low\", color.green, hline.style_dotted, 2)\nfill(h1, h2, color = color.new(color.blue, 90))\nplot(close)",
            ),
            (
                "enum-bare",
                "//@version=6\nenum Kind\n    a\n    b = \"Bee\"\nk = input.enum(Kind.b, \"K\", group = \"g\")\nplot(k == Kind.a ? 1 : 2)",
            ),
            (
                "continuation",
                "x = ta.sma(close,\n     5) +\n     1\nplot(x)",
            ),
            (
                "tabs-and-comments",
                "if close > open // comment\n\tplot_me = 1\nplot(close) // trailing",
            ),
            ("strings", "plot(close, title = 'it\\'s \"quoted\"')"),
            (
                "dynamic-offset",
                "n = input.int(2, \"N\", minval = 1, maxval = 10)\nplot(close[n])\nplot(ta.change(close, n))",
            ),
            (
                "length-expr",
                "n = input.int(10, \"N\", minval = 2)\nplot(ta.sma(close, n * 2))\nplot(ta.sma(close, n + 1))",
            ),
            (
                "macd-cross",
                "[m, s, h] = ta.macd(close, 12, 26, 9)\nplot(ta.crossover(m, s) ? 1 : 0)\nplot(h > h[1] ? 1 : 0)\nplot(ta.change(h))",
            ),
            (
                "reassigned-history",
                "x = 0.0\nx := nz(x[1]) + close\nplot(x)",
            ),
            (
                "var-history",
                "var float s = 0.0\ns += close\nplot(ta.change(s))\nplot(s[3])\nplot(ta.crossover(s, 5) ? 1 : 0)",
            ),
            ("indicator-named", "plot(close)"),
        ];
        for (name, body) in snippets {
            let head = if body.starts_with("//@version=6") {
                String::new()
            } else {
                "//@version=5\n".to_owned()
            };
            let overlay = if *name == "indicator-named" {
                "indicator(title = \"Named  \u{2014} Overlay!\", overlay = true, precision = 3)\n"
            } else {
                "indicator(\"T\", overlay = true)\n"
            };
            let source = format!("{head}{overlay}{body}\n");
            let out = transpile(
                &source,
                &Options {
                    id: Some(format!("pine.{name}")),
                },
            );
            dump(name, &out);
            assert!(
                out.unsupported.is_empty(),
                "{name}: unexpected unsupported items {:?}\n{}",
                out.unsupported,
                out.source
            );
            compiled(&out, name);
        }
    }

    #[test]
    fn header_options() {
        let out = translate(
            "//@version=5\nindicator(\"My Cool Script!\", \"MCS\", false, precision = 3)\nplot(close)\n",
        );
        assert_eq!(out.id, "pine.my-cool-script");
        assert!(out.source.contains("attachment = \"panel\""));
        assert!(out.source.contains("fixed_step:0.001"));
        let out = transpile(
            "//@version=5\nindicator(\"X\")\nplot(close)\n",
            &Options {
                id: Some("ns.custom".to_owned()),
            },
        );
        assert_eq!(out.id, "ns.custom");
        assert!(out.source.contains("id = \"ns.custom\""));
    }

    #[test]
    fn non_ascii_title_gets_a_valid_id() {
        let out = translate(
            "//@version=5\nindicator(\"\u{6307}\u{6a19}\")\nplot(close) // \u{4e2d}\u{6587}\n",
        );
        assert_eq!(out.id, "pine.script");
        compiled(&out, "script");
    }

    #[test]
    fn unsupported_constructs_are_reported_not_dropped() {
        let source = "//@version=5\nindicator(\"U\", overlay = true)\n\
            x = request.security(syminfo.tickerid, \"D\", close)\n\
            y = ta.wma(close, 5)\n\
            plotshape(close > open)\n\
            bgcolor(color.red)\n\
            varip int z = 0\n\
            plot(x)\n\
            plot(close)\n";
        let out = translate(source);
        let lines = out.unsupported.iter().map(|u| u.line).collect::<Vec<_>>();
        assert_eq!(lines, vec![3, 4, 5, 6, 7, 8]);
        assert_eq!(out.source.matches("// UNSUPPORTED:").count(), 6);
        assert!(out.source.contains("// UNSUPPORTED: x = request.security"));
        assert!(out.source.contains("plot(chart.close, id = \"plot\""));
        // The surviving output still compiles: dependents of a failed line are reported too.
        compiled(&out, "u");
    }

    #[test]
    fn history_of_a_reassigned_variable_reads_the_previous_bar_end() {
        let out =
            translate("//@version=5\nindicator(\"H\")\nx = 0.0\nx := nz(x[1]) + close\nplot(x)\n");
        assert!(out.unsupported.is_empty());
        assert!(out.source.contains("var x_prev_1 = na"), "{}", out.source);
        assert!(
            out.source.contains("x := nz(x_prev_1, 0) + chart.close"),
            "{}",
            out.source
        );
        assert!(out.source.contains("x_prev_1 := x"), "{}", out.source);
    }

    #[test]
    fn a_failed_block_header_comments_out_the_whole_block() {
        let out = translate(
            "//@version=5\nindicator(\"B\")\nx = 0.0\nif ta.wma(close, 5) > 0\n    x := 1\nelse\n    x := 2\nplot(x)\nfor i = 3 to 1\n    x := 3\n",
        );
        assert_eq!(out.unsupported.len(), 2, "{:?}", out.unsupported);
        let commented = out
            .source
            .lines()
            .filter(|l| l.starts_with("// UNSUPPORTED:"))
            .collect::<Vec<_>>();
        assert_eq!(
            commented,
            vec![
                "// UNSUPPORTED: if ta.wma(close, 5) > 0",
                "// UNSUPPORTED:     x := 1",
                "// UNSUPPORTED: else",
                "// UNSUPPORTED:     x := 2",
                "// UNSUPPORTED: for i = 3 to 1",
                "// UNSUPPORTED:     x := 3",
            ]
        );
        compiled(&out, "b");
    }

    #[test]
    fn failed_statements_leave_no_helper_bindings_behind() {
        let out = translate(
            "//@version=5\nindicator(\"R\")\nx = ta.stoch(close, high, low, 14) + ta.wma(close, 3)\nplot(close)\n",
        );
        assert_eq!(out.unsupported.len(), 1);
        assert!(!out.source.contains("tmp_"), "{}", out.source);
    }

    #[test]
    fn versions_are_checked() {
        let out = translate("//@version=4\nstudy(\"Old\")\nplot(close)\n");
        assert!(
            out.unsupported
                .iter()
                .any(|u| u.reason.contains("version 4"))
        );
        let out = translate("indicator(\"None\")\nplot(close)\n");
        assert!(out.unsupported.iter().any(|u| u.reason.contains("missing")));
        let out = translate("//@version=6\nindicator(\"Six\")\nplot(close)\n");
        assert!(out.unsupported.is_empty());
        let out = translate("//@version=5\nstrategy(\"S\")\nplot(close)\n");
        assert_eq!(out.unsupported.len(), 1);
    }

    #[test]
    fn v6_declarations_and_types_are_reported() {
        let out = translate(
            "//@version=6\nindicator(\"V6\")\ntype Point\n    float x\nmethod m(float a) => a\nexport f() => 1\nimport a/b/1 as c\nplot(close)\n",
        );
        assert_eq!(out.unsupported.len(), 4, "{:?}", out.unsupported);
    }

    #[test]
    fn division_in_a_window_length_is_reported() {
        let out = translate(
            "//@version=5\nindicator(\"D\")\nn = input.int(10, \"N\")\nplot(ta.sma(close, n / 2))\nplot(close)\n",
        );
        assert_eq!(out.unsupported.len(), 1);
    }

    #[test]
    fn hostile_input_does_not_panic() {
        let deep = format!("x = {}1{}\n", "(".repeat(10_000), ")".repeat(10_000));
        let long = format!("x = {}\n", vec!["1"; 5_000].join(" + "));
        let nested = (0..200)
            .map(|i| format!("{}if true\n", "    ".repeat(i)))
            .collect::<String>();
        let chain = format!(
            "if a\n    x = 1\n{}else\n    x = 3\n",
            "else if b\n    x = 2\n".repeat(20_000)
        );
        let value_chain = format!(
            "x = if a\n    1\n{}else\n    3\n",
            "else if b\n    2\n".repeat(20_000)
        );
        let recursive = "f(x) => f(x)\ny = f(1)\ng(x) =>\n    h = g(x)\n    h\nz = g(2)\n";
        let cases = [
            deep.as_str(),
            long.as_str(),
            nested.as_str(),
            recursive,
            chain.as_str(),
            value_chain.as_str(),
            "if\nfor\nwhile\nswitch\n=>\n",
            "x = (\n",
            "x = )\n",
            "x = \"unterminated\n",
            "plot(",
            "plot(close,,)",
            "[a, b = 1",
            "x = #12\n",
            "x = 1 ? 2\n",
            "a.b.c = 1\n",
            "x = ta.sma()\n",
            "x = ta.sma(close)\n",
            "f(a, ) => a\n",
            "x[1] = 3\n",
            "break\n",
            "\u{feff}\u{200b} \t \u{0}\n",
            "else\n    x = 1\n",
            "for i = 10 to 1\n    x = 1\n",
            "for i = 1 to 3 by 2\n    x = 1\n",
            "x = input.int()\n",
            "enum E\n    1\n",
            "x = switch\n    a\n",
            "x = if a\n",
            "",
        ];
        for source in cases {
            let out = translate(source);
            assert!(out.source.starts_with("language(1)"));
        }
    }

    #[test]
    fn cli_smoke_path_accepts_translated_files() {
        // The sources compile through the same entry point the CLI uses; the directory-level
        // `flowsurface-indicator test` run is exercised by the verification step.
        for (name, source) in [
            (
                "ma-crossover",
                include_str!("../tests/fixtures/ma-crossover.pine"),
            ),
            ("for-loop", include_str!("../tests/fixtures/for-loop.pine")),
        ] {
            let out = transpile(
                source,
                &Options {
                    id: Some(format!("pine.{name}")),
                },
            );
            let compiled = indicator_engine::compile(&out.source, format!("{name}.ind"))
                .unwrap_or_else(|_| panic!("{name} must compile"));
            assert_eq!(compiled.descriptor.id.as_str(), out.id);
        }
    }
}
