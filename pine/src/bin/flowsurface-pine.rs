//! `flowsurface-pine <input.pine> [-o out.ind] [--id ns.name]`

use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use flowsurface_pine::{Options, transpile};

const USAGE: &str = "usage: flowsurface-pine <input.pine> [-o out.ind] [--id namespace.name]";

fn run() -> Result<(), String> {
    let mut input = None;
    let mut output = None;
    let mut id = None;
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "-o" | "--output" => {
                output = Some(
                    arguments
                        .next()
                        .ok_or_else(|| format!("-o needs a path\n{USAGE}"))?,
                );
            }
            "--id" => {
                id = Some(
                    arguments
                        .next()
                        .ok_or_else(|| format!("--id needs a value\n{USAGE}"))?,
                );
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unknown argument '{flag}'\n{USAGE}"));
            }
            _ => {
                if input.replace(argument).is_some() {
                    return Err(format!("expected exactly one input file\n{USAGE}"));
                }
            }
        }
    }
    let input = input.ok_or_else(|| format!("missing input file\n{USAGE}"))?;
    let source =
        fs::read_to_string(&input).map_err(|error| format!("could not read {input}: {error}"))?;
    let result = transpile(&source, &Options { id });
    for item in &result.unsupported {
        eprintln!("line {}: {}", item.line, item.reason);
    }
    for item in &result.notes {
        eprintln!("note: line {}: {}", item.line, item.reason);
    }
    match output {
        Some(path) => {
            let expected = result
                .id
                .rsplit('.')
                .next()
                .map(|name| format!("{name}.ind"))
                .unwrap_or_default();
            let actual = Path::new(&path).file_name().and_then(|n| n.to_str());
            if actual != Some(expected.as_str()) {
                eprintln!(
                    "warning: indicator ID '{}' requires the file name '{expected}'",
                    result.id
                );
            }
            fs::write(&path, &result.source)
                .map_err(|error| format!("could not write {path}: {error}"))
        }
        None => {
            print!("{}", result.source);
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(2)
        }
    }
}
