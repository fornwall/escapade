# Agent status

Who built what, and what is actually finished. Written by the coordinating agent
from **observed repository state** — test counts from `cargo test`, data-file sizes
from disk — rather than from claims. Where an agent never reported, that is said
explicitly.

Fourteen features were built in parallel by separate agents. Ten agents were
terminated mid-work by an API session limit at one point and several more by
transient server errors; most were resumed and finished, some were not.

## Repository state

| check | result |
| --- | --- |
| `cargo test` | **1,616 passing, 0 failing** |
| `cargo check` | clean |
| `cargo clippy --all-targets` | **clean** |

## Complete

Implementation, data file and tests all present. The data-file column is its line
count; a placeholder is 11 lines, so these are all real.

| feature | tests | data file | notes |
| --- | --- | --- | --- |
| `glyph_protocol` | 66 | 298 | The Glyph Protocol. Hand-written `glyf` encoder with a byte-level walkthrough, braille outline visualiser, 12-step demo ending in a live glyph editor and a playable game. The **reference implementation of the probe contract**. |
| `kitty_graphics` | 228 | 592 | Kitty graphics. `render` for PNG/JPEG/GIF/WebP/SVG/DOT/flame graphs, three-stage input sniffer, six procedural shapes as demo steps, and delivery method (`t=f`/`t=t`/`o=z`/`t=d`) chosen from *probed* capability rather than a flag. |
| `grapheme` | 119 | 435 | Mode 2027. Cursor-position measurement is the primitive; the mode query only a hint. Sentinel probe against an impossible mode, whose reliability finding is load-bearing — without it the demo could leave clustering on for whatever ran next. |
| `doctor` | 193 | `data/checks.toml` | Diffs terminfo / XTGETTCAP / observed behaviour / environment across nine capabilities and reports disagreements. Verified against a pty harness impersonating four terminals including a deliberate liar. Found that an invalid name ends processing of an entire XTGETTCAP list, so it sends one name per query. |
| `style` | 153 | 506 | The whole SGR family, generalised from an earlier `underline`. 24-bit colour where the colour *form* decides what `probe` measures; `NO_COLOR` handled by presence-and-non-emptiness; every verdict states its evidence, keeping capability separate from preference. |
| `notification` | 90 | 457 | OSC 9 / 777 / 99. `Auto` picks the richest protocol the probe believes in and says which and why. `Safe` is the only text constructor, so hostile input cannot reach a sequence by accident. |
| `progress` | 78 | 412 | OSC 9;4. Found that **`BEL` termination does not work** — VTE rejects it for this sequence while Microsoft's own tutorial uses it — and that out-of-range values diverge dangerously, so it clamps on the sending side. |
| `decode` | 164 | n/a | Bytes back into named sequences. No data file by design; it is a tool, not a documented feature. **Its agent has not reported**, so the 164 tests are observed rather than attested. |
| `dimensions` | 58 | 350 | Size in cells and pixels, with `probe --rows-at-least`-style thresholds. **Its agent never reported** — killed by the session limit — so completeness is inferred from the tests and data file. |
| `hyperlink` | 56 | 476 | OSC 8. Percent-encodes URIs but *rejects* malformed `id`s, since an id's only job is to compare byte-equal against ids from other programs. Visible text gets Control Pictures stand-ins. |
| `unscroll` | 31 | 296 | `CSI Ps + T`. **Disproved this project's own `TODO.md`** about the pager use case — see Corrections there. Found a third implementation (contour) that kitty's own page does not list. |
| `clipboard` | 48 | 407 | OSC 52. The **only probe here whose measurement is free**: the clipboard extension puts `52` in the device-attributes reply, so the barrier every probe already sends carries the answer — and a conforming terminal must withdraw it when the user disables clipboard access, so it describes the session rather than a version. Reading is deliberately never probed: on Konsole the query *destroys* the clipboard, which `paste` refuses without `--force`. First module to reuse `doctor/identity.rs` instead of copying its table. |
| `color` | 116 | 391 | OSC 4/10/11/12/17/19 and DEC mode 2031. The ~2,800 lines that were unreachable behind a `bail!` are now wired, plus the three modules that were missing: `notify` (the subscription), `probe` and `demo`. The **only probe here that measures rather than infers** — a colour query has a reply, so no support table is consulted. Compiling `report.rs` for the first time turned up a test that had never run and was wrong: pure red is *light*, at luminance 0.2126 against an L\* = 50 threshold of 0.1842. |

