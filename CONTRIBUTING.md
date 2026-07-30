# Contributing

The most valuable contribution to this project is a corrected support matrix, and
it needs no Rust at all.

## Fixing a support matrix

Every `explain` page ends by naming the file it came from. Open
`data/features/<id>.toml`, find the `[[support]]` block for the terminal, and fix
it.

```toml
[[support]]
terminal = "ghostty"      # must be an id from data/terminals.toml
status = "yes"            # yes | partial | flag | passthrough | no | unknown
since = "1.2.0"           # optional: version or date support arrived
notes = "..."             # optional: caveats, gaps, configuration needed
source = "https://..."    # please include one
```

`status` values mean:

| value | meaning |
| --- | --- |
| `yes` | implemented and on by default |
| `partial` | implemented with meaningful gaps — say which in `notes` |
| `flag` | implemented but off until the user opts in |
| `passthrough` | multiplexer: forwards it only when configured to |
| `no` | no known implementation |
| `unknown` | nobody has checked |

`no` and `unknown` are different, and the difference matters: `no` is a claim,
`unknown` is an admission. Please do not upgrade `unknown` to `no` without
actually checking.

If the terminal is not in `data/terminals.toml`, append a `[[terminal]]` block.
Ids are lowercase kebab-case, and a typo fails at startup rather than silently
dropping the row.

## Editing prose without rebuilding

The data files are baked into the binary, but an overlay directory wins over the
embedded copies:

```console
$ ESCAPADE_DATA_DIR=./data cargo run -- glyph explain
```

Edit, save, re-run.

## House rules for data files

* Use TOML **literal** strings (`'''…'''`) for prose, so backslashes in escape
  sequences survive verbatim.
* Write sequences symbolically — `ESC`, `CSI`, `OSC`, `APC`, `DCS`, `ST`, `BEL` —
  and **never embed a literal control byte**. `escapade` prints these as text, so
  a real `0x1b` in a data file would be executed by the reader's terminal. A test
  enforces this.
* Do not paste invisible characters either. A Private Use Area codepoint in the
  prose renders as tofu for exactly the readers being told about tofu; write
  `U+E0A0` instead. A test enforces this too.
* Prose is markdown: inline code, bold, bullet lists and fenced blocks all
  render.

## Adding a feature

1. Write `data/features/<id>.toml`, modelled on `glyph-protocol.toml`.
2. Add one line to `EMBEDDED_FEATURES` in `src/db.rs`.
3. Add a subcommand in `src/cli.rs` and a module under `src/features/`.
4. Map the feature id to the subcommand name in `subcommand_for`, in
   `src/features/mod.rs`, if they differ.

A feature module exports:

```rust
pub const FEATURE_ID: &str = "<id>";
pub fn run(catalog: &Catalog, command: &…, no_color: bool) -> anyhow::Result<()>;
```

`explain` is not yours to write — call `crate::features::explain::render`, which
produces the whole page from the database. That is what keeps the features from
drifting apart in presentation.

## Code conventions

* Build escape sequences directly. This project's code is meant to be readable as
  a reference for the protocol, so a wrapper library would defeat the purpose.
* Doc comments on protocol functions should name the sequence:
  `/// Enables mouse reporting: `CSI ? 1000 h`.`
* Pair every capability query with a `CSI c` barrier, and distinguish *no* from
  *no answer*. See `src/features/glyph_protocol/probe.rs`.
* Never print a codepoint without checking it is printable first — see
  `glyph_protocol/supports.rs::rendered`. Emitting a raw `ESC` would run whatever
  followed it.
* Anything pure gets a unit test. No test may require a terminal.
* `unsafe_code` is forbidden. `clippy::all` and `clippy::pedantic` are warnings,
  and `cargo clippy --all-targets` should be clean.
