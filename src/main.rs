use nix::unistd::isatty;
use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

fn main() {
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

        // Spawn pipe printer thread.
        {
            let quit = Arc::clone(&quit_flag);
            thread::spawn(move || {
                for line in rx_pipe {
                    println!("{}", line);
                    if quit.load(Ordering::SeqCst) {
                        break;
                    }
                }
            });
        }
    }

    // Spawn terminal key listener (from /dev/tty).
    {
        let quit = Arc::clone(&quit_flag);
        thread::spawn(move || {
            let term_in = OpenOptions::new()
                .read(true)
                .open("/dev/tty")
                .expect("Could not open /dev/tty for reading");
            let mut reader = io::BufReader::new(term_in);
            let mut buffer = String::new();
            loop {
                buffer.clear();
                if reader.read_line(&mut buffer).is_ok() {
                    if buffer.trim() == "q" {
                        quit.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }
        });
    }

    let mut counter = 0;

    // Main thread outputs "*" every 1s to the terminal.
    while !quit_flag.load(Ordering::SeqCst) {
        counter = (counter + 1) % 10;
        thread::sleep(Duration::from_secs(1));

        write!(term_out, "\r{}", counter).unwrap();
        term_out.flush().unwrap();
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
                tx.send(line).unwrap();
                if line.trim() == "q" {
                    quit.store(true, Ordering::SeqCst);
                    break;
                }
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
