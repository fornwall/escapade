# Working in this repo

`escapade` documents, probes and demonstrates modern terminal escape-sequence
features. Every feature has a TOML file in `data/features/` and a module in
`src/features/`, and exposes the same verbs: `explain`, `probe` and — where it
makes sense — `demo`.

This file is for someone **changing code**. `CONTRIBUTING.md` is for someone
fixing a support matrix, which needs no Rust at all; `README.md` is for someone
using the tool. `CLAUDE.md` is a symlink to this file.

## Build, test, lint

```console
$ cargo build
$ cargo test                   # everything pure; no test may require a terminal
$ cargo clippy --all-targets   # must be clean, not merely compiling
$ cargo fmt
$ cargo run -- <subcommand>
```

The toolchain is pinned in `rust-toolchain.toml`, and the same version is the
MSRV in `Cargo.toml` and `clippy.toml`. A change is not done until
`cargo clippy --all-targets` is clean.

Data files are embedded with `include_str!`, but an overlay directory wins over
the embedded copies at runtime, so prose edits need no rebuild:

```console
$ ESCAPADE_DATA_DIR=./data cargo run -- glyph explain
```

## Layout

| path | what it is |
| --- | --- |
| `src/main.rs` | Dispatch, panic hook, process exit status. |
| `src/cli.rs` | The whole clap tree. |
| `src/db.rs` | The feature database: loads `data/`, embeds it, rejects unknown terminal ids at startup. |
| `src/term.rs` | Raw mode, alternate screen, private modes, multiplexer detection, `pretty`. |
| `src/input.rs` | Parses what the terminal *sends* — keys, mouse reports, replies. `InputReader::drain` and `wait_for_reply` live here. |
| `src/decrqm.rs` | Shared DECRQM/DECRPM protocol: the five DECRPM states, the impossible-mode sentinel, the report parser. Features keep their own *interpretation* of a mode; the protocol is not theirs to copy. |
| `src/theme.rs` | Colour and markdown rendering. |
| `src/unicode.rs` | Codepoint names, categories, widths, grapheme clustering. |
| `src/features/` | One module per feature. `explain.rs` is the shared renderer; `mod.rs` holds `Support`, the operand helpers and `subcommand_for`. |
| `data/features/*.toml` | One file per feature — the prose and the support matrix. |
| `data/terminals.toml` | The terminal registry every support row's id must exist in. |
| `data/checks.toml` | `doctor`'s check list. |

Note that `src/input.rs` parses the grammar a *terminal* emits, while
`src/features/decode/parse.rs` parses the grammar an *application* emits. They
are different grammars; do not merge them.

## Adding a feature

`CONTRIBUTING.md` has the four registration steps and the module signature. What
matters beyond them is that a feature module owns only its own behaviour:

* **`explain` is not yours to write.** Call `crate::features::explain::render`,
  which builds the whole page from the database. This is what keeps features from
  drifting apart in presentation, and it means adding a feature means writing
  TOML rather than writing a renderer.
* **`probe` is a predicate.** Silent unless `--format json|debug`; the answer is
  the exit status, via `features::Support` — 0 yes, 1 no, 2 unknown. `Unknown`
  must never collapse into `No`. `src/features/glyph_protocol/probe.rs` is the
  reference implementation.
* **`demo` is optional.** Not every feature has one, and `explain` must not claim
  otherwise.

## Protocol conventions

These are load-bearing. Breaking one produces a tool that lies about terminals,
which is worse than a tool that says nothing.

* **Pair every capability query with a Primary Device Attributes barrier**
  (`CSI c`), so silence is distinguishable from slowness. But a **multiplexer
  answers that barrier itself**, so inside one a barrier reply licenses no
  negative verdict — see `features/dimensions/probe.rs` and
  `features/glyph_protocol/probe.rs::ProbeOutcome::verdict`.
* **Drain every reply you provoke.** An undrained reply is delivered to the
  user's shell and printed as garbage.
* **Shield every query a terminal might print.** The mirror of the drain rule, on
  the output side. A terminal with no state for an APC, DCS or OSC control string
  can print its body instead of discarding it, so wrap the query in
  `crate::term::shielded` — see `src/term.rs`. `CSI`-only queries need no shield:
  the control-sequence grammar is universal, so an unknown final byte is still
  consumed.
