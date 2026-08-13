//! Interaction primitives for `dira onboard`.
//!
//! Line-based, no new dependency. `dialoguer`/`inquire` are not in the
//! workspace, and a full-screen TUI would be the wrong shape for a linear
//! flow that has to work over SSH, inside `--print`, and under `--yes`.
//!
//! Everything goes through the [`Ui`] trait so the wizard's decision logic is
//! testable without a terminal: [`Interactive`] reads real stdin, [`Auto`]
//! answers from flags, and [`ScriptedUi`] (tests) replays a fixed script and
//! records what it was asked.

use std::io::{IsTerminal, Write};

/// How the wizard asks questions.
///
/// Every method returns the *decision*, never an `Option`/`Result` the caller
/// has to interpret — a non-interactive `Ui` is expected to have an answer
/// for everything, which is what makes `--yes` and `--print` total.
pub(crate) trait Ui {
    /// A yes/no question. `default` is what Enter (or a non-interactive Ui)
    /// selects.
    fn confirm(&mut self, question: &str, default: bool) -> bool;

    /// A free-text answer. Empty means "skip" at every call site, which is
    /// why this returns `String` rather than `Option<String>` — the caller
    /// decides what empty means for it.
    fn line(&mut self, question: &str) -> String;

    /// Narration. Routed through the trait rather than `println!` so a test
    /// Ui can assert on what the user was told — particularly the knowledge
    /// consent text, which must name the content it sends.
    fn say(&mut self, line: &str);
}

/// Reads real stdin.
pub(crate) struct Interactive;

impl Ui for Interactive {
    fn confirm(&mut self, question: &str, default: bool) -> bool {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        print!("{question} {hint} ");
        let _ = std::io::stdout().flush();
        let mut buf = String::new();
        // EOF (a closed stdin) yields Ok(0) and an empty buffer, which lands
        // on `default` — the same answer Enter gives. A wizard that hung or
        // errored here would break `dira onboard </dev/null`, which is a
        // reasonable thing to run.
        if std::io::stdin().read_line(&mut buf).is_err() {
            return default;
        }
        match buf.trim().to_ascii_lowercase().as_str() {
            "" => default,
            "y" | "yes" => true,
            "n" | "no" => false,
            _ => default,
        }
    }

    fn line(&mut self, question: &str) -> String {
        print!("{question}");
        let _ = std::io::stdout().flush();
        let mut buf = String::new();
        if std::io::stdin().read_line(&mut buf).is_err() {
            return String::new();
        }
        buf.trim().to_string()
    }

    fn say(&mut self, line: &str) {
        println!("{line}");
    }
}

/// Answers without asking — `--yes`, and every flag-forced path.
///
/// `confirm` returns each question's own default, which is why the defaults
/// encoded at the call sites are the real specification of what `--yes`
/// does. `line` always returns empty, i.e. "skip": the only free-text
/// question is the device link code, and there is no way to invent one.
pub(crate) struct Auto {
    /// Whether to echo the questions being auto-answered. On under `--yes`
    /// (the user should see what was decided for them), off under `--print`
    /// (which renders its own plan).
    pub narrate: bool,
}

impl Ui for Auto {
    fn confirm(&mut self, question: &str, default: bool) -> bool {
        if self.narrate {
            println!("{question} {} (auto)", if default { "yes" } else { "no" });
        }
        default
    }

    fn line(&mut self, _question: &str) -> String {
        String::new()
    }

    fn say(&mut self, line: &str) {
        if self.narrate {
            println!("{line}");
        }
    }
}

/// Whether this process can actually hold a conversation.
///
/// Both ends matter: stdin must be a terminal to read an answer, and stdout
/// must be one for the question to be seen. A pipeline like
/// `dira onboard | tee log` has a terminal stdin but a redirected stdout, and
/// prompting there strands the user in front of an invisible question.
pub(crate) fn is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

#[cfg(test)]
pub(crate) mod test_ui {
    use super::Ui;

    /// Replays a fixed script and records every question asked.
    pub(crate) struct ScriptedUi {
        /// Answers for `confirm`, consumed in order. Exhausting it falls back
        /// to the question's own default, so a test only scripts the answers
        /// it cares about.
        pub confirms: Vec<bool>,
        /// Answers for `line`, same contract (falls back to empty).
        pub lines: Vec<String>,
        /// Every question and narration line, in order.
        pub asked: Vec<String>,
    }

    impl ScriptedUi {
        pub(crate) fn new() -> Self {
            Self {
                confirms: Vec::new(),
                lines: Vec::new(),
                asked: Vec::new(),
            }
        }

        pub(crate) fn with_confirms(mut self, v: &[bool]) -> Self {
            self.confirms = v.to_vec();
            self
        }

        pub(crate) fn with_lines(mut self, v: &[&str]) -> Self {
            self.lines = v.iter().map(|s| s.to_string()).collect();
            self
        }

        /// Everything the user was shown, joined — for asserting that a
        /// particular disclosure actually appeared.
        pub(crate) fn transcript(&self) -> String {
            self.asked.join("\n")
        }
    }

    impl Ui for ScriptedUi {
        fn confirm(&mut self, question: &str, default: bool) -> bool {
            self.asked.push(question.to_string());
            if self.confirms.is_empty() {
                default
            } else {
                self.confirms.remove(0)
            }
        }

        fn line(&mut self, question: &str) -> String {
            self.asked.push(question.to_string());
            if self.lines.is_empty() {
                String::new()
            } else {
                self.lines.remove(0)
            }
        }

        fn say(&mut self, line: &str) {
            self.asked.push(line.to_string());
        }
    }
}
