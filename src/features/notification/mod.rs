//! Desktop notifications from the terminal.
//!
//! Three escape sequences do this job, they were designed independently, and
//! they are not interchangeable:
//!
//! ```text
//! OSC 9   ; <text> ST                            iTerm2, before 2010
//! OSC 777 ; notify ; <title> ; <body> ST         urxvt's notify extension
//! OSC 99  ; <key>=<value>: … ; <payload> ST      kitty, 2020
//! ```
//!
//! OSC 9 has one field. OSC 777 has two. OSC 99 has titles, bodies, urgency,
//! ids, replacement of an earlier notification, icons, buttons, sounds,
//! auto-expiry, activation reports and chunked payloads, and is the only one that
//! can be asked whether it is there.
//!
//! Because all three are OSC strings, a terminal that has never heard of the one
//! you sent consumes it up to its terminator and throws it away. Nothing is
//! printed, nothing is corrupted, and no state is left behind. That is the
//! property that makes this feature safe to use unconditionally — the only
//! failure mode is that no notification appears.
//!
//! The modules are:
//!
//! * [`sequence`] — sanitizing user text, and building all three sequences.
//! * [`probe`] — which protocols this terminal plausibly speaks, and how firmly.
//! * [`demo`] — the step-by-step walkthrough.
//!
//! Choosing between the three lives here, in [`resolve`], because choosing is the
//! thing a user most needs made visible.

pub mod demo;
pub mod probe;
pub mod sequence;

use std::{io::Write as _, process::ExitCode};

use anyhow::Result;

use crate::{
    cli::{NotificationAction, NotificationCommand, NotifyProtocol},
    db::Catalog,
    features::text_operand,
    term::{Multiplexer, Term, pretty},
    theme::{Ink, Theme},
};

use self::{
    probe::ProbeOutcome,
    sequence::{Protocol, Request, Urgency, build, safe_id},
};

/// The feature id in the database. The subcommand is `notification`.
pub const FEATURE_ID: &str = "notifications";

/// The protocol used when nothing is known about the terminal.
///
/// OSC 777 rather than OSC 9 for one reason: it can carry a title. The two are
/// implemented by about the same number of terminals, so there is no reach to
/// trade away, and losing the title is a real loss. Being wrong costs nothing —
/// an OSC string a terminal does not recognise is discarded silently.
const FALLBACK: Protocol = Protocol::Osc777;

/// Which protocol was chosen, and why.
///
/// The reason is not decoration. A tool that quietly picked OSC 9 and therefore
/// quietly dropped your title is exactly the kind of invisible behaviour this
/// program exists to expose, so every path that resolves a protocol produces one
/// of these and every path that uses one prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// The protocol to use.
    pub protocol: Protocol,
    /// Why this one, in one sentence.
    pub reason: String,
    /// Whether the user named it rather than leaving it to `auto`.
    pub explicit: bool,
    /// Set when the user named a protocol this terminal is not believed to
    /// implement.
    pub warning: Option<String>,
}

/// Picks a protocol.
///
/// With `--protocol` set to anything but `auto`, the answer is the user's and the
/// only work left is to say whether the terminal is believed to implement it.
///
/// `auto` walks [`Protocol::RICHEST_FIRST`] and takes the first protocol the
/// probe believes in, so a measured OSC 99 wins, then an inferred OSC 99, then
/// OSC 777, then OSC 9. When nothing is believed in — either because the terminal
/// was not recognised, or because it was recognised and implements none of them —
/// it falls back to [`FALLBACK`] and says so, because sending is free and
/// refusing to send would be worse.
#[must_use]
pub fn resolve(requested: NotifyProtocol, outcome: &ProbeOutcome) -> Choice {
    if let Some(protocol) = explicit_protocol(requested) {
        let warning = outcome.verdict(protocol).and_then(|verdict| {
            if verdict.supported() {
                None
            } else {
                Some(format!(
                    "you asked for {}, but {} — sending it anyway, since an unrecognised OSC \
                     string is discarded silently",
                    protocol.label(),
                    verdict.reason
                ))
            }
        });
        return Choice {
            protocol,
            reason: format!("you asked for it with --protocol {}", protocol.flag()),
            explicit: true,
            warning,
        };
    }

    for &protocol in &Protocol::RICHEST_FIRST {
        if let Some(verdict) = outcome.verdict(protocol)
            && verdict.supported()
        {
            return Choice {
                protocol,
                reason: format!(
                    "the richest protocol this terminal supports — {} ({})",
                    verdict.reason,
                    verdict.confidence.label()
                ),
                explicit: false,
                warning: None,
            };
        }
    }

    // Nothing is a yes. Which of the three reasons applies changes what a user
    // should do about it, so they get told apart rather than lumped together.
    let reason = if let Some(multiplexer) = outcome.multiplexer {
        format!(
            "{} is in the way and filters what it does not recognise, so nothing can be \
             established about the terminal beyond it — falling back to {}, which will work if \
             passthrough is on",
            multiplexer.name(),
            FALLBACK.label()
        )
    } else if outcome.identity.known.is_some() {
        // "Not recorded as implementing any of the three" is only a sentence that
        // means anything when we know which terminal we are talking about.
        format!(
            "this terminal is not recorded as implementing any of the three, so falling back to \
             {}; if the record is out of date the notification appears anyway, and if it is right \
             nothing happens",
            FALLBACK.label()
        )
    } else {
        format!(
            "nothing identified this terminal, so falling back to {} — the widest-reaching \
             protocol that can still carry a title",
            FALLBACK.label()
        )
    };
    Choice {
        protocol: FALLBACK,
        reason,
        explicit: false,
        warning: None,
    }
}

