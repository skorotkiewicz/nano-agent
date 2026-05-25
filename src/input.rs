use std::io::{self, IsTerminal, Read, Write};
use std::process::{Command, Stdio};

fn color(code: &str, text: &str) -> String {
    if io::stderr().is_terminal() {
        format!("\x1b[{}m{}\x1b[0m", code, text)
    } else {
        text.to_string()
    }
}

fn repl_line_text(line: &str) -> (&str, bool) {
    let trimmed = line.trim_end();
    if let Some(text) = trimmed.strip_suffix('\\') {
        (text.trim_end(), true)
    } else {
        (line, false)
    }
}

#[derive(Default)]
struct EditableLine {
    chars: Vec<char>,
    cursor: usize,
}

impl EditableLine {
    fn insert(&mut self, ch: char) {
        self.chars.insert(self.cursor, ch);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    fn move_home(&mut self) {
        self.cursor = 0;
    }

    fn move_end(&mut self) {
        self.cursor = self.chars.len();
    }

    fn text(&self) -> String {
        self.chars.iter().collect()
    }
}

struct RawTerminal {
    saved_state: String,
}

impl RawTerminal {
    fn enter() -> io::Result<Self> {
        let output = Command::new("stty")
            .arg("-g")
            .stdin(Stdio::inherit())
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other("failed to read terminal state"));
        }

        let saved_state = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let status = Command::new("stty")
            .args(["raw", "-echo", "min", "0", "time", "1"])
            .stdin(Stdio::inherit())
            .status()?;
        if !status.success() {
            return Err(io::Error::other("failed to enter raw terminal mode"));
        }

