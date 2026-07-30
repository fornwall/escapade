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
| `cargo test` | **1,450 passing, 0 failing** |
| `cargo check` | clean |
| `cargo clippy --all-targets` | 2 warnings remaining |

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

## Implementation done, data file still a placeholder

The code and tests are there; `explain` will show almost nothing until the TOML is
written.

| feature | tests | data file | what is missing |
| --- | --- | --- | --- |
| `keys` | 68 | **11 lines** | The kitty keyboard protocol data file. Its agent was writing it — "the biggest piece of prose" — when the session limit hit, and never reported. Also unverified: whether the reusable **interactive-input construction** other demos were meant to adopt actually exists. |
| `textsize` | 59 | **11 lines** | The text-sizing data file, including the per-key support picture (foot implements `w=` but not `s=`) that the whole feature's verdict model depends on. Its agent never reported. |

## Not finished

| feature | state |
| --- | --- |
| `color` | ~2,800 lines across 5 files, but **`mod.rs` is still the placeholder that `bail!`s**, so `escapade color` does not run and none of that code is reachable. Data file is a placeholder. Its agent was verifying support rows for tmux, Alacritty, foot and Windows Terminal when it was killed. Wiring `mod.rs` is likely a short job with a large payoff — the same situation `style` was in before it was resumed. |
| `clipboard` | **Not started.** 21-line stub only; its agent was killed before writing anything. OSC 52 was independently identified as the most reimplemented escape sequence in the field and this project's most conspicuous gap, so it is the highest-value remaining work. |

## Dropped

`capture` (`CSI 10 i` / `CSI 11 i` screen dumps) was implemented — 56 tests, an
honest best-effort file harvest — and then **removed at the user's decision**: only
xterm supports both forms and mintty only HTML, which is too narrow a base for a
subcommand. Its findings are preserved under Corrections in `TODO.md`, because they
cost real source-reading.

## What to do next, in order

1. **Wire `color`'s `mod.rs`.** One file stands between ~2,800 lines of finished
   work and a working command.
2. **Write the `keys` and `textsize` data files.** Both features work; both
   `explain` pages are empty.
3. **Implement `clipboard`.** The largest genuine gap.
4. Clear the last 2 clippy warnings.
5. `TODO.md`'s "Known issues" — starting with the fact that **nothing is committed
   to git**, so none of this is recoverable if an edit goes wrong. This has already
   cost one retyped block of `cli.rs` and one silently reverted CLI fix.

## A note on what these agents were good at

The most valuable thing they produced was not code but **corrections**. Three
separate agents disproved something this project had written down: that the DECRQSS
success flag can be trusted (it cannot — DEC's own manuals had it reversed), that
`unscroll` fixes the pager case (it does not), and that xterm resources gate the
screen dumps (no resource does). Two also caught real bugs in the shared code —
`explain.rs` advertising a `demo` verb for features that have none, and `main.rs`
silently dropping `--no-color`.

Where they were weakest was **self-verification under churn**: several reported
test counts from isolated copies rather than the real tree, and one silently
reverted a CLI fix in a file it had been told not to touch.
