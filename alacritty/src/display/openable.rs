//! Answers to "would this text open something?", asked of the program that
//! would do the opening.
//!
//! The question cannot be answered here. What a relative path names depends on
//! the directory of the pane that printed it, and a terminal multiplexer's
//! panes are invisible from out here; only the program running them knows.
//!
//! Asking costs a process, so answers are remembered. A remembered answer is
//! used past its `ANSWER_LIFETIME` while a fresh one is fetched, since the
//! alternative — dropping the highlight until the answer comes back — shows
//! the user a flicker every time it expires.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use log::warn;

use crate::config::ui_config::Program;
use crate::event::{EventProxy, EventType};

/// How long an answer is used before it is asked again.
const ANSWER_LIFETIME: Duration = Duration::from_secs(1);

/// How long an answer nobody has asked about is kept.
const ANSWER_RETENTION: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct Openable {
    answers: HashMap<String, (bool, Instant)>,
    unanswered: Vec<String>,
    waiting: bool,
    event_proxy: Option<EventProxy>,
}

impl Openable {
    /// Set the proxy that answers are delivered through.
    pub fn set_event_proxy(&mut self, event_proxy: EventProxy) {
        self.event_proxy = Some(event_proxy);
    }

    /// Whether `program` would open `text`, or `None` until it has said.
    pub fn ask(&mut self, program: &Program, text: &str) -> Option<bool> {
        // Answers come back a line at a time, which cannot carry a newline.
        // Nothing spanning a line break is a path anyway.
        if text.contains('\n') {
            return Some(false);
        }

        let answer = self.answers.get(text).map(|(answer, at)| (*answer, at.elapsed()));
        match answer {
            Some((answer, age)) if age < ANSWER_LIFETIME => Some(answer),
            answer => {
                self.enqueue(program, text.to_owned());
                answer.map(|(answer, _)| answer)
            },
        }
    }

    /// Record the answers to a batch of questions. Questions asked while that
    /// batch was in flight go out with the next one.
    pub fn answer(&mut self, answers: Vec<(String, bool)>) {
        let now = Instant::now();
        self.unanswered.retain(|text| !answers.iter().any(|(answered, _)| answered == text));
        self.answers.extend(answers.into_iter().map(|(text, answer)| (text, (answer, now))));
        self.answers.retain(|_, (_, at)| at.elapsed() < ANSWER_RETENTION);
        self.waiting = false;
    }

    fn enqueue(&mut self, program: &Program, text: String) {
        if !self.unanswered.contains(&text) {
            self.unanswered.push(text);
        }
        self.ask_program(program);
    }

    /// Put every question asked since the last batch to the program at once.
    ///
    /// One batch is in flight at a time, so a screen full of matches costs two
    /// processes rather than one per match.
    fn ask_program(&mut self, program: &Program) {
        let (Some(event_proxy), false) = (self.event_proxy.clone(), self.waiting) else {
            return;
        };
        if self.unanswered.is_empty() {
            return;
        }

        let questions = std::mem::take(&mut self.unanswered);
        let program = program.clone();
        self.waiting = true;
        std::thread::spawn(move || {
            let answers = ask_program(&program, questions);
            event_proxy.send_event(EventType::OpenableAnswers(answers));
        });
    }
}

/// Ask `program` which of `questions` it would open, taking silence for "none".
fn ask_program(program: &Program, questions: Vec<String>) -> Vec<(String, bool)> {
    let output = Command::new(program.program())
        .args(program.args())
        .arg("--")
        .args(&questions)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let stdout = match output {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Err(err) => {
            warn!("Unable to run {:?}: {}", program.program(), err);
            String::new()
        },
    };
    let openable: Vec<&str> = stdout.lines().collect();
    questions
        .into_iter()
        .map(|question| {
            let answer = openable.contains(&question.as_str());
            (question, answer)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without a proxy to deliver answers through, as in a test, nothing is
    /// asked and every question is unanswered.
    #[test]
    fn a_question_has_no_answer_until_it_is_answered() {
        let program = Program::Just("true".into());
        let mut openable = Openable::default();

        assert_eq!(openable.ask(&program, "src/main.rs"), None);

        openable.answer(vec![("src/main.rs".into(), true)]);
        assert_eq!(openable.ask(&program, "src/main.rs"), Some(true));
    }

    #[test]
    fn a_refused_answer_is_remembered() {
        let program = Program::Just("true".into());
        let mut openable = Openable::default();

        openable.answer(vec![("prose".into(), false)]);
        assert_eq!(openable.ask(&program, "prose"), Some(false));
    }

    #[test]
    fn a_question_is_asked_once_per_batch() {
        let program = Program::Just("true".into());
        let mut openable = Openable::default();

        openable.ask(&program, "src/main.rs");
        openable.ask(&program, "src/main.rs");
        openable.ask(&program, "app.py");

        assert_eq!(openable.unanswered, ["src/main.rs", "app.py"]);
    }

    #[test]
    fn text_spanning_a_line_break_is_refused_without_asking() {
        let program = Program::Just("true".into());
        let mut openable = Openable::default();

        assert_eq!(openable.ask(&program, "src/ma\nin.rs"), Some(false));
        assert!(openable.unanswered.is_empty());
    }

    #[test]
    fn an_answer_the_program_did_not_give_is_no() {
        let program = Program::WithArgs {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                r#"for arg in "$@"; do case $arg in *.rs) echo "$arg";; esac; done"#.into(),
                "sh".into(),
            ],
        };

        let answers = ask_program(&program, vec!["src/main.rs".into(), "app.py".into()]);

        assert_eq!(answers, [("src/main.rs".into(), true), ("app.py".into(), false)]);
    }

    #[test]
    fn a_program_which_cannot_be_run_opens_nothing() {
        let program = Program::Just("/nonexistent/program".into());

        let answers = ask_program(&program, vec!["src/main.rs".into()]);

        assert_eq!(answers, [("src/main.rs".into(), false)]);
    }
}
