//! The step-by-step demo.
//!
//! Each step states what it is about to do, shows the actual escape sequence and
//! names every piece of it, and only then renders something clickable. By the
//! end, a reader has seen the bytes behind every link on screen.
//!
//! The arc builds from one link through the two terminators, the `id` parameter,
//! links used as layout, `file://` and its hostname rule, and what a terminal
//! without OSC 8 does — then finishes with a page where everything is clickable,
//! and leaves the alternate screen.
//!
//! One thing this demo deliberately does *not* do is enable mouse reporting.
//! While an application has the mouse captured, clicks belong to the application
//! rather than the terminal, so hyperlinks stop being clickable — which would be
//! a strange way to demonstrate hyperlinks.

use anyhow::Result;

use crate::{
    db::Catalog,
    features::{
        hyperlink::{
            Hyperlink, MAX_ID_LEN, MAX_URI_LEN, Terminator, close, file_uri, local_hostname, probe,
            sanitize_text, sanitize_uri,
        },
        wrap,
    },
    input::{Advance, InputReader},
    term::{CSI, Term, pretty},
    theme::{Ink, Theme},
};

/// Left margin for everything the demo draws.
const MARGIN: u16 = 3;

/// `(crate, version)` — escapade's own dependency list, used by the step that
/// shows hyperlinks as layout. Real crates, so the links go somewhere.
const DEPENDENCIES: &[(&str, &str)] = &[
    ("anyhow", "1.0.104"),
    ("base64", "0.23.0"),
    ("clap", "4.6.4"),
    ("crossterm", "0.29.0"),
    ("serde", "1.0.229"),
    ("termimad", "0.35.1"),
    ("toml", "1.1.4"),
    ("unicode-segmentation", "1.13.3"),
];

/// What the user asked for at the end of a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    /// Go on to the next step.
    Next,
    /// Back up one step.
    Prev,
    /// Leave the demo.
    Quit,
}

impl From<Advance> for Flow {
    fn from(advance: Advance) -> Self {
        match advance {
            Advance::Next => Self::Next,
            Advance::Prev => Self::Prev,
            Advance::Quit => Self::Quit,
        }
    }
}

/// A step's implementation.
type StepFn = fn(&mut Ctx<'_>) -> Result<Flow>;

/// One entry in the demo script: a short name for `--only`, a title for the
/// header, and the code that draws it.
struct Step {
    /// Kebab-case name, stable enough to put in a script or a bug report.
    name: &'static str,
    /// Human title shown in the header.
    title: &'static str,
    /// What the step does.
    run: StepFn,
}

impl std::fmt::Debug for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Step")
            .field("name", &self.name)
            .field("title", &self.title)
            .finish_non_exhaustive()
    }
}

/// Declares a step, keeping the name next to the function it names.
macro_rules! step {
    ($name:literal, $title:literal, $run:expr) => {
        Step {
            name: $name,
            title: $title,
            run: $run,
        }
    };
}

/// The script. Order matters: later steps assume vocabulary from earlier ones.
const STEPS: &[Step] = &[
    step!(
        "plain",
        "One link, and the bytes that make it",
        step_first_link
    ),
    step!("bel", "ST or BEL", step_terminators),
    step!("id", "id=, and a link that wraps", step_id),
    step!(
        "listing",
        "Links as layout: every row clickable",
        step_table
    ),
    step!("file", "file:// needs a hostname", step_file),
    step!(
        "unsupported",
        "What a terminal without OSC 8 does",
        step_degrade
    ),
    step!(
        "hostile",
        "Hostile input, and what happens to it",
        step_sanitize
    ),
    step!(
        "everything",
        "Finale: a page where everything is a link",
        step_finale
    ),
    step!("restore", "Putting the terminal back", step_leave),
];

/// The step names, in script order, for [`crate::features::select_steps`].
fn step_names() -> Vec<&'static str> {
    STEPS.iter().map(|step| step.name).collect()
}

/// Prints the step list for `--list`.
pub fn list_steps(theme: &Theme) {
    println!();
    println!("  {}", theme.paint("OSC 8 hyperlink demo", Ink::Title));
    println!();
    let width = STEPS.iter().map(|step| step.name.len()).max().unwrap_or(0);
    for (index, step) in STEPS.iter().enumerate() {
        println!(
            "  {} {}  {}",
            theme.paint(&format!("{:>2}.", index + 1), Ink::Muted),
            theme.paint(&format!("{:<width$}", step.name), Ink::Accent),
            step.title
        );
    }
    println!();
    println!(
        "  {}",
        theme.paint(
            "escapade hyperlink demo --only <NAME>   run just that step",
            Ink::Muted
        )
    );
    println!(
        "  {}",
        theme.paint(
            "escapade hyperlink demo --step N        start there and carry on",
            Ink::Muted
        )
    );
    println!();
}

