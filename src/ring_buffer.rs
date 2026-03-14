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
    ProbeStdout,
    ProbeStderr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub timestamp: DateTime<Utc>,
    pub stream: Stream,
    pub content: String,
}

impl Stream {
    pub fn is_probe(self) -> bool {
        matches!(self, Stream::ProbeStdout | Stream::ProbeStderr)
    }
}

impl fmt::Display for LogLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let stream_label = match self.stream {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
            Stream::ProbeStdout => "probe:stdout",
            Stream::ProbeStderr => "probe:stderr",
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

    fn lines(
        &self,
        tail: Option<usize>,
        filter: StreamFilter,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
        grep: Option<&str>,
    ) -> Vec<LogLine> {
        let iter = self.buf.iter().filter(|l| {
            // Stream filter
            let stream_ok = match filter {
                StreamFilter::All => !l.stream.is_probe(),
                StreamFilter::Stdout => l.stream == Stream::Stdout,
                StreamFilter::Stderr => l.stream == Stream::Stderr,
                StreamFilter::Probe => l.stream.is_probe(),
                StreamFilter::ProbeStdout => l.stream == Stream::ProbeStdout,
                StreamFilter::ProbeStderr => l.stream == Stream::ProbeStderr,
            };
            if !stream_ok {
                return false;
            }
            // Time filters
            if let Some(ref s) = since {
                if l.timestamp < *s {
                    return false;
                }
            }
            if let Some(ref u) = until {
                if l.timestamp > *u {
                    return false;
                }
            }
            // Grep filter (case-insensitive)
            if let Some(pattern) = grep {
                if !pattern.is_empty()
                    && !l.content.to_lowercase().contains(&pattern.to_lowercase())
                {
                    return false;
                }
            }
            true
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

    pub fn lines(
        &self,
        tail: Option<usize>,
        filter: StreamFilter,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
        grep: Option<&str>,
    ) -> Vec<LogLine> {
        self.inner
            .lock()
            .unwrap()
            .lines(tail, filter, since, until, grep)
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

        let all = rb.lines(None, StreamFilter::All, None, None, None);
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
        let all = rb.lines(None, StreamFilter::All, None, None, None);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].content, "line-2");
        assert_eq!(all[1].content, "line-3");
        assert_eq!(all[2].content, "line-4");
    }

    #[test]
    fn eviction_at_byte_limit() {
        let rb = RingBuffer::with_capacity(usize::MAX, 25);
        rb.push(Stream::Stdout, "aaaaaaaaaa".into());
        rb.push(Stream::Stdout, "bbbbbbbbbb".into());
        rb.push(Stream::Stdout, "cccccccccc".into());

        let all = rb.lines(None, StreamFilter::All, None, None, None);
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
        let last3 = rb.lines(Some(3), StreamFilter::All, None, None, None);
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

        let only_out = rb.lines(None, StreamFilter::Stdout, None, None, None);
        assert_eq!(only_out.len(), 2);
        assert!(only_out.iter().all(|l| l.stream == Stream::Stdout));

        let only_err = rb.lines(None, StreamFilter::Stderr, None, None, None);
        assert_eq!(only_err.len(), 2);
        assert!(only_err.iter().all(|l| l.stream == Stream::Stderr));
    }

    #[test]
    fn since_filter() {
        let rb = RingBuffer::new();
        rb.push(Stream::Stdout, "old".into());
        std::thread::sleep(std::time::Duration::from_millis(50));
        let cutoff = Utc::now();
        std::thread::sleep(std::time::Duration::from_millis(50));
        rb.push(Stream::Stdout, "new".into());

        let filtered = rb.lines(None, StreamFilter::All, Some(cutoff), None, None);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].content, "new");
    }

    #[test]
    fn until_filter() {
        let rb = RingBuffer::new();
        rb.push(Stream::Stdout, "old".into());
        std::thread::sleep(std::time::Duration::from_millis(50));
        let cutoff = Utc::now();
        std::thread::sleep(std::time::Duration::from_millis(50));
        rb.push(Stream::Stdout, "new".into());

        let filtered = rb.lines(None, StreamFilter::All, None, Some(cutoff), None);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].content, "old");
    }

    #[test]
    fn since_until_window() {
        let rb = RingBuffer::new();
        rb.push(Stream::Stdout, "before".into());
        std::thread::sleep(std::time::Duration::from_millis(50));
        let start = Utc::now();
        std::thread::sleep(std::time::Duration::from_millis(50));
        rb.push(Stream::Stdout, "middle".into());
        std::thread::sleep(std::time::Duration::from_millis(50));
        let end = Utc::now();
        std::thread::sleep(std::time::Duration::from_millis(50));
        rb.push(Stream::Stdout, "after".into());

        let filtered = rb.lines(None, StreamFilter::All, Some(start), Some(end), None);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].content, "middle");
    }

    #[test]
    fn since_with_tail() {
        let rb = RingBuffer::new();
        rb.push(Stream::Stdout, "old".into());
        std::thread::sleep(std::time::Duration::from_millis(50));
        let cutoff = Utc::now();
        std::thread::sleep(std::time::Duration::from_millis(50));
        rb.push(Stream::Stdout, "new1".into());
        rb.push(Stream::Stdout, "new2".into());
        rb.push(Stream::Stdout, "new3".into());

        let filtered = rb.lines(Some(2), StreamFilter::All, Some(cutoff), None, None);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].content, "new2");
        assert_eq!(filtered[1].content, "new3");
    }

    #[test]
    fn grep_filter() {
        let rb = RingBuffer::new();
        rb.push(Stream::Stdout, "hello world".into());
        rb.push(Stream::Stdout, "foo bar".into());
        rb.push(Stream::Stdout, "Hello Again".into());

        let filtered = rb.lines(None, StreamFilter::All, None, None, Some("hello"));
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].content, "hello world");
        assert_eq!(filtered[1].content, "Hello Again");
    }

    #[test]
    fn grep_with_tail() {
        let rb = RingBuffer::new();
        rb.push(Stream::Stdout, "match1".into());
        rb.push(Stream::Stdout, "no".into());
        rb.push(Stream::Stdout, "match2".into());
        rb.push(Stream::Stdout, "match3".into());

        let filtered = rb.lines(Some(2), StreamFilter::All, None, None, Some("match"));
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].content, "match2");
        assert_eq!(filtered[1].content, "match3");
    }

    #[test]
    fn grep_case_insensitive() {
        let rb = RingBuffer::new();
        rb.push(Stream::Stdout, "ERROR: something".into());
        rb.push(Stream::Stdout, "error: another".into());
        rb.push(Stream::Stdout, "Error: mixed".into());
        rb.push(Stream::Stdout, "info: ok".into());

        let filtered = rb.lines(None, StreamFilter::All, None, None, Some("error"));
        assert_eq!(filtered.len(), 3);
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
