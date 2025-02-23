use nix::sys::signal::{signal, SigHandler, Signal};
use nix::sys::termios::{tcgetattr, tcsetattr, LocalFlags, SetArg};
use nix::unistd::isatty;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::{self, BufRead, Write};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

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

fn main() {
    // Ignore SIGPIPE so broken stdout does not panic.
    let _ = unsafe { signal(Signal::SIGPIPE, SigHandler::SigIgn) };

    let stdin = io::stdin();
    let is_pipe = !isatty(stdin.as_raw_fd()).unwrap_or(false);

    // Use an atomic flag for quitting.
    let quit_flag = Arc::new(AtomicBool::new(false));

    // Terminal output to /dev/tty
    let mut term_out = OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .expect("Could not open /dev/tty for writing");

    // Channel for pipe lines if pipe is attached.
    let (tx_pipe, rx_pipe) = mpsc::channel::<String>();

    // Spawn pipe reader thread if input is piped.
    if is_pipe {
        let tx_pipe = tx_pipe.clone();
        let quit = Arc::clone(&quit_flag);
        thread::spawn(move || {
            for line in io::stdin().lock().lines() {
                if quit.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(line) = line {
                    // Send line; ignore send errors on quit.
                    let _ = tx_pipe.send(line);
                }
            }
        });

        // Modified pipe printer thread: use locked stdout and catch BrokenPipe errors.
        {
            let quit = Arc::clone(&quit_flag);
            thread::spawn(move || {
                let stdout = io::stdout();
                let mut out = stdout.lock();
                for line in rx_pipe {
                    let highlighted_line = highlight_word_in_string(&line, "row");
                    if writeln!(out, "{}", highlighted_line).is_err() {
                        break;
                    }
                    if quit.load(Ordering::SeqCst) {
                        break;
                    }
                }
            });
        }
    }

    // Updated terminal key listener (from /dev/tty) with raw mode.
    {
        use nix::fcntl::{fcntl, FcntlArg, OFlag};
        let quit = Arc::clone(&quit_flag);
        thread::spawn(move || {
            let mut term_in = OpenOptions::new()
                .read(true)
                .open("/dev/tty")
                .expect("Could not open /dev/tty for reading");
            let fd = term_in.as_raw_fd();

            // Set /dev/tty to nonblocking mode.
            let flags = OFlag::from_bits_truncate(
                fcntl(fd, FcntlArg::F_GETFL).expect("Failed to get flags"),
            );
            fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))
                .expect("Failed to set nonblocking mode");

            let orig_termios = tcgetattr(fd).expect("Failed to get terminal attributes");
            let mut raw = orig_termios.clone();
            raw.local_flags.remove(LocalFlags::ICANON);
            raw.local_flags.remove(LocalFlags::ECHO);
            tcsetattr(fd, SetArg::TCSANOW, &raw).expect("Failed to set terminal to raw mode");

            let mut buf = [0u8; 1];
            loop {
                match term_in.read(&mut buf) {
                    Ok(n) if n > 0 => {
                        if buf[0] as char == 'q' {
                            quit.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // No input available, sleep briefly to avoid busy looping.
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => {
                        eprintln!("Error reading from /dev/tty: {:?}", e);
                        break;
                    }
                    _ => {}
                }
            }
            tcsetattr(fd, SetArg::TCSANOW, &orig_termios)
                .expect("Failed to restore terminal attributes");
        });
    }
    // Counter printing code temporarily disabled.
    // Instead, keep the main thread alive until a quit signal is received.
    while !quit_flag.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(2000));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_pipe_input() {
        // Simplified test: simulate pipe input and quitting.
        let (tx, rx) = mpsc::channel();
        let input = b"line1\nline2\nq\n";
        let stdin = Cursor::new(input);
        let is_pipe = true;
        let quit_flag = Arc::new(AtomicBool::new(false));
        let (tx_pipe, rx_pipe) = mpsc::channel::<String>();

        if is_pipe {
            let quit = Arc::clone(&quit_flag);
            let tx_pipe = tx_pipe.clone();
            thread::spawn(move || {
                for line in io::BufReader::new(stdin).lines() {
                    if quit.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Ok(line) = line {
                        let _ = tx_pipe.send(line);
                    }
                }
            });
        }

        // Consume pipe output.
        let quit = Arc::clone(&quit_flag);
        thread::spawn(move || {
            for line in rx_pipe {
                if line.trim() == "q" {
                    quit.store(true, Ordering::SeqCst);
                    tx.send(line).unwrap();
                    break;
                }
                tx.send(line).unwrap();
            }
        });

        // Main simulation loop.
        while !quit_flag.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
        }

        let mut output = Vec::new();
        while let Ok(line) = rx.try_recv() {
            output.push(line);
        }

        assert_eq!(output, vec!["line1", "line2", "q"]);
    }
}
