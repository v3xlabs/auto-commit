use std::io::{self, IsTerminal, Write};

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType},
    ExecutableCommand, QueueableCommand,
};

/// One choice, with the detail that only shows while it is highlighted. The
/// detail is why this exists rather than a list of one-line labels: a commit
/// body has to be read before the message carrying it is picked.
pub struct Item {
    pub title: String,
    pub detail: String,
}

/// What the reader did with the list.
pub enum Choice {
    Item(usize),
    Edit(usize),
    Feedback(String),
    Cancel,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ink {
    Plain,
    Dim,
    Bold,
    Accent,
    Chosen,
}

struct Line {
    text: String,
    ink: Ink,
}

fn line(text: impl Into<String>, ink: Ink) -> Line {
    Line {
        text: text.into(),
        ink,
    }
}

/// Renders the list until something is chosen. Only the highlighted item shows
/// its detail, so the list stays short however long the bodies are.
///
/// `thinking` enables the key that shows it. Passing `None` hides that action
/// rather than offering one that does nothing.
pub fn select(
    prompt: &str,
    items: &[Item],
    thinking: Option<&str>,
    can_edit: bool,
) -> io::Result<Choice> {
    if items.is_empty() {
        return Ok(Choice::Cancel);
    }

    let mut stderr = io::stderr();

    if !stderr.is_terminal() || !io::stdin().is_terminal() {
        return Ok(Choice::Item(0));
    }

    let mut state = State {
        cursor_at: 0,
        thinking_open: false,
        feedback: None,
    };

    let mut drawn = 0u16;

    terminal::enable_raw_mode()?;
    stderr.execute(cursor::Hide)?;

    let outcome = loop {
        draw(
            &mut stderr,
            prompt,
            items,
            thinking,
            can_edit,
            &state,
            &mut drawn,
        )?;

        let Event::Key(KeyEvent {
            code, modifiers, ..
        }) = event::read()?
        else {
            continue;
        };

        if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
            break Choice::Cancel;
        }

        // Typing feedback takes every printable key, so the list shortcuts
        // only apply while the field is closed.
        if let Some(text) = state.feedback.as_mut() {
            match code {
                KeyCode::Char(typed) => text.push(typed),
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Esc => state.feedback = None,
                KeyCode::Enter if !text.trim().is_empty() => break Choice::Feedback(text.clone()),
                _ => {}
            }

            continue;
        }

        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                state.cursor_at = state.cursor_at.checked_sub(1).unwrap_or(items.len() - 1);
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                state.cursor_at = (state.cursor_at + 1) % items.len();
            }
            KeyCode::Enter => break Choice::Item(state.cursor_at),
            KeyCode::Esc | KeyCode::Char('q') => break Choice::Cancel,
            KeyCode::Char('f') => state.feedback = Some(String::new()),
            KeyCode::Char('e') if can_edit => break Choice::Edit(state.cursor_at),
            // A toggle, not a print. Pressing it twice puts the panel away
            // again rather than printing the same thinking a second time.
            KeyCode::Char('t') if thinking.is_some() => {
                state.thinking_open = !state.thinking_open;
            }
            KeyCode::Char(digit) if digit.is_ascii_digit() => {
                if let Some(index) = digit.to_digit(10).map(|digit| digit as usize) {
                    if index >= 1 && index <= items.len() {
                        break Choice::Item(index - 1);
                    }
                }
            }
            _ => {}
        }
    };

    erase(&mut stderr, drawn)?;
    stderr.execute(cursor::Show)?;
    terminal::disable_raw_mode()?;

    Ok(outcome)
}

struct State {
    cursor_at: usize,
    thinking_open: bool,
    feedback: Option<String>,
}

/// Draws one frame in place, over the rows the previous frame used.
fn draw(
    stderr: &mut io::Stderr,
    prompt: &str,
    items: &[Item],
    thinking: Option<&str>,
    can_edit: bool,
    state: &State,
    drawn: &mut u16,
) -> io::Result<()> {
    erase(stderr, *drawn)?;

    // A frame taller than the terminal scrolls, and the row count used to
    // erase it is then wrong, so the panels are trimmed to what fits.
    let (columns, rows) = terminal::size().unwrap_or((0, 0));
    let width = (columns as usize).max(40);
    let height = (rows as usize).max(10);

    let mut lines = vec![line(format!("  {prompt}"), Ink::Bold), line("", Ink::Plain)];

    for (index, item) in items.iter().enumerate() {
        let picked = index == state.cursor_at;

        if picked {
            lines.push(line(
                format!("> {}  {}", index + 1, item.title),
                Ink::Chosen,
            ));

            for wrapped in wrap(item.detail.trim(), width.saturating_sub(5)) {
                lines.push(line(format!("     {wrapped}"), Ink::Dim));
            }
        } else {
            lines.push(line(format!("  {}  {}", index + 1, item.title), Ink::Plain));
        }
    }

    if state.thinking_open {
        if let Some(thinking) = thinking {
            lines.push(line("", Ink::Plain));

            // Room for what is already queued plus the actions below.
            let room = height.saturating_sub(lines.len() + 6);
            let body: Vec<String> = wrap(thinking.trim(), width.saturating_sub(4));
            let shown = body.len().min(room);

            for wrapped in body.iter().take(shown) {
                lines.push(line(format!("    {wrapped}"), Ink::Dim));
            }

            if shown < body.len() {
                lines.push(line(
                    format!("    and {} more lines", body.len() - shown),
                    Ink::Dim,
                ));
            }
        }
    }

    lines.push(line("", Ink::Plain));

    match &state.feedback {
        Some(text) => {
            lines.push(line(format!("  feedback: {text}\u{2588}"), Ink::Accent));
            lines.push(line("  enter to send, esc to go back", Ink::Dim));
        }
        None => {
            let mut actions = Vec::new();

            if can_edit {
                actions.push("e  edit".to_owned());
            }

            actions.push("f  give feedback".to_owned());

            if thinking.is_some() {
                actions.push(if state.thinking_open {
                    "t  hide thinking".to_owned()
                } else {
                    "t  show thinking".to_owned()
                });
            }

            actions.push("q  cancel".to_owned());

            lines.push(line(format!("  {}", actions.join("   ")), Ink::Accent));
            lines.push(line("  up and down to move, enter to choose", Ink::Dim));
        }
    }

    // The rest of the tool goes through owo-colors, which checks this itself.
    // These styles are written straight to the terminal, so they check here.
    let colour = std::env::var_os("NO_COLOR").is_none();

    for entry in &lines {
        stderr.queue(Clear(ClearType::CurrentLine))?;

        match entry.ink {
            _ if !colour => {}
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
        }

        stderr.queue(Print(cut(&entry.text, width)))?;
        stderr.queue(SetAttribute(Attribute::Reset))?;
        stderr.queue(ResetColor)?;
        stderr.queue(Print("\r\n"))?;
    }

    stderr.flush()?;
    *drawn = lines.len() as u16;

    Ok(())
}

fn erase(stderr: &mut io::Stderr, lines: u16) -> io::Result<()> {
    if lines == 0 {
        return Ok(());
    }

    stderr.queue(cursor::MoveToPreviousLine(lines))?;
    stderr.queue(Clear(ClearType::FromCursorDown))?;
    stderr.flush()
}

/// Greedy word wrap. Existing line breaks are kept, because a commit body uses
/// them to separate paragraphs.
fn wrap(text: &str, width: usize) -> Vec<String> {
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
fn cut(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }

    text.chars().take(width.saturating_sub(1)).collect()
}
