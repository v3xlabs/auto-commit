use std::io::{self, IsTerminal};

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    terminal, ExecutableCommand,
};

use crate::screen::{self, line, Ink, Line};

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
    Reload,
    Cancel,
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
        let frame = frame(prompt, items, thinking, can_edit, &state);
        screen::paint(&mut stderr, &frame, &mut drawn)?;

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
            KeyCode::Char('r') => break Choice::Reload,
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

    screen::erase(&mut stderr, drawn)?;
    stderr.execute(cursor::Show)?;
    terminal::disable_raw_mode()?;

    Ok(outcome)
}

struct State {
    cursor_at: usize,
    thinking_open: bool,
    feedback: Option<String>,
}

fn frame(
    prompt: &str,
    items: &[Item],
    thinking: Option<&str>,
    can_edit: bool,
    state: &State,
) -> Vec<Line> {
    // A frame taller than the terminal scrolls, and the row count used to
    // erase it is then wrong, so the panels are trimmed to what fits.
    let (width, height) = screen::size();

    let mut lines = vec![line(format!("  {prompt}"), Ink::Bold), line("", Ink::Plain)];

    for (index, item) in items.iter().enumerate() {
        let picked = index == state.cursor_at;

        if picked {
            lines.push(line(
                format!("> {}  {}", index + 1, item.title),
                Ink::Chosen,
            ));

            for wrapped in screen::wrap(item.detail.trim(), width.saturating_sub(5)) {
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
            let body: Vec<String> = screen::wrap(thinking.trim(), width.saturating_sub(4));
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
            actions.push("r  reload".to_owned());

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

    lines
}
