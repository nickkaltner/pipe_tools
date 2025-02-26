use nix::sys::signal::{signal, SigHandler, Signal};
use nix::sys::termios::{tcgetattr, tcsetattr, LocalFlags, SetArg};
use nix::unistd::isatty;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::{self, BufRead, Write};
use std::os::unix::io::AsRawFd;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

struct StatusArea {
    status_lines: Vec<String>,
}

impl StatusArea {
    fn new() -> Self {
        StatusArea {
            status_lines: vec![String::new(); 3],
        }
    }

    fn update(&mut self, line: usize, text: &str) {
        if line < 3 {
            self.status_lines[line] = text.to_string();
            self.redraw();
        }
    }

    fn redraw(&self) {
        // Use /dev/tty for status updates instead of stdout
        let mut term_out = OpenOptions::new()
            .write(true)
            .open("/dev/tty")
            .expect("Could not open /dev/tty for writing");

        let (_, rows) = get_terminal_size().unwrap();

        // save the current cursor position
        write!(term_out, "\x1B[s").unwrap();

        // Move cursor to the beginning of the status area
        write!(term_out, "\x1B[{};1H", rows - 2).unwrap();
        write!(term_out, "\x1b[44m").unwrap();

        // Clear the status area
        for _ in 0..3 {
            write!(term_out, "\x1B[2K").unwrap(); // Clear the current line
            write!(term_out, "\x1B[1B").unwrap(); // Move cursor down one line
        }

        // Move cursor back to the beginning of the status area
        write!(term_out, "\x1B[{};1H", rows - 2).unwrap();

        // Print the status lines
        for line in &self.status_lines {
            writeln!(term_out, "{}", line).unwrap();
        }

        // Reset scroll region
        set_scroll_region_on_term(&mut term_out, 0, rows - 4).unwrap();

        // restore the cursor position
        write!(term_out, "\x1B[u").unwrap();
        write!(term_out, "\x1b[0m").unwrap();
    }
}

// Add this helper function for setting scroll region on /dev/tty
fn set_scroll_region_on_term<W: Write>(term: &mut W, top: u16, bottom: u16) -> io::Result<()> {
    write!(term, "\x1B[{};{}r", top + 1, bottom + 1)?;
    term.flush()?;
    Ok(())
}

fn set_scroll_region(top: u16, bottom: u16) -> io::Result<()> {
    let mut stdout = io::stdout();
    write!(stdout, "\x1B[{};{}r", top + 1, bottom + 1)?;
    stdout.flush()?;
    Ok(())
}

fn reset_scroll_region() -> io::Result<()> {
    let mut stdout = io::stdout();
    write!(stdout, "\x1B[0r")?;
    stdout.flush()?;
    Ok(())
}

fn get_terminal_size() -> io::Result<(u16, u16)> {
    let size = crossterm::terminal::size()?;
    Ok(size)
}

fn highlight_word_in_string(string: &str, word: &str) -> String {
    match string.find(word) {
        Some(_position) => {
            let highlighted = format!("{}{}{}", "\x1B[37;101m", word, "\x1B[0m");
            // Replace the word with the highlighted version in the original string.
            string.replace(word, &highlighted)
        }
        None => {
            string.to_string() // Return the input string if no match is found.
        }
    }
}

