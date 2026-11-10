# flowsurface-pine

Translates a subset of Pine Script v5/v6 into Flowsurface `.ind` indicator sources.
The output is plain `.ind` text; check it with `flowsurface-indicator check` from `indicator-engine/`.

## Usage

```bash
flowsurface-pine <input.pine> [-o out.ind] [--id namespace.name]
```

- Without `-o`, the generated source is printed to stdout.
- `--id` sets the indicator ID. Without it, the ID is `pine.<slugified title>`, taken from the script's `indicator(...)` / `strategy(...)` title.
- The output file name must be the name segment of the ID plus `.ind`. For example, `--id ns.atr-band` requires `atr-band.ind`. A mismatch prints a warning but still writes the file.

## Diagnostics

Anything the translator cannot handle is kept in the output as a `// UNSUPPORTED:` comment and printed to stderr:

```
line 12: <reason>
```

Lossy translations are printed as notes:

```
note: line 12: <reason>
```

The exit code is 0 for both. Read stderr before using the output.

## Supported input

Pine Script v5 and v6 are both accepted. Coverage is partial: user-defined functions are inlined, stateful `ta.*` calls are hoisted out of conditional branches, common `ta` helpers are expanded, and enum-style inputs are mapped to string inputs. Constructs outside this subset are reported as unsupported rather than silently dropped.

## Tests

```bash
cargo test -p flowsurface-pine --locked
```

Fixtures live in `pine/tests/fixtures/`.

## Indicator editor: From Pine

The indicator editor's **From Pine** button uses the same translator. Pine v5 or v6 source pasted there is converted into a new, unsaved `.ind` source. Unsupported lines are shown as errors and lossy translations as warnings; both are labelled with the Pine line number (`Pine line N: ...`). Save and Check are disabled while the Pine input is shown.
