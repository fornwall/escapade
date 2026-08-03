> [!WARNING]
> **Experimental.** escapade is AI-generated and has not seen real world usage.

# escapade

Demonstrate, visualize, play with and actually use modern terminal features.

Terminals have grown a lot of capabilities that almost nothing takes advantage
of. They are undocumented or documented in one person's blog post, unevenly
supported, and invisible until something renders wrong. `escapade` gives each of
them the same treatment:

| verb | what it does |
| --- | --- |
| `explain` | what the feature is, its escape sequences piece by piece, who uses it, which terminals implement it |
| `probe` | ask *your* terminal whether it implements it, and show what it answered |
| `demo` | watch it work, one step at a time, with the actual bytes on screen |

Some features earn a fourth verb, because they are useful outside a demo:
`graphics render`, `glyph supports`, `grapheme render`.

```console
$ escapade list
$ escapade glyph explain
$ escapade glyph demo
$ escapade graphics render diagram.dot --cols 60
$ escapade hyperlink --url https://example.com "click me"
```

## Features

* **`glyph`** — register your own vector glyphs with the terminal at
  runtime, so an application ships its icons instead of asking users to install a
  patched font.
* **`graphics`** — the kitty graphics protocol: real images, positioned by cell,
  including `.png`, `.svg` and graphviz `.dot` files.
* **`grapheme`** — grapheme clustering, so a family emoji occupies the cells a
  human would count rather than the cells `wcwidth` guesses.
* **`hyperlink`** — OSC 8, which makes a run of text clickable.
* **`notification`** — raise a real desktop notification from inside the
  terminal.

## Design

Four ideas hold this together.

**The feature database is data, not code.** Everything `explain` prints comes
from one TOML file per feature under `data/`, in the spirit of
[webassembly.org/features](https://webassembly.org/features/). Correcting a
support matrix or rewriting a paragraph needs no Rust. Point `ESCAPADE_DATA_DIR`
at a checkout to edit without rebuilding. See [CONTRIBUTING.md](CONTRIBUTING.md).

**Sequences are written out, not wrapped.** The code that drives the terminal is
the code a reader should be able to copy, so it builds escape sequences directly
rather than through a library. Only raw mode goes through `crossterm`, because
doing it properly means `termios` on Unix and console handles on Windows, and
none of that is instructive.

**Probing has a barrier.** "Does this terminal support X?" is answered with
silence when it does not, and a timeout alone cannot tell silence from slowness.
Every capability query is therefore paired with a Primary Device Attributes
request (`CSI c`), which every terminal since the VT100 answers. If the barrier
comes back and the feature query did not, that is a conclusion rather than a
guess — and if neither comes back, the result is reported as *inconclusive*,
which is a third thing.

**Nothing renders by magic.** Each demo step names the bytes it is about to send
before sending them.

## Building

Rust 1.97.1, edition 2024. `rust-toolchain.toml` pins the version.

```console
$ cargo build --release
$ cargo test
$ cargo clippy --all-targets
```

`unsafe_code` is forbidden crate-wide, and `clippy::all` plus
`clippy::pedantic` are warnings.

Rendering `.dot` files needs graphviz on `PATH`; everything else is
self-contained.

## Layout

```
data/
  terminals.toml            the terminal registry, referenced by every matrix
  features/<id>.toml        one file per feature: prose, sequences, support
src/
  db.rs                     loads and validates the database
  term.rs                   raw mode, alternate screen, private modes
  input.rs                  keys, mouse reports and terminal replies, hand-parsed
  unicode.rs                codepoint names, categories, graphemes, widths
  theme.rs                  colors and markdown rendering
  features/
    explain.rs              the shared explain renderer, used by every feature
    <feature>/              one module per feature
```

## A note on support matrices

Terminal support for anything recent is a moving target, and several of the
matrices here were assembled from specifications and release notes rather than
from running every terminal. Rows carry a `source` where one exists. Corrections
are the most useful contribution you can make — the file to edit is named at the
bottom of every `explain` output.

## License

MIT or Apache-2.0, at your option.
