use std::{
    io::{IsTerminal, Write},
    time::{Duration, Instant},
};

use indicatif::{ProgressBar, ProgressStyle};
use owo_colors::{OwoColorize, Stream};
use serde::Serialize;

use crate::cli::GlobalArgs;

/// Everything the run produces, in the order it happens. The human reporter
/// renders these on stderr, the JSON reporters write them to stdout.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Context {
        staged_files: usize,
        included: usize,
        withheld: Vec<String>,
        added: u64,
        deleted: u64,
        diff_bytes: usize,
        commits_used: usize,
        model: String,
    },
    Tool {
        name: String,
        arguments: serde_json::Value,
        returned_bytes: usize,
    },
    /// A reasoning model's thinking, which arrives long before its answer.
    /// Shown dimmed on stderr so a twenty second wait is not a blank screen.
    Reasoning {
        text: String,
    },
    Delta {
        text: String,
    },
    /// Progress for an answer that cannot be shown as it streams, because a
    /// half-written JSON object is not readable.
    Writing {
        bytes: usize,
    },
    Message {
        subject: String,
        body: String,
    },
    Branch {
        candidates: Vec<crate::branch::Suggestion>,
    },
    Squash {
        groups: Vec<crate::squash::Group>,
    },
    Renamed {
        from: String,
        to: String,
    },
    Committed {
        sha: String,
    },
    Error {
        code: &'static str,
        message: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Human,
    Json,
    JsonStream,
}

pub struct Reporter {
    mode: Mode,
    quiet: bool,
    timing: bool,
    started: Instant,
    first_token: Option<Duration>,
    tool_calls: u32,
    spinner: Option<ProgressBar>,
    folded: serde_json::Map<String, serde_json::Value>,
    tools: Vec<serde_json::Value>,
    streaming_line: bool,
    reasoning_line: bool,
    first_reasoning: Option<Duration>,
    reasoning_column: usize,
    show_thinking: bool,
    thinking: String,
    thought_for: Option<Duration>,
}

impl Reporter {
    pub fn new(args: &GlobalArgs) -> Self {
        let mode = if args.json {
            Mode::Json
        } else if args.json_stream {
            Mode::JsonStream
        } else {
            Mode::Human
        };

        Self {
            mode,
            quiet: args.quiet,
            timing: args.timing,
            started: Instant::now(),
            first_token: None,
            tool_calls: 0,
            spinner: None,
            folded: serde_json::Map::new(),
            tools: Vec::new(),
            streaming_line: false,
            reasoning_line: false,
            first_reasoning: None,
            reasoning_column: 0,
            show_thinking: args.show_thinking,
            thinking: String::new(),
            thought_for: None,
        }
    }

    /// Progress belongs on stderr, and only when a person is watching.
    fn shows_progress(&self) -> bool {
        self.mode == Mode::Human && !self.quiet
    }

    fn interactive(&self) -> bool {
        self.shows_progress() && std::io::stderr().is_terminal()
    }