fn main() -> io::Result<()> {
    // Ignore SIGPIPE so broken stdout does not panic.
    let _ = unsafe { signal(Signal::SIGPIPE, SigHandler::SigIgn) };

    let filter_string = Arc::new(Mutex::new("stream".to_string()));

    let (_, rows) = get_terminal_size()?;

    // save the current cursor position
    print!("\x1B[s");
    // clear the screen
    print!("\x1B[2J");

    // Set scroll region to exclude the status area
    set_scroll_region(0, rows - 4)?;

    let mut status_bar = StatusArea::new();
    status_bar.update(0, "");
    status_bar.update(
        1,
        format!(
            "Filter [\x1B[37;101m{}\x1b[44m]",
            filter_string.lock().unwrap()
        )
        .as_str(),
    );
    status_bar.update(2, "");
    status_bar.redraw();

    print!("\x1B[u"); // restore cursor position

    let stdin = io::stdin();
    let is_pipe = !isatty(stdin.as_raw_fd()).unwrap_or(false);

    // Replace the atomic flag with a quit channel.
    let (quit_tx, quit_rx) = mpsc::channel::<()>();

    // Terminal output to /dev/tty
    let mut term_out = OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .expect("Could not open /dev/tty for writing");

    let _ = writeln!(term_out, "rows {} ", rows);

    // Channel for pipe lines if pipe is attached.
    let (tx_pipe, rx_pipe) = mpsc::channel::<String>();

    // Before creating pipe threads, clone it for pipe printer
    let filter_for_pipe = filter_string.clone();

    // Spawn pipe reader thread if input is piped.
    if is_pipe {
        let tx_pipe = tx_pipe.clone();
        let quit_tx_pipe = quit_tx.clone();
        thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                if let Ok(line) = line {
                    // Send line; ignore send errors on quit.
                    let _ = tx_pipe.send(line);
                }
            }
            // When the pipe ends send quit signal.
            let _ = quit_tx_pipe.send(());
        });

        // Modified pipe printer thread with access to shared filter string
        {
            let filter_string = filter_for_pipe.clone();

            thread::spawn(move || {
                let stdout = io::stdout();
                let mut out = stdout.lock();
                for line in rx_pipe {
                    // Get the current filter string for highlighting
                    let current_filter = filter_string.lock().unwrap().clone();
                    let highlighted_line = highlight_word_in_string(&line, &current_filter);
                    if writeln!(out, "{}", highlighted_line).is_err() {
                        break;
                    }
                }
            });
        }
    }

    // Updated terminal key listener with filter editing capabilities
    {
        use nix::fcntl::{fcntl, FcntlArg, OFlag};
        let quit_tx_term = quit_tx.clone();
        let filter_string_for_input = filter_string.clone();
        let mut term_in = OpenOptions::new()
            .read(true)
            .write(true)
            .append(true)
            .open("/dev/tty")
            .expect("Could not open /dev/tty for reading");
        let fd = term_in.as_raw_fd();

        // Set /dev/tty to nonblocking mode.
        let flags =
            OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL).expect("Failed to get flags"));
        fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))
            .expect("Failed to set nonblocking mode");

        let orig_termios = tcgetattr(fd).expect("Failed to get terminal attributes");
        let mut raw = orig_termios.clone();
        raw.local_flags.remove(LocalFlags::ICANON);
        raw.local_flags.remove(LocalFlags::ECHO);
        tcsetattr(fd, SetArg::TCSANOW, &raw).expect("Failed to set terminal to raw mode");

        // Create a mutex-wrapped reference to status_bar for the thread
        let status_bar = Arc::new(Mutex::new(status_bar));
        let status_bar_for_thread = status_bar.clone();

        thread::spawn(move || {
            let mut buf = [0u8; 1];
            loop {
                match term_in.read(&mut buf) {
                    Ok(1) => {
                        match buf[0] {
                            b'q' => {
                                // Still quit when 'q' is pressed
                                writeln!(term_out, "Quitting...").unwrap_or(());
                                let _ = quit_tx_term.send(());
                                break;
                            }
                            8 | 127 => {
                                // Backspace or Delete
                                // Remove the last character from filter_string
                                let mut filter = filter_string_for_input.lock().unwrap();
                                if !filter.is_empty() {
                                    filter.pop();
                                    // Update status bar with new filter
                                    let mut status = status_bar_for_thread.lock().unwrap();
                                    status.update(
                                        1,
                                        &format!("Filter [\x1B[37;101m{}\x1b[44m]", *filter),
                                    );
                                }
                            }
                            32..=126 => {
                                // Printable ASCII
                                // Add the character to filter_string
                                let mut filter = filter_string_for_input.lock().unwrap();
                                filter.push(buf[0] as char);
                                // Update status bar with new filter
                                let mut status = status_bar_for_thread.lock().unwrap();
                                status.update(
                                    1,
                                    &format!("Filter [\x1B[37;101m{}\x1b[44m]", *filter),
                                );
                            }
                            _ => {} // Ignore other keys
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // No data available yet
                        // thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break, // Error reading, exit thread
                    Ok(0) => break,  // End of file
                    _ => {}          // Unexpected read size
                }
            }

            // Restore terminal attributes once before exiting
            let _ = tcsetattr(fd, SetArg::TCSANOW, &orig_termios);
        });
    }
    // Instead of polling on an atomic flag, block until a quit signal is received.
    let _ = quit_rx.recv();
    let _ = reset_scroll_region();
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::io::Cursor;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_highlight_word_found() {
        let input = "this is a stream of data";
        let expected = format!("this is a {} of data", "\x1B[37;101mstream\x1B[0m");
        let result = highlight_word_in_string(input, "stream");
        assert_eq!(result, expected);
    }

    #[test]
    fn test_highlight_word_not_found() {
        let input = "no match here";
        let result = highlight_word_in_string(input, "stream");
        assert_eq!(result, "no match here");
    }

    #[test]
    fn test_pipe_simulation() {
        // Simulate piped input by reading from a Cursor.
        let input_data = b"line1\nline2\nstream\nq\n";
        let cursor = Cursor::new(input_data);
        let reader = std::io::BufReader::new(cursor);

        // Channel simulating the pipe sender/receiver.
        let (tx_pipe, rx_pipe) = mpsc::channel::<String>();
        // Collect output after highlighting.
        let (tx_out, rx_out) = mpsc::channel::<String>();

        // Simulated pipe reader thread.
        thread::spawn(move || {
            for line in reader.lines() {
                if let Ok(line) = line {
                    let _ = tx_pipe.send(line);
                }
            }
        });

        // Simulated pipe printer thread.
        let filter_word = "stream";
        thread::spawn(move || {
            for line in rx_pipe {
                let highlighted_line = highlight_word_in_string(&line, filter_word);
                let _ = tx_out.send(highlighted_line);
            }
        });

        // Wait a bit for threads to process.
        thread::sleep(Duration::from_millis(100));

        // Collect all output.
        let mut outputs = Vec::new();
        while let Ok(line) = rx_out.try_recv() {
            outputs.push(line);
        }

        let expected = vec![
            "line1".to_string(),
            "line2".to_string(),
            format!("{}{}{}", "s", "\x1B[37;101mstream\x1B[0m", ""),
            "q".to_string(),
        ];
        assert_eq!(outputs, expected);
    }
}