/// Maps the command-line protocol onto a wire protocol, or `None` for `auto`.
fn explicit_protocol(requested: NotifyProtocol) -> Option<Protocol> {
    match requested {
        NotifyProtocol::Auto => None,
        NotifyProtocol::Kitty => Some(Protocol::Osc99),
        NotifyProtocol::Osc9 => Some(Protocol::Osc9),
        NotifyProtocol::Osc777 => Some(Protocol::Osc777),
    }
}

/// Entry point for `escapade notification …`.
///
/// # Errors
///
/// Propagates whatever the chosen action fails with.
pub fn run(catalog: &Catalog, command: &NotificationCommand, no_color: bool) -> Result<ExitCode> {
    let theme = Theme::new(no_color);
    match &command.action {
        Some(NotificationAction::Explain { no_matrix }) => {
            let feature = catalog.feature(FEATURE_ID)?;
            crate::features::explain::render(&theme, catalog, feature, !no_matrix);
            Ok(ExitCode::SUCCESS)
        }
        // The only path whose exit status carries information.
        Some(NotificationAction::Probe { format }) => probe::report(&theme, *format),
        Some(NotificationAction::Demo {
            force,
            only,
            step,
            list,
        }) => {
            if *list {
                demo::list_steps(&theme);
            } else {
                demo::run(&theme, *force, only.as_deref(), *step)?;
            }
            Ok(ExitCode::SUCCESS)
        }
        None => {
            raise(&theme, command)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Raises the notification described on the command line.
///
/// The body comes from the arguments or from standard input, so the useful shape
/// works:
///
/// ```sh
/// make 2>&1 | tail -5 | escapade notification --title 'build failed'
/// ```
///
/// Piped build output is far more likely to contain a stray `ESC` or `BEL` than a
/// hand-typed argument ever was, which makes this the more important of the two
/// inputs for [`sequence::Safe`] to be guarding.
///
/// # Errors
///
/// Fails if no body was given and standard input is a terminal, if standard input
/// cannot be read or is not UTF-8, or if the terminal cannot be written to.
fn raise(theme: &Theme, command: &NotificationCommand) -> Result<()> {
    let body = text_operand(&command.body)?;
    let mut request = Request::new(command.title.as_deref(), &body);
    request.urgency = command.urgency.map(Urgency::from);
    // An explicit id is kept as the user wrote it, sanitized; otherwise one is
    // minted, because OSC 99 needs an id to tie a chunked notification's escapes
    // together. Deriving it from the process id stops separate runs from
    // replacing each other's notifications.
    request.id_explicit = command.id.is_some();
    request.id = match &command.id {
        Some(id) => safe_id(id),
        None => safe_id(&format!("escapade-{}", std::process::id())),
    };

    let outcome = resolution_outcome(command.protocol)?;
    let choice = resolve(command.protocol, &outcome);
    let built = build(choice.protocol, &request);

    // Everything explanatory goes to stderr, so that `--dry-run` output and the
    // escape sequence itself stay the only things on stdout.
    let mut err = std::io::stderr();
    let _ = writeln!(
        err,
        "{} {} — {}",
        theme.paint("notification:", Ink::Muted),
        theme.paint(choice.protocol.label(), Ink::Accent),
        theme.paint(&choice.reason, Ink::Muted)
    );
    if let Some(warning) = &choice.warning {
        let _ = writeln!(err, "{} {warning}", theme.paint("notification:", Ink::Warn));
    }
    if request.altered() {
        let _ = writeln!(
            err,
            "{} {}",
            theme.paint("notification:", Ink::Warn),
            theme.paint(
                "the text contained control characters, which were removed — they would have \
                 ended the escape sequence early",
                Ink::Muted
            )
        );
    }
    for note in &built.lost {
        let _ = writeln!(err, "{} {note}", theme.paint("notification:", Ink::Warn));
    }

    if command.dry_run {
        println!();
        for sequence in &built.sequences {
            println!("  {}", theme.paint(&pretty(sequence), Ink::Code));
        }
        println!();
        let summary = format!(
            "{} escape {}, {} bytes on the wire",
            built.sequences.len(),
            if built.sequences.len() == 1 {
                "code"
            } else {
                "codes"
            },
            built.joined().len()
        );
        println!("  {}", theme.paint(&summary, Ink::Muted));
        if let Some(multiplexer) = outcome.multiplexer {
            println!(
                "  {}",
                theme.paint(
                    &format!("would be wrapped for {} on the way out", multiplexer.name()),
                    Ink::Muted
                )
            );
        }
        println!();
        return Ok(());
    }

    emit(outcome.multiplexer, &built.joined())
}

/// Works out what is known about the terminal, doing as little as possible.
///
/// A one-liner should not cost a round trip, so the terminal is only actually
/// asked anything when asking could change the answer: the user left the protocol
/// to `auto`, the environment did not identify the terminal, and there is a
/// terminal there to answer. Otherwise the environment is enough.
///
/// # Errors
///
/// Fails if the terminal cannot be set up for the query.
fn resolution_outcome(requested: NotifyProtocol) -> Result<ProbeOutcome> {
    let from_env = ProbeOutcome::from_environment();
    if requested != NotifyProtocol::Auto || from_env.identity.known.is_some() {
        return Ok(from_env);
    }
    probe::detect()
}

/// Writes the sequences to stdout, wrapping for a multiplexer if one is in the
/// way, and flushes.
///
/// # Errors
///
/// Fails if stdout does.
fn emit(multiplexer: Option<Multiplexer>, sequences: &str) -> Result<()> {
    let mut term = Term::new()?;
    match multiplexer {
        Some(multiplexer) => term.write_now(&multiplexer.wrap(sequences)),
        None => term.write_now(sequences),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::notification::{
        probe::{Identity, Measurement, infer, known_by_token},
        sequence::Osc99Support,
    };

    /// A terminal that could be asked freely but said nothing at all.
    ///
    /// No barrier reply, so nothing is measured and the support table drives all
    /// three protocols — which is what these tests are about. Handing OSC 99 a
    /// measured "no" instead would make every case here resolve past it.
    const TABLE_ONLY: Measurement = Measurement {
        answered_barrier: false,
        multiplexed: false,
        interactive: true,
    };

    /// A probe outcome standing in for a terminal identified as `token`.
    fn outcome_for(token: &str) -> ProbeOutcome {
        let identity = Identity {
            known: known_by_token(token),
            source: "TERM_PROGRAM".to_owned(),
            raw: token.to_owned(),
        };
        let mut outcome = ProbeOutcome::from_environment();
        outcome.verdicts = infer(&identity, None, TABLE_ONLY);
        outcome.identity = identity;
        // The test process is usually inside tmux and never has a tty, neither of
        // which is what these tests are about.
        outcome.multiplexer = None;
        outcome.interactive = true;
        outcome
    }

    #[test]
    fn auto_picks_osc99_where_it_exists() {
        let choice = resolve(NotifyProtocol::Auto, &outcome_for("kitty"));
        assert_eq!(choice.protocol, Protocol::Osc99);
        assert!(!choice.explicit);
        assert!(choice.reason.contains("richest"));
        assert!(choice.warning.is_none());
    }

    #[test]
    fn auto_steps_down_to_osc777_when_osc99_is_absent() {
        // Ghostty, WezTerm and Rio document OSC 777 but not OSC 99.
        for token in ["ghostty", "wezterm", "rio"] {
            let choice = resolve(NotifyProtocol::Auto, &outcome_for(token));
            assert_eq!(choice.protocol, Protocol::Osc777, "{token}");
            assert!(choice.reason.contains("OSC 777"), "{}", choice.reason);
        }
    }

    #[test]
    fn auto_steps_all_the_way_down_to_osc9() {
        // iTerm2 has only the one it invented.
        let choice = resolve(NotifyProtocol::Auto, &outcome_for("iTerm.app"));
        assert_eq!(choice.protocol, Protocol::Osc9);
    }

    #[test]
    fn auto_prefers_a_measured_answer_over_the_table() {
        let mut outcome = outcome_for("alacritty");
        let support = Osc99Support {
            payloads: vec!["title".to_owned(), "body".to_owned()],
            ..Osc99Support::default()
        };
        outcome.verdicts = infer(&outcome.identity, Some(&support), TABLE_ONLY);
        let choice = resolve(NotifyProtocol::Auto, &outcome);
        assert_eq!(choice.protocol, Protocol::Osc99);
        assert!(choice.reason.contains("measured"), "{}", choice.reason);
    }

    #[test]
    fn a_multiplexer_makes_auto_say_so_rather_than_claim_a_terminal() {
        // Inside tmux the table may be perfectly right about the outer terminal
        // and still wrong about what happens, because tmux filters the sequence.
        let mut outcome = outcome_for("kitty");
        outcome.multiplexer = Some(Multiplexer::Tmux);
        outcome.verdicts = infer(
            &outcome.identity,
            None,
            Measurement {
                answered_barrier: true,
                multiplexed: true,
                interactive: true,
            },
        );
        let choice = resolve(NotifyProtocol::Auto, &outcome);
        assert_eq!(choice.protocol, FALLBACK);
        assert!(choice.reason.contains("tmux"), "{}", choice.reason);
        assert!(choice.reason.contains("passthrough"), "{}", choice.reason);
    }

    #[test]
    fn auto_falls_back_and_explains_itself_on_an_unknown_terminal() {
        let choice = resolve(NotifyProtocol::Auto, &outcome_for("some-new-terminal"));
        assert_eq!(choice.protocol, FALLBACK);
        assert!(
            choice.reason.contains("nothing identified"),
            "{}",
            choice.reason
        );
        assert!(choice.warning.is_none(), "the fallback is not a warning");
    }

    #[test]
    fn auto_sends_something_even_where_nothing_is_supported() {
        // Alacritty implements none of the three. Sending is still right: it
        // costs nothing, and the alternative is silently doing nothing at all.
        let choice = resolve(NotifyProtocol::Auto, &outcome_for("alacritty"));
        assert_eq!(choice.protocol, FALLBACK);
        assert!(choice.reason.contains("not recorded"), "{}", choice.reason);
    }

    #[test]
    fn an_explicit_protocol_is_never_second_guessed() {
        for (flag, expected) in [
            (NotifyProtocol::Kitty, Protocol::Osc99),
            (NotifyProtocol::Osc9, Protocol::Osc9),
            (NotifyProtocol::Osc777, Protocol::Osc777),
        ] {
            let choice = resolve(flag, &outcome_for("kitty"));
            assert_eq!(choice.protocol, expected);
            assert!(choice.explicit);
            assert!(choice.reason.contains("--protocol"));
        }
    }

    #[test]
    fn an_explicit_protocol_the_terminal_lacks_is_warned_about_but_still_used() {
        // kitty explicitly refused OSC 777.
        let choice = resolve(NotifyProtocol::Osc777, &outcome_for("kitty"));
        assert_eq!(choice.protocol, Protocol::Osc777);
        let warning = choice.warning.expect("a warning");
        assert!(warning.contains("OSC 777"), "{warning}");
        assert!(warning.contains("discarded silently"), "{warning}");
    }

    #[test]
    fn every_cli_protocol_maps_onto_a_wire_protocol() {
        assert_eq!(explicit_protocol(NotifyProtocol::Auto), None);
        for flag in [
            NotifyProtocol::Kitty,
            NotifyProtocol::Osc9,
            NotifyProtocol::Osc777,
        ] {
            assert!(explicit_protocol(flag).is_some(), "{flag:?}");
        }
        // The `--protocol` spelling and the protocol's own name agree, so an
        // error message can be pasted back onto the command line.
        assert_eq!(Protocol::Osc9.flag(), "osc9");
        assert_eq!(Protocol::Osc777.flag(), "osc777");
        assert_eq!(Protocol::Osc99.flag(), "kitty");
    }

    #[test]
    fn resolution_never_returns_auto_in_disguise() {
        // Whatever happens, a concrete protocol comes out, so no caller has to
        // handle "undecided".
        for token in ["kitty", "ghostty", "iTerm.app", "alacritty", "nonesuch"] {
            for flag in [
                NotifyProtocol::Auto,
                NotifyProtocol::Kitty,
                NotifyProtocol::Osc9,
                NotifyProtocol::Osc777,
            ] {
                let choice = resolve(flag, &outcome_for(token));
                assert!(
                    Protocol::RICHEST_FIRST.contains(&choice.protocol),
                    "{token} {flag:?}"
                );
                assert!(!choice.reason.is_empty());
            }
        }
    }

    #[test]
    fn the_feature_id_matches_the_database() {
        let catalog = Catalog::load().unwrap();
        assert!(catalog.feature(FEATURE_ID).is_ok());
    }
}