/// Runs the demo.
///
/// # Errors
///
/// Fails if `only` names no step or is an ambiguous prefix, or if the terminal
/// cannot be set up or written to.
pub fn run(
    theme: &Theme,
    catalog: &Catalog,
    force: bool,
    only: Option<&str>,
    start_step: Option<usize>,
) -> Result<()> {
    // Resolved before anything is probed or drawn, so a mistyped `--only` is a
    // plain error message rather than an error message on the alternate screen.
    let selection = crate::features::select_steps(&step_names(), only, start_step)?;

    let mut term = Term::new()?;
    let mut input = InputReader::new()?;

    let outcome = probe::probe(catalog, &mut term, &mut input)?;
    // Not a `--force` case: without a terminal there is no alternate screen to
    // take over, no keypress to wait for, and nothing to click. Refusing here
    // beats letting raw mode fail with an errno.
    if !outcome.interactive || (!outcome.looks_supported() && !force) {
        term.restore();
        refuse(theme, &outcome);
        return Ok(());
    }

    let mut index = selection.start;

    term.enable_raw()?;
    term.enter_alt_screen()?;
    term.hide_cursor()?;

    let mut ctx = Ctx {
        term: &mut term,
        input: &mut input,
        theme,
        host: local_hostname(),
        terminal_name: outcome.terminal_name.clone(),
        forced: force && !outcome.looks_supported(),
        row: 0,
        width: 0,
    };

    let result = (|| -> Result<()> {
        loop {
            let flow = (STEPS[index].run)(&mut ctx)?;
            match flow {
                Flow::Next => {
                    // `--only` runs the one step and stops, rather than carrying
                    // on through the rest of the script.
                    if selection.single || index + 1 >= STEPS.len() {
                        return Ok(());
                    }
                    index += 1;
                }
                Flow::Prev => index = index.saturating_sub(1),
                Flow::Quit => return Ok(()),
            }
        }
    })();

    // A link left open would attach its URI to the shell prompt that follows.
    let _ = ctx.term.write_now(&close(Terminator::St));
    term.restore();
    result
}

/// The refusal shown when the terminal appears not to support hyperlinks.
fn refuse(theme: &Theme, outcome: &probe::ProbeOutcome) {
    println!();
    let message = "sorry - this does not seem to be supported by your terminal - check \
                   'escapade hyperlink explain' for support, or use \
                   'escapade hyperlink demo --force' to disable feature probing.";
    for line in wrap(message, theme.width().saturating_sub(2)) {
        println!("  {}", theme.paint(&line, Ink::Warn));
    }

    println!();
    // Two quite different reasons produce the same refusal, and saying which one
    // applies is the difference between a fixable problem and a mystery.
    let reason = if outcome.interactive {
        match (&outcome.terminal_name, outcome.status) {
            (Some(name), Some(status)) => format!(
                "This terminal was identified as {name}, which the support matrix records as \
                 {}. That is an inference, not a measurement: OSC 8 has no capability query, so \
                 nothing was actually asked. If the database is wrong — a newer release, a fork, \
                 a setting — use --force.",
                status.label()
            ),
            _ => "The terminal could not be identified, and OSC 8 has no capability query to fall \
                  back on, so nothing could be established either way. --force runs the demo \
                  regardless."
                .to_owned(),
        }
    } else {
        "stdout is not a terminal. The demo needs the alternate screen, a keypress to advance, \
         and a pointer to hover with, and a pipe has none of those — so --force cannot help here. \
         `escapade hyperlink --url … TEXT` does work in a pipe, and `escapade hyperlink explain` \
         needs nothing at all."
            .to_owned()
    };
    for line in wrap(&reason, theme.width().saturating_sub(2)) {
        println!("  {}", theme.paint(&line, Ink::Muted));
    }
    println!();
}

// ---------------------------------------------------------------------------
// Drawing context
// ---------------------------------------------------------------------------

/// Everything a step needs, plus a cursor so steps can write line by line.
struct Ctx<'a> {
    term: &'a mut Term,
    input: &'a mut InputReader,
    theme: &'a Theme,
    /// This machine's hostname, for `file://` links.
    host: String,
    /// Display name of the terminal, when the probe worked it out.
    terminal_name: Option<String>,
    /// Whether the demo is running past a negative inference.
    forced: bool,
    /// Next row to write to.
    row: u16,
    /// Usable text width.
    width: usize,
}