## Implementation done, data file still a placeholder

The code and tests are there; `explain` will show almost nothing until the TOML is
written.

| feature | tests | data file | what is missing |
| --- | --- | --- | --- |
| `keys` | 68 | **11 lines** | The kitty keyboard protocol data file. Its agent was writing it — "the biggest piece of prose" — when the session limit hit, and never reported. Also unverified: whether the reusable **interactive-input construction** other demos were meant to adopt actually exists. |
| `textsize` | 59 | **11 lines** | The text-sizing data file, including the per-key support picture (foot implements `w=` but not `s=`) that the whole feature's verdict model depends on. Its agent never reported. |

## Not finished

Nothing. Every subcommand in `--help` now does something; `clipboard`, the last
`bail!("not implemented yet")`, is in the table above.

## Dropped

`capture` (`CSI 10 i` / `CSI 11 i` screen dumps) was implemented — 56 tests, an
honest best-effort file harvest — and then **removed at the user's decision**: only
xterm supports both forms and mintty only HTML, which is too narrow a base for a
subcommand. Its findings are preserved under Corrections in `TODO.md`, because they
cost real source-reading.

## What to do next, in order

1. **Write the `keys` and `textsize` data files.** Both features work; both
   `explain` pages are empty. This is now the only unfinished work of its kind.
2. **Settle the five `unknown` rows in `clipboard`'s matrix** — VS Code, GNU
   Screen, ConEmu, Warp and wayst. Each says "not checked" rather than guessing;
   GNU Screen's source could not be reached at all while the file was written.
3. **Verify `color`'s support matrix.** Ten rows are `unknown` and say so, and
   two `yes` rows (VTE, Konsole) cite a repository rather than a specific file.
   Checking those against source is exactly the contribution `CONTRIBUTING.md`
   asks for and needs no Rust.
4. `TODO.md`'s "Known issues" — the top one is now that `escapade color`'s
   one-shot report never asks for the colour scheme, so `--is-dark` decides from
   luminance even where the terminal would have answered outright.

## Shared code

`src/decrqm.rs` (18 tests) is the DECRQM protocol — the five DECRPM states, the
impossible-mode sentinel that iTerm2 fails, and the report parser. It replaces
two near-identical copies that had been carried in `doctor` and `grapheme` and
had already drifted: one parsed DECRPM values as `u16`, so a terminal answering
anything larger produced no report at all rather than an undefined one. Each
feature keeps its own *interpretation* — what a reset mode 2027 means to a
program measuring text is `grapheme`'s business, not the protocol's.

## A note on what these agents were good at

The most valuable thing they produced was not code but **corrections**. Three
separate agents disproved something this project had written down: that the DECRQSS
success flag can be trusted (it cannot — DEC's own manuals had it reversed), that
`unscroll` fixes the pager case (it does not), and that xterm resources gate the
screen dumps (no resource does). A fourth disproved something *tmux's own manual
page* implies — that `set-clipboard external` forwards an application's OSC 52. It
does not: `input_osc_52_parse` returns immediately unless the option is `on`, so
under the default nothing an application sends is looked at at all. Two also caught real bugs in the shared code —
`explain.rs` advertising a `demo` verb for features that have none, and `main.rs`
silently dropping `--no-color`.

Where they were weakest was **self-verification under churn**: several reported
test counts from isolated copies rather than the real tree, and one silently
reverted a CLI fix in a file it had been told not to touch.
