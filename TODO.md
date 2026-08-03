# TODO

## Unimplemented subcommands

**None.** `unscroll`, `decode`, `color` and `clipboard` have all been
implemented, `capture` was implemented then dropped, and **two of those disproved
what this file claimed about them** — see "Corrections" below. No subcommand
returns `bail!("… is not implemented yet")` any more.

`clipboard` (OSC 52) turned out to have one thing this file did not know about,
and it changes what a probe can do: terminal authors have agreed on a **clipboard
extension** that puts `52` among the parameters of the Primary Device Attributes
reply, and requires a terminal to withdraw it when the user disables clipboard
access. So the barrier every probe here already sends doubles as the capability
question, and the answer describes the session rather than a version number. See
`features/clipboard/probe.rs` and the spec at
<https://github.com/contour-terminal/vt-extensions/blob/master/clipboard-extension.md>.

Anything built later should follow the conventions the built features share, none of which are
written down anywhere but the code:

* `explain` calls `crate::features::explain::render` — features never write their
  own renderer.
* `probe` is a **predicate**: silent unless `--format json|debug`, answering
  through the exit status via `features::Support` (0 yes, 1 no, 2 unknown).
  `Unknown` must never collapse into `No`.
* Any capability query is paired with a Primary Device Attributes barrier
  (`CSI c`) so silence is distinguishable from slowness — and a multiplexer
  answers that barrier *itself*, so inside one a barrier reply licenses no
  negative verdict. See `features/glyph_protocol/probe.rs::ProbeOutcome::verdict`.
* Every reply you provoke must be **drained**, or it is delivered to the user's
  shell and printed as garbage.
* Text operands come from arguments or stdin via `features::text_operand` /
  `bytes_operand`.
* Data-file prose is **one source line per paragraph** — termimad re-wraps each
  source line independently, so pre-wrapped prose wraps twice and reads ragged.
  No literal control bytes and no invisible characters; tests enforce both.

### `escapade decode` — bytes back into named sequences — **built**

Kept as written, because it is the specification the implementation was built
from and the reasoning is worth seeing next to the result.

The inverse of the `demo` verb. Instead of showing a sequence and then its effect,
take an effect you already have — a byte stream that rendered wrong — and name the
sequences in it.

```console
$ printf '\e[1;31mred\e[0m' | escapade decode
CSI 1;31 m    SGR — bold, foreground red
"red"
CSI 0 m       SGR — reset all attributes
```

Three use cases, in order of value:

1. **Debugging your own output.** A TUI draws a corrupted border; capture its
   bytes and ask what it actually emitted, rather than squinting at `cat -v`.
2. **Auditing untrusted text** before printing it — log lines, filenames, branch
   names. This is the counterpart to the sanitizing in `hyperlink`,
   `notification` and `style`: `decode` shows *why* those sanitizers exist.
3. **Learning**, and as a diffable text form of a byte stream for regression
   tests.

`--unicode` additionally annotates the text between sequences with codepoint
names, categories, widths and grapheme cluster boundaries, all of which
`src/unicode.rs` already provides.

Note the parser direction: `src/input.rs` parses sequences the terminal *sends*
(keys, replies). `decode` needs sequences an application sends — SGR, CSI, OSC,
APC, DCS — which is a different grammar. `term::pretty` already names the
introducers, and the feature database holds the descriptions, so a decoder could
look up "OSC 8" and report it as the hyperlinks feature rather than hard-coding
prose.

Prior art is GNU `teseq`, which is the *only* tool that does this — and it is
dormant and predates every protocol in this project.

## Also considered, not scoped

From two research sweeps. Kept here so the ideas live in one place.

* **`escapade probe` (aggregate)** — probe every feature at once, one table or one
  JSON stream. Now that individual probes are predicates, this is the natural
  capstone: "characterize this terminal in one command". Highest scripting value
  of anything unbuilt.
* **`escapade title`** — `OSC 0`/`OSC 2`, plus the push/pop title stack at
  `CSI 22 t` / `CSI 23 t`. Mundane but very script-friendly.
* **`escapade cursor`** — DECSCUSR shapes and cursor colour.
* **`escapade pointer`** — `OSC 22` mouse-pointer shape. Interesting *because* it
  is three incompatible protocols sharing one number, with Konsole destructively
  resetting on an unrecognised name — exactly the kind of mess this tool exists to
  document — but low practical value.
* **`escapade sync`** — mode 2026 synchronized output, promoted from internal use.

