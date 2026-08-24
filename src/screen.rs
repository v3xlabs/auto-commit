use std::io::{self, Write};

use crossterm::{
    cursor,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType},
    QueueableCommand,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    Plain,
    Dim,
    Bold,
    Accent,
    Chosen,
    Added,
    Removed,
}

struct Span {
    text: String,
    ink: Ink,
}

/// One row of a frame, in as many inks as it needs. A tree row carries its
/// path in one ink and its counts in two others, so a row cannot be a single
/// string with a single colour.
pub struct Line(Vec<Span>);

pub fn line(text: impl Into<String>, ink: Ink) -> Line {
    Line(vec![Span {
        text: text.into(),
        ink,
    }])
}

impl Line {
    pub fn then(mut self, text: impl Into<String>, ink: Ink) -> Self {
        self.0.push(Span {
            text: text.into(),
            ink,
        });

        self
    }
}

/// The room a frame has to work with. Both are floored, because a frame drawn
/// for a two row terminal is worse than one that overflows it.
pub fn size() -> (usize, usize) {
    let (columns, rows) = terminal::size().unwrap_or((0, 0));

    ((columns as usize).max(40), (rows as usize).max(10))
}

/// Draws a frame in place, over the rows the previous frame used, and records
/// how many rows this one took so the next call can erase it.
pub fn paint(stderr: &mut io::Stderr, lines: &[Line], drawn: &mut u16) -> io::Result<()> {
    erase(stderr, *drawn)?;

    let (width, _) = size();

    // The rest of the tool goes through owo-colors, which checks this itself.
    // These styles are written straight to the terminal, so they check here.
    let colour = std::env::var_os("NO_COLOR").is_none();

    for entry in lines {
        stderr.queue(Clear(ClearType::CurrentLine))?;

        let mut left = width;

        for span in &entry.0 {
            if left == 0 {
                break;
            }

            if colour {
                match span.ink {
                    Ink::Plain => {}
                    Ink::Dim => {
                        stderr.queue(SetAttribute(Attribute::Dim))?;
                    }
                    Ink::Bold => {
                        stderr.queue(SetAttribute(Attribute::Bold))?;
                    }
                    Ink::Accent => {
                        stderr.queue(SetForegroundColor(Color::Cyan))?;
                    }
                    Ink::Chosen => {
                        stderr.queue(SetForegroundColor(Color::Cyan))?;
                        stderr.queue(SetAttribute(Attribute::Bold))?;
                    }
                    Ink::Added => {
                        stderr.queue(SetForegroundColor(Color::Green))?;
                    }
                    Ink::Removed => {
                        stderr.queue(SetForegroundColor(Color::Red))?;
                    }
                }
            }

            let text = cut(&span.text, left);
            left -= text.chars().count();

            stderr.queue(Print(text))?;
            stderr.queue(SetAttribute(Attribute::Reset))?;
            stderr.queue(ResetColor)?;
        }

        stderr.queue(Print("\r\n"))?;
    }

    stderr.flush()?;
    *drawn = lines.len() as u16;

    Ok(())
}

pub fn erase(stderr: &mut io::Stderr, lines: u16) -> io::Result<()> {
    if lines == 0 {
        return Ok(());
    }

    stderr.queue(cursor::MoveToPreviousLine(lines))?;
    stderr.queue(Clear(ClearType::FromCursorDown))?;
    stderr.flush()
}

/// Greedy word wrap. Existing line breaks are kept, because a commit body uses
/// them to separate paragraphs.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }

    let width = width.max(20);
    let mut out = Vec::new();

    for paragraph in text.lines() {
        let mut current = String::new();

        for word in paragraph.split_whitespace() {
            if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > width {
                out.push(std::mem::take(&mut current));
            }

            if !current.is_empty() {
                current.push(' ');
            }

            current.push_str(word);
        }

        out.push(current);
    }

    out
}

/// Truncates on a character boundary, counting characters rather than bytes so
/// a multi byte character is never cut in half.
pub fn cut(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }

    text.chars().take(width.saturating_sub(1)).collect()
}
