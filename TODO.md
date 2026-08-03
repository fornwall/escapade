# TODO

Outstanding work, in rough order of value. Conventions for writing any of it are
in `AGENTS.md`.

## Unverified support matrices

The highest-value work here, and it needs no Rust — see `CONTRIBUTING.md`. `no`
and `unknown` are different claims, so these rows must be *checked*, not guessed.

* **`clipboard` has five `unknown` rows** — VS Code, GNU Screen, ConEmu, Warp and
  wayst. Each says "not checked" rather than guessing. GNU Screen's source could
  not be reached at all while the file was written.
* **`color` has ten `unknown` rows** — Rio, mintty, Windows Terminal, VS Code,
  st, ConEmu, Warp, wayst, GNU Screen and Zellij. Two `yes` rows also need
  pinning down: VTE and Konsole cite a repository root rather than a specific
  file, and VTE's own row admits palette queries and mode 2031 were not verified.
* **`unscroll`'s contour row** does not record which release the sequence arrived
  in; contour's changelog would settle it.

## Known issues

* **`escapade color`'s one-shot report never asks for the colour scheme.**
  `report::run` passes `None` where the DEC mode 2031 scheme would go, so
  `Basis::Reported` is only ever reached under `--watch`, and `--is-dark` decides
  from luminance even on a terminal that would have answered outright. Fixing it
  means either a second round trip or threading the `CSI ? 996 n` request into
  `query::round_trip` alongside the colour queries; the latter is better and
  needs the reply matcher to know about a non-OSC answer.
* **Name collision:** `db::Support` (a support-matrix row) and
  `features::Support` (a three-state probe verdict) are unrelated types with the
  same name. It compiles because they are imported explicitly, but it reads
  badly. `features::Support` wants renaming — `Verdict` is taken locally inside
  `glyph_protocol`, so it needs a third name.
* **`--debug` as a shorthand for `--format debug`** was requested and not done.
  It belongs in one pass across every probe rather than one feature diverging.
* **The env-marker terminal-identification table is duplicated.** `ENV_MARKERS`
  is carried separately in `clipboard`, `progress`, `style` and `hyperlink`, and
  at one point the copies disagreed. `doctor/identity.rs` is the most complete
  identification table and `clipboard/probe.rs` already calls `identity::family`
  instead of carrying its own keyword list; `ENV_MARKERS` maps environment
  variables rather than names, so `identity` has no equivalent yet. A shared
  version of that table is the remaining piece.
* **The feature schema does not say what identifies a sequence.** `decode`'s
  attribution to a feature (`decode/catalog.rs`) has to derive the discriminator
  — which number or verb makes `OSC 9 ; 4` progress rather than a notification —
  from the `request` text by heuristic, because no field declares it. That module
  points here for the schema change that would make it exact: a per-sequence
  field naming the discriminating parameter, so attribution becomes a lookup.
* **The support matrices need a staleness marker.** vtdn.dev's per-cell
  `last_scanned` date is the best mechanism found in the prior-art survey, and
  the database has no equivalent. Related: export the database as JSON, publish a
  schema, and license the data CC BY 4.0 so it can be shared with the two
  projects already doing this.

## Ideas, not scoped

From two research sweeps, kept so the ideas live in one place. **Treat each as a
hypothesis until an implementer checks it against a primary source** — of three
such sections written this way and then actually built, two turned out to be
wrong about the protocol in ways only the source revealed.

* **`escapade probe` (aggregate)** — probe every feature at once, one table or
  one JSON stream. Now that individual probes are predicates, this is the natural
  capstone: "characterize this terminal in one command". Highest scripting value
  of anything unbuilt.
* **`escapade title`** — `OSC 0`/`OSC 2`, plus the push/pop title stack at
  `CSI 22 t` / `CSI 23 t`. Mundane but very script-friendly.
* **`escapade cursor`** — DECSCUSR shapes and cursor colour.
* **`escapade pointer`** — `OSC 22` mouse-pointer shape. Interesting *because* it
  is three incompatible protocols sharing one number, with Konsole destructively
  resetting on an unrecognised name — exactly the kind of mess this tool exists
  to document — but low practical value.
* **`escapade sync`** — mode 2026 synchronized output, promoted from internal use.

## Rejected

Not worth building, with the reason, so the question does not get reopened
blind.

* **Screen dumps (`CSI 10 i` HTML, `CSI 11 i` SVG).** Too narrow a base to carry
  a subcommand: only xterm supports both forms, and mintty only the HTML one.
  What source-reading established, in case it is ever reconsidered:
  * **No resource gates the dumps.** `xtermMediaControl` has no runtime check;
    the only gate is the compile-time `--disable-screen-dumps`
    (`OPT_SCREEN_DUMPS`), and dumps have been on by default since patch #331
    (2017-12-30). They cannot be switched off from a configuration file, and
    `printFileName` does not exist in xterm at all.
  * The `?` is what separates a screenshot from a print job: `CSI 10 i` and
    `CSI 11 i` dump, while every `CSI ? Ps i` form drives the printer.
  * **SVG is xterm-only** and higher fidelity than the HTML form — it keeps
    double underlines, which the XHTML flattens to single. mintty has `CSI 11 i`
    in source behind an `#ifdef support_SVG` that nothing defines, plus
    `CSI 12 i` for PNG and `CSI 10 ; 8 i` to include scrollback, neither of which
    xterm can do.
  * **The round trip cannot be made reliable**, for three independent reasons:
    the output directory is xterm's own cwd, which no control sequence reports
    and which over ssh is the wrong filesystem; the filename resolves only to the
    second and is opened `O_APPEND`, so a same-second repeat concatenates two
    documents into one invalid file; and there is no reply, so no success signal
    exists.
* **Image-to-text rendering** — `chafa` has four backends and the only correct
  GNU Screen passthrough.
* **A Unicode width survey** — `ucs-detect` owns it at far greater scale.
* **A general Unicode CLI** — `uni` already has `-as json`.
* **A VT100 conformance suite.**
* **A website** — [vtdn.dev](https://github.com/cfoust/vtdn.dev) and
  [terminfo.dev](https://github.com/beorn/terminfo.dev) both exist, so publishing
  the data is worth more than publishing a site.