    pub fn start(&mut self, message: &str) {
        if !self.interactive() {
            if self.shows_progress() {
                eprintln!("{message}");
            }

            return;
        }

        let spinner = ProgressBar::new_spinner();

        spinner.set_style(
            ProgressStyle::with_template("  {spinner:.magenta} {msg} {elapsed:.dim}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner())
                .tick_strings(&["✶", "✸", "✹", "✺", "✹", "✷", "✵"]),
        );

        spinner.enable_steady_tick(Duration::from_millis(110));
        spinner.set_message(message.to_owned());

        self.spinner = Some(spinner);
    }

    pub fn step(&self, message: &str) {
        match &self.spinner {
            Some(spinner) => spinner.set_message(message.to_owned()),
            None if self.shows_progress() => eprintln!("{message}"),
            None => {}
        }
    }

    pub fn stop(&mut self) {
        if let Some(spinner) = self.spinner.take() {
            spinner.finish_and_clear();
        }
    }

    pub fn event(&mut self, event: Event) {
        if let Event::Delta { .. } | Event::Writing { .. } = event {
            if self.first_token.is_none() {
                self.first_token = Some(self.started.elapsed());
            }
        }

        if let Event::Reasoning { .. } = event {
            if self.first_reasoning.is_none() {
                self.first_reasoning = Some(self.started.elapsed());
            }
        }

        if let Event::Tool { .. } = event {
            self.tool_calls += 1;
        }

        match self.mode {
            Mode::JsonStream => self.write_json_line(&event),
            Mode::Json => self.fold(&event),
            Mode::Human => self.render(&event),
        }
    }

    fn write_json_line(&self, event: &Event) {
        let mut stdout = std::io::stdout().lock();

        if let Ok(line) = serde_json::to_string(event) {
            let _ = writeln!(stdout, "{line}");
            let _ = stdout.flush();
        }
    }

    /// The single-object shape is the same events folded into named fields.
    /// Deltas are dropped, because the message they build up is also here.
    fn fold(&mut self, event: &Event) {
        let Ok(value) = serde_json::to_value(event) else {
            return;
        };

        match event {
            // Deltas and reasoning build up something already reported whole.
            Event::Delta { .. } | Event::Reasoning { .. } | Event::Writing { .. } => {}
            Event::Tool { .. } => {
                self.tools.push(value);
                let tools = serde_json::Value::Array(self.tools.clone());
                self.folded.insert("tools".to_owned(), tools);
            }
            Event::Context { .. } => {
                self.folded.insert("context".to_owned(), value);
            }
            Event::Message { .. } => {
                self.folded.insert("message".to_owned(), value);
            }
            Event::Branch { .. } => {
                self.folded.insert("branch".to_owned(), value);
            }
            Event::Squash { .. } => {
                self.folded.insert("squash".to_owned(), value);
            }
            Event::Renamed { .. } => {
                self.folded.insert("renamed".to_owned(), value);
            }
            Event::Committed { .. } => {
                self.folded.insert("committed".to_owned(), value);
            }
            Event::Error { .. } => {
                self.folded.insert("error".to_owned(), value);
            }
        }
    }

    fn render(&mut self, event: &Event) {
        match event {
            // A printed line, not a spinner message. It says which model and
            // how much context produced the answer, and that is worth keeping
            // on screen rather than clearing when the spinner stops.
            Event::Context {
                staged_files,
                included,
                withheld,
                added,
                deleted,
                commits_used,
                model,
                ..
            } => {
                if !self.shows_progress() {
                    return;
                }

                self.stop();

                let files = if included == staged_files {
                    format!("{staged_files} files")
                } else {
                    format!("{included} of {staged_files} files")
                };

                eprintln!(
                    "  {}  {} {}  {}",
                    files.if_supports_color(Stream::Stderr, |text| text.bold()),
                    format!("+{added}").if_supports_color(Stream::Stderr, |text| text.green()),
                    format!("-{deleted}").if_supports_color(Stream::Stderr, |text| text.red()),
                    format!("{commits_used} commits of context, {model}")
                        .if_supports_color(Stream::Stderr, |text| text.dimmed())
                );

                if !withheld.is_empty() {
                    eprintln!(
                        "  {} {}",
                        "withheld".if_supports_color(Stream::Stderr, |text| text.yellow()),
                        withheld
                            .join(", ")
                            .if_supports_color(Stream::Stderr, |text| text.dimmed())
                    );
                }
            }

            Event::Tool {
                name,
                arguments,
                returned_bytes,
            } => {
                if !self.shows_progress() {
                    return;
                }

                // A tool call is a decision the model made about your diff, so
                // it gets a line of its own rather than a spinner flicker.
                self.end_reasoning();
                self.stop();

                eprintln!(
                    "{} {name} {arguments} {}",
                    "tool".if_supports_color(Stream::Stderr, |text| text.cyan()),
                    format!("({returned_bytes} bytes)")
                        .if_supports_color(Stream::Stderr, |text| text.dimmed())
                );

                self.start("reading it back");
            }

            // Kept whichever way it is rendered, so the picker can offer to
            // show it after the fact.
            Event::Reasoning { text } => {
                self.thinking.push_str(text);

                if !self.shows_progress() {
                    return;
                }

                // Collapsed, this is a spinner message. Without a spinner to
                // replace it, every chunk would print its own line.
                if !self.show_thinking {
                    if self.spinner.is_some() {
                        self.step(musing(self.started.elapsed()));
                    }

                    return;
                }

                if !self.reasoning_line {
                    self.stop();
                    self.reasoning_line = true;
                }

                // Indented to sit under the run, and dimmed because it is
                // working out rather than an answer.
                for (index, part) in text.split_inclusive('\n').enumerate() {
                    if index > 0 || self.reasoning_column == 0 {
                        eprint!("  ");
                    }

                    eprint!(
                        "{}",
                        part.if_supports_color(Stream::Stderr, |text| text.dimmed())
                    );

                    self.reasoning_column = if part.ends_with('\n') {
                        0
                    } else {
                        self.reasoning_column + part.len()
                    };
                }

                let _ = std::io::stderr().flush();
            }

            // Only worth showing where it can replace the previous count.
            // Without a spinner every chunk would be its own line.
            Event::Writing { bytes } => {
                if self.spinner.is_some() {
                    self.end_reasoning();
                    self.step(&format!("writing the answer, {bytes} bytes"));
                }
            }

            Event::Delta { text } => {
                if !self.shows_progress() {
                    return;
                }

                self.end_reasoning();

                if !self.streaming_line {
                    self.stop();
                    self.streaming_line = true;
                }

                // Dimmed, because this is the draft being written. The
                // message is rendered properly once it is complete.
                eprint!(
                    "{}",
                    text.if_supports_color(Stream::Stderr, |text| text.dimmed())
                );
                let _ = std::io::stderr().flush();
            }

            Event::Message { .. } | Event::Branch { .. } | Event::Squash { .. } => {}

            Event::Renamed { from, to } => {
                if self.shows_progress() {
                    eprintln!(
                        "renamed {from} to {}",
                        to.if_supports_color(Stream::Stderr, |text| text.green())
                    );
                }
            }

            Event::Committed { sha } => {
                if self.shows_progress() {
                    eprintln!(
                        "committed {}",
                        sha.if_supports_color(Stream::Stderr, |text| text.green())
                    );
                }
            }

            Event::Error { message, .. } => {
                self.stop();
                eprintln!(
                    "{} {message}",
                    "auto-commit:".if_supports_color(Stream::Stderr, |text| text.red())
                );
            }
        }
    }

    /// Whether the model has thought at all, and for how long. The picker
    /// offers to print it only when there is something to print.
    pub fn thinking(&self) -> Option<&str> {
        (!self.thinking.trim().is_empty()).then_some(self.thinking.as_str())
    }

    /// Turned on from the config once it has been read, which is after this
    /// was built from the flags.
    pub fn set_show_thinking(&mut self, show: bool) {
        self.show_thinking = self.show_thinking || show;
    }

    /// Closes the thinking block. Collapsed, that means replacing it with how
    /// long it took; expanded, ending the last line.
    fn end_reasoning(&mut self) {
        if self.first_reasoning.is_some() && self.thought_for.is_none() {
            self.thought_for = Some(self.started.elapsed());

            if self.reasoning_line {
                eprintln!();
            }

            self.reasoning_line = false;

            if self.shows_progress() {
                self.stop();
                eprintln!(
                    "  {}",
                    format!(
                        "thought for {:.1}s",
                        self.thought_for.unwrap_or_default().as_secs_f32()
                    )
                    .if_supports_color(Stream::Stderr, |text| text.dimmed())
                );
            }
        }

        if self.reasoning_line {
            eprintln!();
            self.reasoning_line = false;
        }
    }

    /// Clears the streamed preview so the final rendering is not printed twice.
    pub fn end_stream(&mut self) {
        if self.streaming_line {
            eprintln!();
            self.streaming_line = false;
        }
    }

    /// Closes every open line and clears the spinner, so whatever prints next
    /// starts at column zero. A prompt that lands halfway along a spinner line
    /// is the most obvious way a CLI looks broken.
    pub fn settle(&mut self) {
        self.stop();
        self.end_reasoning();
        self.end_stream();
    }

    pub fn is_human(&self) -> bool {
        self.mode == Mode::Human
    }

    /// The payload, and the only thing that ever reaches stdout in human mode.
    pub fn payload(&self, text: &str) {
        if self.mode != Mode::Human {
            return;
        }

        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{text}");
        let _ = stdout.flush();
    }

    pub fn finish(&mut self) {
        self.stop();

        if self.mode == Mode::Json {
            let object = serde_json::Value::Object(std::mem::take(&mut self.folded));

            if let Ok(line) = serde_json::to_string(&object) {
                println!("{line}");
            }
        }

        if self.timing {
            let ms = |at: Option<Duration>| {
                at.map(|elapsed| format!("{} ms", elapsed.as_millis()))
                    .unwrap_or_else(|| "none".to_owned())
            };

            eprintln!(
                "timing: first thought {}, first token {}, {} tool calls, {} ms total",
                ms(self.first_reasoning),
                ms(self.first_token),
                self.tool_calls,
                self.started.elapsed().as_millis()
            );
        }
    }
}

/// A word for the spinner while the model thinks. It changes every few
/// seconds so a long wait reads as something happening rather than a stall.
fn musing(elapsed: Duration) -> &'static str {
    const WORDS: [&str; 6] = [
        "thinking",
        "pondering",
        "mulling it over",
        "weighing it up",
        "still pondering",
        "nearly there, probably",
    ];

    WORDS[(elapsed.as_secs() as usize / 3) % WORDS.len()]
}
