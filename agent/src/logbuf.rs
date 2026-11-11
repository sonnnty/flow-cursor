//! The most recent log lines, kept in memory for the status window (also
//! when the log goes to the systemd journal and there is no file to open).

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};

/// Lines kept.
pub const LOG_LINES: usize = 1000;
/// Longer lines are cut (a log line with a huge debug dump).
const MAX_LINE_CHARS: usize = 2000;

/// Ring buffer of formatted log lines; a `tracing_subscriber` writer.
#[derive(Clone, Default)]
pub struct LogBuffer(Arc<Mutex<VecDeque<String>>>);

impl LogBuffer {
    pub fn push(&self, line: &str) {
        let line = line.trim_end();
        if line.is_empty() {
            return;
        }
        let line = match line.char_indices().nth(MAX_LINE_CHARS) {
            Some((i, _)) => format!("{}…", &line[..i]),
            None => line.to_string(),
        };
        let mut q = self.0.lock().expect("poisoned");
        if q.len() >= LOG_LINES {
            q.pop_front();
        }
        q.push_back(line);
    }

    /// The last `n` lines, oldest first.
    pub fn tail(&self, n: usize) -> Vec<String> {
        let q = self.0.lock().expect("poisoned");
        q.iter().skip(q.len().saturating_sub(n)).cloned().collect()
    }
}

/// Collects one formatted event and stores it when dropped.
pub struct LineWriter {
    buf: LogBuffer,
    bytes: Vec<u8>,
}

impl Write for LineWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for LineWriter {
    fn drop(&mut self) {
        for l in String::from_utf8_lossy(&self.bytes).lines() {
            self.buf.push(l);
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LineWriter;
    fn make_writer(&'a self) -> LineWriter {
        LineWriter { buf: self.clone(), bytes: Vec::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_last_lines() {
        let b = LogBuffer::default();
        for i in 0..LOG_LINES + 5 {
            b.push(&format!("line {i}\n"));
        }
        let t = b.tail(3);
        assert_eq!(
            t,
            vec![
                format!("line {}", LOG_LINES + 2),
                format!("line {}", LOG_LINES + 3),
                format!("line {}", LOG_LINES + 4)
            ]
        );
        assert_eq!(b.tail(usize::MAX).len(), LOG_LINES);
        b.push("   \n");
        assert_eq!(b.tail(1)[0], format!("line {}", LOG_LINES + 4), "blank lines are skipped");
        b.push(&"é".repeat(MAX_LINE_CHARS + 10));
        assert_eq!(b.tail(1)[0].chars().count(), MAX_LINE_CHARS + 1);
    }

    #[test]
    fn works_as_a_tracing_writer() {
        use tracing_subscriber::prelude::*;
        let b = LogBuffer::default();
        let sub = tracing_subscriber::registry()
            .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(b.clone()));
        tracing::subscriber::with_default(sub, || tracing::info!(peer = 3, "hello from the test"));
        let t = b.tail(10);
        assert_eq!(t.len(), 1);
        assert!(t[0].contains("INFO") && t[0].contains("hello from the test") && t[0].contains("peer=3"), "{t:?}");
    }
}