        Ok(Self { saved_state })
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = Command::new("stty")
            .arg(&self.saved_state)
            .stdin(Stdio::inherit())
            .status();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineKey {
    Char(char),
    Left,
    Right,
    Home,
    End,
    Backspace,
    Delete,
    Enter,
    Interrupt,
    Eof,
    Unknown,
}

enum TtyLineResult {
    Line(String),
    Interrupted,
    Eof,
}

fn read_required_byte(stdin: &mut io::StdinLock<'_>) -> io::Result<u8> {
    loop {
        let mut byte = [0];
        match stdin.read(&mut byte) {
            Ok(1) => return Ok(byte[0]),
            Ok(0) => continue,
            Ok(_) => unreachable!(),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
}

fn read_optional_byte(stdin: &mut io::StdinLock<'_>) -> io::Result<Option<u8>> {
    let mut byte = [0];
    match stdin.read(&mut byte) {
        Ok(1) => Ok(Some(byte[0])),
        Ok(0) => Ok(None),
        Ok(_) => unreachable!(),
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(None),
        Err(e) => Err(e),
    }
}

fn utf8_width(first: u8) -> Option<usize> {
    if first & 0b1000_0000 == 0 {
        Some(1)
    } else if first & 0b1110_0000 == 0b1100_0000 {
        Some(2)
    } else if first & 0b1111_0000 == 0b1110_0000 {
        Some(3)
    } else if first & 0b1111_1000 == 0b1111_0000 {
        Some(4)
    } else {
        None
    }
}

fn read_utf8_char(stdin: &mut io::StdinLock<'_>, first: u8) -> io::Result<Option<char>> {
    let Some(width) = utf8_width(first) else {
        return Ok(None);
    };

    let mut bytes = [0; 4];
    bytes[0] = first;
    for byte in bytes.iter_mut().take(width).skip(1) {
        *byte = read_required_byte(stdin)?;
    }

    Ok(std::str::from_utf8(&bytes[..width])
        .ok()
        .and_then(|text| text.chars().next()))
}

fn read_csi_key(stdin: &mut io::StdinLock<'_>) -> io::Result<LineKey> {
    let mut params = Vec::new();
    loop {
        let Some(byte) = read_optional_byte(stdin)? else {
            return Ok(LineKey::Unknown);
        };

        match byte {
            b'C' => return Ok(LineKey::Right),
            b'D' => return Ok(LineKey::Left),
            b'H' => return Ok(LineKey::Home),
            b'F' => return Ok(LineKey::End),
            b'~' => {
                return Ok(match params.first().copied() {
                    Some(b'1') | Some(b'7') => LineKey::Home,
                    Some(b'3') => LineKey::Delete,
                    Some(b'4') | Some(b'8') => LineKey::End,
                    _ => LineKey::Unknown,
                });
            }
            b'0'..=b'9' | b';' => params.push(byte),
            _ => return Ok(LineKey::Unknown),
        }
    }
}

fn read_escape_key(stdin: &mut io::StdinLock<'_>) -> io::Result<LineKey> {
    let Some(byte) = read_optional_byte(stdin)? else {
        return Ok(LineKey::Unknown);
    };

    match byte {
        b'[' => read_csi_key(stdin),
        b'O' => match read_optional_byte(stdin)? {
            Some(b'H') => Ok(LineKey::Home),
            Some(b'F') => Ok(LineKey::End),
            _ => Ok(LineKey::Unknown),
        },
        _ => Ok(LineKey::Unknown),
    }
}

fn read_line_key(stdin: &mut io::StdinLock<'_>) -> io::Result<LineKey> {
    let byte = read_required_byte(stdin)?;
    match byte {
        b'\r' | b'\n' => Ok(LineKey::Enter),
        1 => Ok(LineKey::Home),
        3 => Ok(LineKey::Interrupt),
        4 => Ok(LineKey::Eof),
        5 => Ok(LineKey::End),
        8 | 127 => Ok(LineKey::Backspace),
        27 => read_escape_key(stdin),
        byte if byte >= 0x20 => {
            read_utf8_char(stdin, byte).map(|ch| ch.map(LineKey::Char).unwrap_or(LineKey::Unknown))
        }
        _ => Ok(LineKey::Unknown),
    }
}

fn repaint_line(prompt: &str, line: &EditableLine) -> io::Result<()> {
    eprint!("\r\x1b[2K{} {}", color("36", prompt), line.text());
    let chars_after_cursor = line.chars.len().saturating_sub(line.cursor);
    if chars_after_cursor > 0 {
        eprint!("\x1b[{}D", chars_after_cursor);
    }
    io::stderr().flush()
}

fn read_tty_line(prompt: &str) -> io::Result<Option<String>> {
    let outcome = {
        let _raw = RawTerminal::enter()?;
        let mut line = EditableLine::default();
        let mut stdin = io::stdin().lock();

        repaint_line(prompt, &line)?;
        loop {
            match read_line_key(&mut stdin)? {
                LineKey::Char(ch) => line.insert(ch),
                LineKey::Left => line.move_left(),
                LineKey::Right => line.move_right(),
                LineKey::Home => line.move_home(),
                LineKey::End => line.move_end(),
                LineKey::Backspace => line.backspace(),
                LineKey::Delete => line.delete(),
                LineKey::Enter => break TtyLineResult::Line(line.text()),
                LineKey::Interrupt => break TtyLineResult::Interrupted,
                LineKey::Eof if line.chars.is_empty() => break TtyLineResult::Eof,
                LineKey::Eof => line.delete(),
                LineKey::Unknown => continue,
            }
            repaint_line(prompt, &line)?;
        }
    };

    match outcome {
        TtyLineResult::Line(line) => {
            eprintln!();
            Ok(Some(line))
        }
        TtyLineResult::Interrupted => {
            eprintln!("^C");
            Ok(Some(String::new()))
        }
        TtyLineResult::Eof => {
            eprintln!();
            Ok(None)
        }
    }
}

fn read_tty_repl_input() -> Option<String> {
    let mut input = String::new();
    let mut continuation = false;

    loop {
        let prompt = if continuation { "... >" } else { "you >" };
        let line = match read_tty_line(prompt) {
            Ok(Some(line)) => line,
            Ok(None) => return None,
            Err(e) => {
                eprintln!("input error: {}", e);
                return None;
            }
        };

        let (text, continues) = repl_line_text(&line);
        if !input.is_empty() {
            input.push('\n');
        }
        input.push_str(text);

        if continues {
            continuation = true;
            continue;
        }

        return Some(input.trim().to_string());
    }
}

async fn read_plain_repl_input<R>(lines: &mut tokio::io::Lines<R>) -> Option<String>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut input = String::new();
    let mut continuation = false;

    loop {
        let prompt = if continuation { "... >" } else { "you >" };
        eprint!("{} ", color("36", prompt));
        let _ = io::stderr().flush();

        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            _ => {
                eprintln!();
                return None;
            }
        };

        let (text, continues) = repl_line_text(&line);
        if !input.is_empty() {
            input.push('\n');
        }
        input.push_str(text);

        if continues {
            continuation = true;
            continue;
        }

        return Some(input.trim().to_string());
    }
}

pub async fn read_repl_input<R>(lines: &mut tokio::io::Lines<R>) -> Option<String>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    if io::stdin().is_terminal() && io::stderr().is_terminal() {
        tokio::task::spawn_blocking(read_tty_repl_input)
            .await
            .unwrap_or(None)
    } else {
        read_plain_repl_input(lines).await
    }
}

#[cfg(test)]
mod tests {
    use super::{EditableLine, repl_line_text};

    #[test]
    fn repl_line_text_detects_continuation() {
        assert_eq!(repl_line_text("first line \\"), ("first line", true));
        assert_eq!(repl_line_text("plain text"), ("plain text", false));
    }

    #[test]
    fn editable_line_edits_at_cursor() {
        let mut line = EditableLine::default();
        for ch in "helo".chars() {
            line.insert(ch);
        }

        line.move_left();
        line.insert('l');
        assert_eq!(line.text(), "hello");

        line.move_home();
        line.delete();
        assert_eq!(line.text(), "ello");

        line.move_end();
        line.backspace();
        assert_eq!(line.text(), "ell");
    }
}