* **Distinguish *no* from *no answer*.** Most terminals implement almost none of
  these queries; reading silence as a denial is the standard way to get this
  wrong.
* **Build escape sequences directly.** This code is meant to be readable as a
  reference for the protocol, so a wrapper library would defeat the purpose.
* **Name the sequence in the doc comment** of any protocol function:
  ``/// Enables mouse reporting: `CSI ? 1000 h`.``
* **Never print a codepoint without checking it is printable** — see
  `glyph_protocol/supports.rs::rendered`. Emitting a raw `ESC` would run whatever
  followed it.
* **Text operands come from `features::text_operand` / `bytes_operand`**, which
  implement the arguments-or-stdin convention and report the hang case rather
  than blocking on it.

## Code conventions

* `unsafe_code` is forbidden. `clippy::all` and `clippy::pedantic` are warnings;
  `unwrap_used` and `expect_used` are warnings outside tests.
* Anything pure gets a unit test, in a `#[cfg(test)]` module in the same file.
* Hostile input reaches a sequence only through a constructor that sanitizes it.
  `notification`'s `Safe` is the pattern: make the safe constructor the only one.

## Data files

`CONTRIBUTING.md` has the house rules — TOML literal strings, symbolic sequence
names, no literal control bytes, no invisible characters. Tests enforce the last
two. One rule that is easy to miss and not visible until it renders:

* **One source line per paragraph.** termimad re-wraps each source line
  independently, so pre-wrapped prose wraps twice and reads ragged.

## Findings already established

Counterintuitive things that cost real source-reading, with where the reasoning
lives. Do not re-derive these, and do not "fix" the code that looks wrong because
of them.

| finding | recorded in |
| --- | --- |
| The DECRQSS valid/invalid status flag cannot be trusted — DEC's own manuals had it reversed, and a real VT420 confirmed the documented codes were the wrong way round. Branch on the reply's *content*. | `src/features/doctor/observe.rs` (`SgrReadback::advisory_flag`) |
| A multiplexer answers the device-attributes barrier — and DECRQSS — from its own emulator, so neither says anything about the terminal underneath. | `src/features/dimensions/probe.rs`, `src/features/style/probe.rs` |
| An invalid XTGETTCAP name ends processing of the whole list, so a batch silently loses everything after it. `doctor` sends one name per query. | `src/features/doctor/tcap.rs` |
| iTerm2 answers DECRQM for modes that do not exist, so every DECRQM run first asks about an impossible sentinel mode. | `src/decrqm.rs` |
| `OSC 9;4` progress must end with `ST`: VTE deliberately rejects `BEL` for this one sequence, while Microsoft's own tutorial uses it. | `src/features/progress/sequence.rs` |
| tmux's `set-clipboard external` does **not** forward an application's OSC 52, despite what the manual implies — `input_osc_52_parse` returns immediately unless the value is `on`. | `data/features/clipboard.toml` |
| Asking Konsole to *read* the clipboard destroys it: the query falls through to the write path and sets it empty. `paste` refuses there without `--force`. | `src/features/clipboard/probe.rs`, `data/features/clipboard.toml` |
| Terminals advertise OSC 52 support by putting `52` in the Primary Device Attributes reply, and must withdraw it when the user disables clipboard access — so the barrier already carries the answer. Its *absence* still proves nothing. | `src/features/clipboard/probe.rs` |
| Unscroll does not recover a pager's screen: on the alternate screen it degrades to `SD` and inserts blanks, and alternate-screen output never enters scrollback at all. | `src/features/unscroll/mod.rs`, `data/features/unscroll.toml` |
| macOS Terminal.app consumes only the two-byte `ESC _` introducer of an APC string and prints the body as text, rather than discarding the string as ECMA-48 requires. Queries are shielded with autowrap off so a leak cannot wrap, and therefore cannot scroll — which is the case cursor-restore cannot undo. `nsterm` has no `ech`, so the erase is `CSI K`. | `src/term.rs` (`shielded`) |