impl std::fmt::Debug for Ctx<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx")
            .field("row", &self.row)
            .field("width", &self.width)
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl Ctx<'_> {
    /// Clears the screen and draws the chrome for step `index`.
    fn page(&mut self, index: usize) -> Result<()> {
        self.term.refresh_size()?;
        self.width = usize::from(self.term.cols.saturating_sub(MARGIN * 2)).min(96);
        self.term.clear()?;
        // Any link left open by the previous step ends here, so a stray URI
        // cannot bleed onto this page.
        self.term.write(&close(Terminator::St))?;

        let title = STEPS[index].title;
        let counter = format!("step {}/{}", index + 1, STEPS.len());
        let heading = "escapade · OSC 8 hyperlinks";
        let gap = self
            .width
            .saturating_sub(heading.chars().count() + counter.chars().count());

        self.row = 2;
        let line = format!(
            "{}{}{}",
            self.theme.paint(heading, Ink::Title),
            " ".repeat(gap),
            self.theme.paint(&counter, Ink::Muted)
        );
        self.line(&line)?;
        self.line(&self.theme.paint(&"─".repeat(self.width), Ink::Muted))?;
        self.blank();
        self.line(&self.theme.paint(title, Ink::Accent))?;
        self.blank();
        Ok(())
    }

    /// Writes one line at the margin and advances the cursor.
    fn line(&mut self, text: &str) -> Result<()> {
        let row = self.row;
        self.term.put(MARGIN, row, text)?;
        self.row = self.row.saturating_add(1);
        Ok(())
    }

    /// Writes one line at a deeper indent.
    fn indented(&mut self, indent: usize, text: &str) -> Result<()> {
        let row = self.row;
        self.term
            .put(MARGIN + u16::try_from(indent).unwrap_or(0), row, text)?;
        self.row = self.row.saturating_add(1);
        Ok(())
    }

    /// Skips a row.
    fn blank(&mut self) {
        self.row = self.row.saturating_add(1);
    }

    /// Wraps and writes a paragraph.
    fn para(&mut self, text: &str) -> Result<()> {
        for line in wrap(text, self.width) {
            self.line(&line)?;
        }
        Ok(())
    }

    /// Wraps and writes a paragraph in the muted tone.
    fn note(&mut self, text: &str) -> Result<()> {
        for line in wrap(text, self.width.saturating_sub(2)) {
            let painted = self.theme.paint(&line, Ink::Muted);
            self.indented(2, &painted)?;
        }
        Ok(())
    }

    /// Shows a sequence in readable form, then names each of its pieces.
    fn sequence(&mut self, raw: &str, parts: &[(&str, &str)]) -> Result<()> {
        let painted = self.theme.paint(&pretty(raw), Ink::Code);
        self.indented(2, &painted)?;
        self.blank();
        let token_width = parts
            .iter()
            .map(|(token, _)| token.chars().count())
            .max()
            .unwrap_or(0);
        for (token, meaning) in parts {
            let label = format!("{token:<token_width$}");
            let lines = wrap(meaning, self.width.saturating_sub(token_width + 7));
            for (index, line) in lines.iter().enumerate() {
                if index == 0 {
                    let text = format!(
                        "{}  {}",
                        self.theme.paint(&label, Ink::Accent),
                        self.theme.paint(line, Ink::Muted)
                    );
                    self.indented(2, &text)?;
                } else {
                    let text = self.theme.paint(line, Ink::Muted);
                    self.indented(4 + token_width, &text)?;
                }
            }
        }
        Ok(())
    }

    /// Builds a clickable, styled run of text. See [`render_link`].
    fn link(&self, uri: &str, text: &str, id: Option<&str>) -> String {
        render_link(self.theme, uri, text, id)
    }

    /// Writes one indented line that is entirely a link.
    fn link_line(&mut self, indent: usize, uri: &str, text: &str, id: Option<&str>) -> Result<()> {
        let rendered = self.link(uri, text, id);
        self.indented(indent, &rendered)
    }

    /// Draws the footer hint and blocks until the user chooses.
    fn wait(&mut self) -> Result<Flow> {
        let row = self.term.rows.saturating_sub(2);
        let hint = self.theme.paint(
            "space/enter next · p back · q quit · click the links",
            Ink::Muted,
        );
        self.term.put(MARGIN, row, &hint)?;
        self.term.flush()?;
        Ok(self.input.wait_for_advance()?.into())
    }

    /// A one-line reminder that the demo is running past a negative inference.
    fn forced_note(&mut self) -> Result<()> {
        if self.forced {
            self.note(
                "The probe inferred that this terminal does not support OSC 8, and the demo is \
                 running under --force. The text will still be there; only the links will be \
                 missing.",
            )?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

/// The whole feature, in one sequence.
fn step_first_link(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(0)?;
    ctx.para(
        "There is no handshake. Nothing is asked, nothing is answered. A hyperlink is one OSC \
         string that turns the URI on, the text you want read, and one that turns it off again.",
    )?;
    ctx.blank();

    let link = Hyperlink::new("https://example.com")?;
    ctx.sequence(
        &link.wrap("click me"),
        &[
            (
                "OSC",
                "Operating System Command — `ESC ]`. The escape class for things said to the \
                 terminal rather than drawn on the screen.",
            ),
            (
                "8",
                "the command number. A terminal that has never heard of command 8 still parses an \
                 OSC string correctly, throws it away, and prints the text — which is why this is \
                 safe to send without asking first.",
            ),
            (
                ";;",
                "two semicolons. The first ends the command number; the second ends the parameter \
                 field, which is empty here. This is the shape of nearly every hyperlink in the \
                 wild.",
            ),
            (
                "https://…",
                "the target. Only bytes 32–126 are allowed, so everything else is percent-encoded \
                 before it goes out.",
            ),
            (
                "ST",
                "String Terminator — `ESC \\`. Ends the OSC string. Everything printed from here on \
                 carries the URI.",
            ),
            ("click me", "the visible text. Ordinary output, outside the OSC string."),
            (
                "OSC 8;; ST",
                "the same sequence with the URI left out. An empty URI means no link, so this is \
                 how a link ends.",
            ),
        ],
    )?;
    ctx.blank();

    ctx.indented(2, &ctx.theme.paint("and it renders as:", Ink::Strong))?;
    ctx.blank();
    ctx.link_line(4, "https://example.com", "click me", None)?;
    ctx.blank();
    ctx.note(
        "The URL is nowhere on screen. That is the point — a terminal that guesses links from \
         patterns can only link text that looks like a URL, so the URL has to be printed whether \
         anyone wants to read it or not.",
    )?;
    ctx.forced_note()?;

    ctx.wait()
}

/// The two string terminators.
fn step_terminators(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(1)?;
    ctx.para(
        "An OSC string can end two ways. `ST` is `ESC \\`, from ECMA-48. `BEL` is a single 0x07, \
         which xterm accepted and everyone copied. Both work; they are not equally good ideas.",
    )?;
    ctx.blank();

    let st = Hyperlink::new("https://example.com/st")?;
    let bel = Hyperlink::new("https://example.com/bel")?.with_terminator(Terminator::Bel);

    for (link, label) in [
        (&st, "ST — the standard"),
        (&bel, "BEL — the xterm spelling"),
    ] {
        let rendered = format!(
            "{}  {}",
            ctx.theme
                .paint(&format!("{:<26}", pretty(&link.open())), Ink::Code),
            ctx.theme.paint(label, Ink::Muted)
        );
        ctx.indented(2, &rendered)?;
    }
    ctx.blank();

    let sizes = format!(
        "{} bytes with ST, {} bytes with BEL — the whole saving is one byte per sequence.",
        st.open().len(),
        bel.open().len()
    );
    ctx.indented(2, &ctx.theme.paint(&sizes, Ink::Strong))?;
    ctx.blank();

    ctx.indented(
        2,
        &ctx.theme
            .paint("both of these should be clickable:", Ink::Strong),
    )?;
    ctx.blank();
    let st_line = format!(
        "{}{}{}",
        st.open(),
        ctx.theme.paint("terminated with ST", Ink::Link),
        st.close()
    );
    ctx.indented(4, &st_line)?;
    let bel_line = format!(
        "{}{}{}",
        bel.open(),
        ctx.theme.paint("terminated with BEL", Ink::Link),
        bel.close()
    );
    ctx.indented(4, &bel_line)?;
    ctx.blank();

    ctx.note(
        "Prefer ST. The failure modes are not symmetric: on a terminal that does not recognise the \
         OSC and does not treat BEL as a terminator, `ST` is swallowed as part of a string nobody \
         wanted, and `BEL` rings the bell.",
    )?;

    ctx.wait()
}

/// The `id` parameter, and why a wrapped link needs one.
fn step_id(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(2)?;
    ctx.para(
        "A link often does not fit on one row. Two runs of cells pointing at the same URI are \
         still two separate links as far as the terminal knows, so hovering one highlights half of \
         it. The `id` parameter is the fix: same URI plus same non-empty id means one link.",
    )?;
    ctx.blank();

    let uri = "https://doc.rust-lang.org/book/";
    let tagged = Hyperlink::new(uri)?.with_id("the-book")?;

    ctx.sequence(
        &tagged.open(),
        &[
            (
                "id=the-book",
                "the parameter field, no longer empty. Parameters are colon-separated `key=value` \
                 pairs, and `id` is the only key anyone has defined.",
            ),
            (
                "the-book",
                "an opaque token. Terminals compare it byte for byte, so it must not contain `:` \
                 or `;`, must stay inside printable ASCII, and — in VTE — must be at most \
                 250 bytes.",
            ),
        ],
    )?;
    ctx.blank();

    ctx.indented(2, &ctx.theme.paint("no id — hover each row:", Ink::Strong))?;
    ctx.link_line(4, uri, "The Rust Programming Language:", None)?;
    ctx.link_line(4, uri, "a language empowering everyone", None)?;
    ctx.blank();

    ctx.indented(
        2,
        &ctx.theme
            .paint("id=the-book on both rows — hover either:", Ink::Strong),
    )?;
    let first = format!(
        "{}{}{}",
        tagged.open(),
        ctx.theme.paint("The Rust Programming Language:", Ink::Link),
        tagged.close()
    );
    ctx.indented(4, &first)?;
    let second = format!(
        "{}{}{}",
        tagged.open(),
        ctx.theme.paint("a language empowering everyone", Ink::Link),
        tagged.close()
    );
    ctx.indented(4, &second)?;
    ctx.blank();

    ctx.note(&format!(
        "Do not hand out ids you do not need. Two unrelated links that happen to share a URI and \
         an id highlight together, which reads as a bug. Simple filters should omit it; a \
         screen-managing application should derive it from something unique, and a multi-window \
         one should prefix it per window. Escapade refuses an id longer than {MAX_ID_LEN} bytes \
         rather than let VTE truncate two different ids into one.",
    ))?;
    ctx.blank();
    ctx.note(
        "The unused half of the parameter field is deliberate room to grow: unknown keys are \
         ignored, so `id=x:something=new` is forward-compatible. Only `id` exists today.",
    )?;

    ctx.wait()
}

/// Hyperlinks used as layout: a table where the URLs never appear.
fn step_table(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(3)?;
    ctx.para(
        "Once text can carry a target, output stops having to be written for a pattern matcher. \
         Here are this program's own dependencies. Every crate name links to crates.io and every \
         version to its documentation, and not one URL is on screen.",
    )?;
    ctx.blank();

    let header = format!(
        "{}  {}",
        ctx.theme.paint(&format!("{:<22}", "crate"), Ink::Muted),
        ctx.theme.paint("version", Ink::Muted)
    );
    ctx.indented(2, &header)?;
    for (name, version) in DEPENDENCIES {
        let crate_link = ctx.link(
            &format!("https://crates.io/crates/{name}"),
            &format!("{name:<22}"),
            None,
        );
        let docs_link = ctx.link(&format!("https://docs.rs/{name}/{version}"), version, None);
        let row = format!("{crate_link}  {docs_link}");
        ctx.indented(2, &row)?;
    }
    ctx.blank();

    ctx.note(
        "This is what `ls --hyperlink`, `gcc -fdiagnostics-urls`, `ripgrep --hyperlink-format` and \
         `systemctl` all do. The program printing the output already knows the target; nothing has \
         to reconstruct it from the rendered text afterwards.",
    )?;
    ctx.blank();
    ctx.note(
        "One thing to keep in mind: the URI is a cell attribute, and the clipboard carries \
         characters. Copy a row from this table and you get the crate name, not the URL.",
    )?;

    ctx.wait()
}

/// `file://` and the hostname rule.
fn step_file(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(4)?;
    ctx.para(
        "Local files are where OSC 8 earns its keep, and where the specification has its one \
         genuinely surprising rule: a `file://` URI in a hyperlink must carry a hostname.",
    )?;
    ctx.blank();

    let host = ctx.host.clone();
    let wrong = "file:///etc/hostname";
    let right = file_uri(&host, "/etc/hostname");

    ctx.indented(
        2,
        &ctx.theme
            .paint(&format!("{:<10}{wrong}", "wrong:"), Ink::Bad),
    )?;
    ctx.indented(
        2,
        &ctx.theme
            .paint(&format!("{:<10}{right}", "right:"), Ink::Good),
    )?;
    ctx.blank();
    ctx.note(
        "The terminal compares that hostname against its own and refuses to open a file belonging \
         to a machine it is not running on. Without it, output from the far end of an ssh \
         connection quietly points at your local filesystem — same path, entirely different file. \
         Empty and `localhost` both mean \"here\", which is why the wrong form works right up until \
         it matters.",
    )?;
    ctx.blank();

    ctx.indented(
        2,
        &ctx.theme
            .paint("real files, in this directory:", Ink::Strong),
    )?;
    let entries = list_directory(6);
    if entries.is_empty() {
        ctx.note("(nothing readable here)")?;
    } else {
        for (label, path) in &entries {
            let uri = file_uri(&host, path);
            ctx.link_line(4, &uri, label, None)?;
        }
    }
    ctx.blank();
    ctx.note(&format!(
        "Hostname taken as `{host}`. gethostname() needs libc and this crate forbids unsafe, so it \
         comes from $HOSTNAME, $HOST or the kernel's own copy, falling back to `localhost`.",
    ))?;

    ctx.wait()
}

/// Graceful degradation.
fn step_degrade(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(5)?;
    ctx.para(
        "This is the reason there is no capability query. ECMA-48 says an OSC string runs to its \
         terminator, whatever the command number means — so a terminal that has never heard of \
         command 8 still knows exactly where the string ends, and drops it.",
    )?;
    ctx.blank();

    let link = Hyperlink::new("https://example.com")?;
    ctx.indented(2, &ctx.theme.paint("what goes out:", Ink::Strong))?;
    ctx.indented(
        4,
        &ctx.theme.paint(&pretty(&link.wrap("click me")), Ink::Code),
    )?;
    ctx.blank();

    ctx.indented(
        2,
        &ctx.theme
            .paint("what a terminal with OSC 8 shows:", Ink::Strong),
    )?;
    ctx.link_line(4, "https://example.com", "click me", None)?;
    ctx.blank();

    ctx.indented(
        2,
        &ctx.theme
            .paint("what a terminal without it shows:", Ink::Strong),
    )?;
    ctx.indented(4, "click me")?;
    ctx.blank();

    ctx.note(
        "Same eight characters, same column, no stray bytes and no corrupted line. The link is \
         gone; nothing else is. That is what makes it correct to emit hyperlinks unconditionally \
         and never branch on support.",
    )?;
    ctx.blank();
    ctx.note(
        "It also means detection is genuinely impossible from this end. There is no query to send \
         and no observable difference between the two screens above, so `escapade hyperlink probe` \
         reports an inference — from XTVERSION, the environment and the support matrix — rather \
         than a measurement. It is the only probe in escapade that cannot actually ask.",
    )?;
    if let Some(name) = ctx.terminal_name.clone() {
        ctx.blank();
        ctx.note(&format!("The probe thinks this is {name}."))?;
    }

    ctx.wait()
}

/// What sanitisation is for.
fn step_sanitize(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(6)?;
    ctx.para(
        "A URI inside an OSC string is only safe while it cannot end the OSC string. If a branch \
         name, a filename or a redirect target reaches the wire unencoded, whoever controls it \
         controls the terminal.",
    )?;
    ctx.blank();

    // Shown through sanitize_text so the control bytes are visible rather than
    // executed: this is a demonstration, not an attack.
    let hostile_uri = "https://example.com/\u{1b}\\\u{1b}[2J\u{1b}]0;pwned\u{7}";
    let encoded = sanitize_uri(hostile_uri)?;

    ctx.indented(2, &ctx.theme.paint("a hostile target:", Ink::Bad))?;
    ctx.indented(4, &ctx.theme.paint(&sanitize_text(hostile_uri), Ink::Code))?;
    ctx.note(
        "An `ST` to close the OSC string early, then a screen clear, then a window-title sequence \
         — arbitrary escape sequences with the terminal's full authority.",
    )?;
    ctx.blank();

    ctx.indented(2, &ctx.theme.paint("after percent-encoding:", Ink::Good))?;
    ctx.indented(4, &ctx.theme.paint(&encoded, Ink::Code))?;
    ctx.note(
        "Every byte outside 32–126 becomes three harmless characters. `%1B` is not an escape; it \
         is a percent sign, a one and a B. The specification requires this anyway; the security \
         property is a bonus.",
    )?;
    ctx.blank();

    let hostile_text = "invoice.pdf\u{1b}]8;;https://evil.example\u{1b}\\ and the rest is mine";
    ctx.indented(2, &ctx.theme.paint("hostile link text:", Ink::Bad))?;
    ctx.indented(4, &ctx.theme.paint(&sanitize_text(hostile_text), Ink::Code))?;
    ctx.note(
        "The visible text is *outside* the OSC string, so it needs the opposite treatment: it \
         cannot be percent-encoded, because it is meant to be read. Control bytes are replaced \
         with their Control Pictures glyphs instead — inert, one cell wide, and visible.",
    )?;
    ctx.blank();

    ctx.indented(2, &ctx.theme.paint("both, rendered:", Ink::Strong))?;
    let link = Hyperlink::new(hostile_uri)?;
    let rendered = format!(
        "{}{}{}",
        link.open(),
        ctx.theme.paint(&sanitize_text(hostile_text), Ink::Link),
        link.close()
    );
    ctx.indented(4, &rendered)?;
    ctx.blank();
    ctx.note(&format!(
        "The link still exists and points somewhere useless. Nothing executed, the line after it \
         is intact, and the screen was not cleared. The remaining limits are lengths: {MAX_URI_LEN} \
         bytes of URI and {MAX_ID_LEN} bytes of id, past which the terminal drops the link and \
         keeps the text.",
    ))?;

    ctx.wait()
}

/// The payoff: a page of nothing but links.
fn step_finale(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(7)?;
    ctx.para(
        "Put it together and terminal output stops being a wall of text with some URLs in it. \
         Everything below is clickable, and there is not a single URL on the screen.",
    )?;
    ctx.blank();

    let host = ctx.host.clone();

    ctx.indented(2, &ctx.theme.paint("this directory", Ink::Accent))?;
    let entries = list_directory(10);
    for (index, (label, path)) in entries.iter().enumerate() {
        let uri = file_uri(&host, path);
        let bullet = ctx.theme.paint("·", Ink::Muted);
        // The id is derived from the row number rather than the filename: a
        // filename can contain `:` or `;`, which a parameter value may not.
        let link = ctx.link(&uri, label, Some(&format!("tree-{index}")));
        ctx.indented(4, &format!("{bullet} {link}"))?;
    }
    if entries.is_empty() {
        ctx.indented(4, &ctx.theme.paint("(nothing readable here)", Ink::Muted))?;
    }
    ctx.blank();

    ctx.indented(2, &ctx.theme.paint("further reading", Ink::Accent))?;
    for (title, uri) in [
        (
            "the specification — Hyperlinks in Terminal Emulators",
            "https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda",
        ),
        (
            "OSC8-Adoption — which terminal shipped it, and when",
            "https://github.com/Alhadis/OSC8-Adoption",
        ),
        (
            "ECMA-48 — the standard that defines OSC and ST",
            "https://ecma-international.org/publications-and-standards/standards/ecma-48/",
        ),
    ] {
        let bullet = ctx.theme.paint("›", Ink::Muted);
        let link = ctx.link(uri, title, None);
        ctx.indented(4, &format!("{bullet} {link}"))?;
    }
    ctx.blank();

    ctx.indented(2, &ctx.theme.paint("and one you can run", Ink::Accent))?;
    ctx.indented(
        4,
        &ctx.theme.paint(
            "escapade hyperlink --url https://example.com \"click me\"",
            Ink::Code,
        ),
    )?;
    ctx.blank();
    ctx.note(
        "If a link is not clickable, try holding Shift: a full-screen application with mouse \
         reporting on takes clicks for itself. This demo never enables mouse reporting, which is \
         why the links above should work without it.",
    )?;

    ctx.wait()
}

/// The last step: hand the terminal back.
fn step_leave(ctx: &mut Ctx<'_>) -> Result<Flow> {
    ctx.page(8)?;
    ctx.para(
        "One piece of housekeeping. A hyperlink is a state attribute, so a link left open attaches \
         its URI to whatever is printed next — including the shell prompt. Closing costs one \
         sequence and is legal even when nothing is open.",
    )?;
    ctx.blank();

    ctx.sequence(
        &close(Terminator::St),
        &[(
            "OSC 8;; ST",
            "close. No parameters, no URI. Emit this after every label and there is no state to \
             keep track of.",
        )],
    )?;
    ctx.blank();
    ctx.term.write(&close(Terminator::St))?;
    ctx.indented(2, &ctx.theme.paint("✔ any open link closed", Ink::Good))?;
    ctx.blank();

    ctx.sequence(
        &format!("{CSI}?1049l"),
        &[(
            "CSI ?1049l",
            "leave the alternate screen. Mode 1049 saved the cursor and switched buffers on the \
             way in, so your scrollback is exactly as you left it — this demo never scrolled it.",
        )],
    )?;
    ctx.blank();
    ctx.note("Press space to return to your shell.")?;

    let flow = ctx.wait()?;

    // The alternate screen goes away here rather than in the cleanup path, so
    // that leaving it is a visible step of the demo rather than a side effect.
    ctx.term.show_cursor()?;
    ctx.term.leave_alt_screen()?;
    ctx.term.disable_raw()?;

    println!();
    let spec = ctx.link(
        "https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda",
        "Hyperlinks in Terminal Emulators",
        None,
    );
    println!(
        "  {} {spec}",
        ctx.theme.paint("Specification:", Ink::Accent)
    );
    println!(
        "  {}",
        ctx.theme.paint(
            "That line is itself a hyperlink — you are back in your shell, and it still works.",
            Ink::Muted
        )
    );
    println!();

    match flow {
        Flow::Prev => Ok(Flow::Prev),
        _ => Ok(Flow::Quit),
    }
}

/// Builds one clickable, styled run of text.
///
/// The label goes through [`sanitize_text`] on the way in. Some of the labels in
/// this demo come from the filesystem, and a filename containing an `ESC` must
/// not be able to close the link and relabel the rest of the row.
///
/// Falls back to unlinked text if the URI or the id cannot be sanitised, which is
/// the same degradation a terminal without OSC 8 produces — better than a step
/// that aborts over an awkward filename.
fn render_link(theme: &Theme, uri: &str, text: &str, id: Option<&str>) -> String {
    let built = Hyperlink::new(uri).and_then(|link| match id {
        Some(id) => link.with_id(id),
        None => Ok(link),
    });
    let painted = theme.paint(&sanitize_text(text), Ink::Link);
    match built {
        Ok(link) => format!("{}{painted}{}", link.open(), link.close()),
        Err(_) => painted,
    }
}

/// Up to `limit` entries of the current directory, as `(label, absolute path)`.
///
/// Real files, because a `file://` link to something that does not exist teaches
/// nothing. Sorted so the list is stable between steps, and directories get a
/// trailing slash so the label says what it is.
fn list_directory(limit: usize) -> Vec<(String, String)> {
    let Ok(current) = std::env::current_dir() else {
        return Vec::new();
    };
    let Ok(read) = std::fs::read_dir(&current) else {
        return Vec::new();
    };
    let mut entries: Vec<(String, String)> = read
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            let path = entry.path().to_string_lossy().into_owned();
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            Some((if is_dir { format!("{name}/") } else { name }, path))
        })
        .collect();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries.truncate(limit);
    entries
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The shared resolver, spelled out once so the tests read like the command
    /// line does.
    fn select(only: Option<&str>, step: Option<usize>) -> Result<crate::features::Selection> {
        crate::features::select_steps(&step_names(), only, step)
    }

    #[test]
    fn every_step_has_a_name_and_a_title() {
        for step in STEPS {
            assert!(!step.title.is_empty());
            assert!(
                step.title.chars().count() < 60,
                "title too long for the header: {:?}",
                step.title
            );
            assert!(!step.name.is_empty());
            assert!(
                step.name
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch == '-'),
                "step name `{}` should be lowercase kebab-case so it is easy to type",
                step.name
            );
        }
    }

    #[test]
    fn step_names_are_unique_so_only_is_unambiguous() {
        let mut names = step_names();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "two steps share a name");
    }

    #[test]
    fn only_selects_a_single_step_by_name() {
        // The case this feature exists for: someone debugging why a wrapped link
        // does not hover as one unit wants the `id` step and nothing else.
        let selection = select(Some("id"), None).unwrap();
        assert_eq!(STEPS[selection.start].name, "id");
        assert!(
            selection.single,
            "--only must not run on into the next step"
        );

        // Case-insensitive, and unambiguous prefixes work.
        assert_eq!(
            STEPS[select(Some("FILE"), None).unwrap().start].name,
            "file"
        );
        assert_eq!(
            STEPS[select(Some("unsup"), None).unwrap().start].name,
            "unsupported"
        );
    }

    #[test]
    fn a_misspelt_only_names_the_real_steps() {
        let error = select(Some("no-such-step"), None).unwrap_err().to_string();
        for step in STEPS {
            assert!(error.contains(step.name), "{error}");
        }
    }

    #[test]
    fn step_numbers_on_the_command_line_are_one_based_and_clamp() {
        for (given, expected) in [(1_usize, 0_usize), (2, 1), (9, 8)] {
            let selection = select(None, Some(given)).unwrap();
            assert_eq!(selection.start, expected);
            assert!(!selection.single, "--step carries on through the script");
        }
        assert_eq!(select(None, Some(0)).unwrap().start, 0);
        assert_eq!(select(None, Some(999)).unwrap().start, STEPS.len() - 1);
        assert_eq!(select(None, None).unwrap().start, 0);
    }

    #[test]
    fn the_demo_ends_by_leaving_the_alternate_screen() {
        let last = &STEPS[STEPS.len() - 1];
        assert_eq!(last.name, "restore");
        assert!(last.title.contains("back"), "last step is {:?}", last.title);
        assert!(
            std::ptr::fn_addr_eq(last.run, step_leave as StepFn),
            "the final step must be the one that restores the terminal"
        );
    }

    #[test]
    fn advance_maps_onto_flow_the_way_the_footer_promises() {
        assert_eq!(Flow::from(Advance::Next), Flow::Next);
        assert_eq!(Flow::from(Advance::Prev), Flow::Prev);
        assert_eq!(Flow::from(Advance::Quit), Flow::Quit);
    }

    #[test]
    fn a_rendered_link_never_lets_its_label_out_of_the_link() {
        let theme = Theme::plain();
        // A filename that tries to close the link and relabel the rest of the row.
        let hostile = "invoice.pdf\u{1b}]8;;https://evil.example\u{1b}\\";
        let rendered = render_link(&theme, "https://example.com", hostile, None);

        // Four escape bytes, all ours: the open pair and the close pair.
        assert_eq!(rendered.matches('\u{1b}').count(), 4);
        assert!(rendered.contains('\u{241b}'), "{rendered:?}");
        assert!(rendered.starts_with("\u{1b}]8;;https://example.com\u{1b}\\"));
        assert!(rendered.ends_with("\u{1b}]8;;\u{1b}\\"));
    }

    #[test]
    fn a_rendered_link_degrades_to_plain_text_when_it_cannot_be_built() {
        let theme = Theme::plain();
        // An unusable id must not lose the text, only the link.
        let rendered = render_link(&theme, "https://example.com", "label", Some("bad;id"));
        assert_eq!(rendered, "label");
        // And so must an empty URI.
        assert_eq!(render_link(&theme, "", "label", None), "label");
    }

    #[test]
    fn listing_a_directory_never_panics_and_stays_within_its_limit() {
        let entries = list_directory(3);
        assert!(entries.len() <= 3);
        for (label, path) in &entries {
            assert!(!label.is_empty());
            assert!(path.starts_with('/') || path.contains(':'), "{path:?}");
            // Every path must survive being turned into a URI.
            let uri = file_uri("myhost", path);
            assert!(uri.starts_with("file://myhost"));
            assert!(!uri.contains(' '));
            assert!(sanitize_uri(&uri).is_ok());
        }
    }

    #[test]
    fn directory_listings_are_sorted_and_hide_dotfiles() {
        let entries = list_directory(50);
        let labels: Vec<&str> = entries.iter().map(|(label, _)| label.as_str()).collect();
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        assert_eq!(labels, sorted);
        assert!(!labels.iter().any(|label| label.starts_with('.')));
    }
}
