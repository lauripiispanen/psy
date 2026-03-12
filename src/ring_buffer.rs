use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::protocol::StreamFilter;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub timestamp: DateTime<Utc>,
    pub stream: Stream,
    pub content: String,
}

impl fmt::Display for LogLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let stream_label = match self.stream {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
        };
        write!(
            f,
            "[{} {}] {}",
            self.timestamp.format("%Y-%m-%dT%H:%M:%S%.3fZ"),
            stream_label,
            self.content,
        )
    }
}

// ---------------------------------------------------------------------------
// Inner state (behind Mutex)
// ---------------------------------------------------------------------------

const DEFAULT_MAX_LINES: usize = 10_000;
const DEFAULT_MAX_BYTES: usize = 2 * 1024 * 1024; // 2 MB

struct Inner {
    buf: VecDeque<LogLine>,
    total_bytes: usize,
    max_lines: usize,
    max_bytes: usize,
    tx: broadcast::Sender<LogLine>,
}

impl Inner {
    fn new(max_lines: usize, max_bytes: usize) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            buf: VecDeque::new(),
            total_bytes: 0,
            max_lines,
            max_bytes,
            tx,
        }
    }

    fn evict(&mut self) {
        while (self.buf.len() > self.max_lines || self.total_bytes > self.max_bytes)
            && !self.buf.is_empty()
        {
            if let Some(old) = self.buf.pop_front() {
                self.total_bytes = self.total_bytes.saturating_sub(old.content.len());
            }
        }
    }

    fn push(&mut self, stream: Stream, content: String) {
        let line = LogLine {
            timestamp: Utc::now(),
            stream,
            content,
        };
        self.total_bytes += line.content.len();
        self.buf.push_back(line.clone());
        self.evict();
        // Ignore send errors — no active subscribers is fine.
        let _ = self.tx.send(line);
    }

    fn lines(&self, tail: Option<usize>, filter: StreamFilter) -> Vec<LogLine> {
        let iter = self.buf.iter().filter(|l| match filter {
            StreamFilter::All => true,
            StreamFilter::Stdout => l.stream == Stream::Stdout,
            StreamFilter::Stderr => l.stream == Stream::Stderr,
        });

        match tail {
            Some(n) => {
                let filtered: Vec<_> = iter.cloned().collect();
                let start = filtered.len().saturating_sub(n);
                filtered[start..].to_vec()
            }
            None => iter.cloned().collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Public handle (cheaply cloneable)
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct RingBuffer {
    inner: Arc<Mutex<Inner>>,
}

impl RingBuffer {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::new(DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES))),
        }
    }

    /// Create a ring buffer with custom limits (useful for testing).
    pub fn with_capacity(max_lines: usize, max_bytes: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::new(max_lines, max_bytes))),
        }
    }

    pub fn push(&self, stream: Stream, content: String) {
        self.inner.lock().unwrap().push(stream, content);
    }

    pub fn lines(&self, tail: Option<usize>, filter: StreamFilter) -> Vec<LogLine> {
        self.inner.lock().unwrap().lines(tail, filter)
    }

    /// Subscribe to new log lines via a broadcast channel (for follow mode).
    pub fn subscribe(&self) -> broadcast::Receiver<LogLine> {
        self.inner.lock().unwrap().tx.subscribe()
    }
}

impl Default for RingBuffer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_push_retrieve() {
        let rb = RingBuffer::new();
        rb.push(Stream::Stdout, "hello".into());
        rb.push(Stream::Stderr, "world".into());

        let all = rb.lines(None, StreamFilter::All);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].content, "hello");
        assert_eq!(all[0].stream, Stream::Stdout);
        assert_eq!(all[1].content, "world");
        assert_eq!(all[1].stream, Stream::Stderr);
    }

    #[test]
    fn eviction_at_line_limit() {
        let rb = RingBuffer::with_capacity(3, usize::MAX);
        for i in 0..5 {
            rb.push(Stream::Stdout, format!("line-{i}"));
        }
        let all = rb.lines(None, StreamFilter::All);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].content, "line-2");
        assert_eq!(all[1].content, "line-3");
        assert_eq!(all[2].content, "line-4");
    }

    #[test]
    fn eviction_at_byte_limit() {
        // Each line is 10 bytes; allow 25 bytes max => keeps at most 2 full lines
        // after eviction runs (the third push brings total to 30, evicting the oldest).
        let rb = RingBuffer::with_capacity(usize::MAX, 25);
        rb.push(Stream::Stdout, "aaaaaaaaaa".into()); // 10
        rb.push(Stream::Stdout, "bbbbbbbbbb".into()); // 20
        rb.push(Stream::Stdout, "cccccccccc".into()); // 30 -> evict first -> 20

        let all = rb.lines(None, StreamFilter::All);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].content, "bbbbbbbbbb");
        assert_eq!(all[1].content, "cccccccccc");
    }

    #[test]
    fn tail_parameter() {
        let rb = RingBuffer::new();
        for i in 0..10 {
            rb.push(Stream::Stdout, format!("line-{i}"));
        }
        let last3 = rb.lines(Some(3), StreamFilter::All);
        assert_eq!(last3.len(), 3);
        assert_eq!(last3[0].content, "line-7");
        assert_eq!(last3[1].content, "line-8");
        assert_eq!(last3[2].content, "line-9");
    }

    #[test]
    fn stream_filtering() {
        let rb = RingBuffer::new();
        rb.push(Stream::Stdout, "out-1".into());
        rb.push(Stream::Stderr, "err-1".into());
        rb.push(Stream::Stdout, "out-2".into());
        rb.push(Stream::Stderr, "err-2".into());

        let only_out = rb.lines(None, StreamFilter::Stdout);
        assert_eq!(only_out.len(), 2);
        assert!(only_out.iter().all(|l| l.stream == Stream::Stdout));

        let only_err = rb.lines(None, StreamFilter::Stderr);
        assert_eq!(only_err.len(), 2);
        assert!(only_err.iter().all(|l| l.stream == Stream::Stderr));
    }

    #[test]
    fn display_format() {
        let line = LogLine {
            timestamp: "2025-03-12T10:15:32.123Z".parse::<DateTime<Utc>>().unwrap(),
            stream: Stream::Stdout,
            content: "content here".into(),
        };
        assert_eq!(
            line.to_string(),
            "[2025-03-12T10:15:32.123Z stdout] content here"
        );
    }
}