Explicitly rejected, because existing tools do them better: image-to-text
rendering (`chafa` has four backends and the only correct Screen passthrough), a
Unicode width survey (`ucs-detect` owns it at far greater scale), a general
Unicode CLI (`uni` already has `-as json`), a VT100 conformance suite, and a
website — [vtdn.dev](https://github.com/cfoust/vtdn.dev) and
[terminfo.dev](https://github.com/beorn/terminfo.dev) both exist, so publishing
the data is worth more than publishing a site.


## Corrections

Things this file asserted that turned out to be false. Kept rather than quietly
deleted, because the reasoning that produced the error is worth seeing.

### `unscroll` does **not** fix the pager case

This file originally said: *"When `less`, `man` or `git log` exits the alternate
screen, everything it displayed vanishes and the prompt lands near the top of an
otherwise empty screen; the context you were reading is in scrollback, just out of
sight. Unscroll pulls those lines back down."*

That is wrong for two independent reasons, both established from kitty's spec and
from `screen.c`/`history.c`:

1. **On** the alternate screen the sequence degrades to plain `SD` and inserts
   **blank** lines — the alternate screen has no scrollback, and kitty's code
   gates the fill on `self->linebuf == self->main_linebuf`.
2. **After** the pager exits it still cannot recover the pager's output, because
   alternate-screen output never enters scrollback at all, and leaving the
   alternate screen restores the primary screen exactly as it was. So there is
   usually no hole to fill, and what unscroll pulls down is history from *before*
   the pager ran.

What it genuinely fixes is kitty's own stated motivation: shell completions
printed below the prompt that push the top of the primary screen away, and
commands that clear the primary screen or dump a screenful over what you were
reading. This file's *second* example — "a command that clears the screen and then
prints three lines" — was correct.

The lesson for the rest of this file: the `capture` and `decode` sections are
written from the same kind of research summary, and should be treated as
hypotheses until an implementer checks them against a primary source.


### `capture` was built, then dropped

`escapade capture` (`CSI 10 i` for HTML, `CSI 11 i` for SVG) was implemented — 56
tests, an honest best-effort harvest — and then **removed, because two
implementations is too narrow a base to carry a subcommand.** Only xterm supports
both forms, and mintty only the HTML one. The code is gone; the findings are kept
here because they cost real source-reading and would otherwise be lost.

What this file originally claimed about it was wrong in every particular. It said
the `printFileName`, `printFileImmediate` and `printAttributes` resources "decide
where and whether", and that the feature was probably disabled by default:

* **`printFileName` does not exist in xterm.** `printFileImmediate` and
  `printModeImmediate` belong to the `print-immediate` *action*; `printAttributes`,
  `printerCommand` and `printerControlMode` belong to the printer, not the dumps;
  `allowWindowOps` is unrelated.
* **No resource gates the dumps at all.** `xtermMediaControl` has no runtime check.
  The only gate is the compile-time `--disable-screen-dumps` (`OPT_SCREEN_DUMPS`),
  and dumps have been **on by default since patch #331** (2017-12-30), having
  arrived in #323 needing an explicit enable. It cannot be switched off from a
  configuration file.
* The `?` is what separates a screenshot from a print job: `CSI 10 i` and
  `CSI 11 i` dump, while every `CSI ? Ps i` form drives the printer.
* **SVG is xterm-only**, and higher fidelity than the HTML form — it keeps double
  underlines, which the XHTML flattens to single. mintty has `CSI 11 i` in source
  behind an `#ifdef support_SVG` that nothing defines, plus `CSI 12 i` for PNG and
  `CSI 10 ; 8 i` to include scrollback, which xterm cannot do.
* **The round trip cannot be made reliable**, for three independent reasons: the
  output directory is xterm's own cwd, which no control sequence reports and which
  over ssh is the wrong filesystem; the filename resolves only to the second and
  is opened `O_APPEND`, so a same-second repeat concatenates two documents into
  one invalid file; and there is no reply, so no success signal exists.

The lesson that generalises: the `decode` section below is written from the same
kind of research summary, and should be treated as a hypothesis until an
implementer checks it against a primary source. Two of three such sections have now
turned out to be wrong.

## Known issues

* ~~**Unimplemented commands are visible in `--help`**~~ — fixed. Every
  subcommand in `--help` now does something; nothing needs `#[command(hide =
  true)]`.
* **Name collision:** `db::Support` (a support-matrix row) and
  `features::Support` (a three-state probe verdict) are unrelated types with the
  same name. It compiles because they are imported explicitly, but it reads
  badly. `features::Support` wants renaming — `Verdict` is taken locally inside
  `glyph_protocol`, so it needs a third name.
* **`escapade color`'s one-shot report never asks for the colour scheme.**
  `report::run` passes `None` where the mode 2031 scheme would go, so
  `Basis::Reported` is only ever reached under `--watch` and `--is-dark` decides
  from luminance even on a terminal that would have answered outright. Fixing it
  means either a second round trip or threading the `CSI ? 996 n` request into
  `query::round_trip` alongside the colour queries; the latter is better and
  needs the reply matcher to know about a non-OSC answer.
* **`--debug` as a shorthand for `--format debug`** was requested and not done.
  It belongs in one pass across all fourteen probes rather than one feature
  diverging.
* **The support matrices need a staleness marker.** vtdn.dev's per-cell
  `last_scanned` date is the best mechanism found in the prior-art survey and the
  database has no equivalent. Related: export the database as JSON, publish a
  schema, and license the data CC BY 4.0 so it can be shared with the two
  projects already doing this.
* **The env-marker terminal-identification table is duplicated** across probe
  modules, and at one point the copies disagreed. `doctor/identity.rs` is now the
  most complete version; the others should migrate to it. `clipboard/probe.rs`
  is the first to do so — it calls `identity::family` and translates the two
  family tokens that differ from registry ids, rather than carrying a fourth copy
  of the keyword table. The `ENV_MARKERS` list is still per-module, because that
  one maps environment variables rather than names and `identity` has no
  equivalent; a shared version of *that* is the remaining piece of this.
