use std::io::{self, BufRead, Write};

use crate::protocol::{LogsArgs, Request, Response, StreamFilter};

/// Read PSY_SOCK from the environment, returning a friendly error if unset.
fn sock_path() -> Result<String, String> {
    std::env::var("PSY_SOCK")
        .map_err(|_| "PSY_SOCK not set \u{2014} are you inside a psy session?".to_string())
}

// ---------------------------------------------------------------------------
// Platform-specific transport
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod transport {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    pub fn connect(path: &str) -> Result<(impl BufRead, impl Write), String> {
        let stream = UnixStream::connect(path)
            .map_err(|e| format!("Cannot connect to psy root at {path}: {e}"))?;
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| format!("clone error: {e}"))?,
        );
        Ok((reader, stream))
    }

    pub fn connect_streaming(path: &str) -> Result<(BufReader<UnixStream>, UnixStream), String> {
        let stream = UnixStream::connect(path)
            .map_err(|e| format!("Cannot connect to psy root at {path}: {e}"))?;
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| format!("clone error: {e}"))?,
        );
        Ok((reader, stream))
    }
}

#[cfg(windows)]
mod transport {
    use std::fs::OpenOptions;
    use std::io::{BufRead, BufReader, Write};

    pub fn connect(path: &str) -> Result<(impl BufRead, impl Write), String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("Cannot connect to psy root at {path}: {e}"))?;
        let reader_file = file.try_clone().map_err(|e| format!("clone error: {e}"))?;
        let reader = BufReader::new(reader_file);
        Ok((reader, file))
    }

    pub fn connect_streaming(
        path: &str,
    ) -> Result<(BufReader<std::fs::File>, std::fs::File), String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("Cannot connect to psy root at {path}: {e}"))?;
        let reader_file = file.try_clone().map_err(|e| format!("clone error: {e}"))?;
        let reader = BufReader::new(reader_file);
        Ok((reader, file))
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Send a single request to the root process and return its response.
pub fn send_command(request: Request) -> Result<Response, String> {
    let path = sock_path()?;
    let (mut reader, mut writer) = transport::connect(&path)?;

    let mut payload =
        serde_json::to_string(&request).map_err(|e| format!("serialize error: {e}"))?;
    payload.push('\n');
    writer
        .write_all(payload.as_bytes())
        .map_err(|e| format!("write error: {e}"))?;
    writer.flush().map_err(|e| format!("flush error: {e}"))?;

    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read error: {e}"))?;

    if line.is_empty() {
        return Err("Connection closed before response was received".to_string());
    }

    let response: Response =
        serde_json::from_str(&line).map_err(|e| format!("deserialize error: {e}"))?;

    Ok(response)
}

/// Follow logs for a named process, printing each line to stdout until the
/// connection is closed or the user presses Ctrl-C.
pub fn follow_logs(name: &str, stream: StreamFilter) -> Result<(), String> {
    let path = sock_path()?;
    let (mut reader, mut writer) = transport::connect_streaming(&path)?;

    let request = Request::logs_follow(LogsArgs {
        name: name.to_string(),
        tail: None,
        stream,
    });
    let mut payload =
        serde_json::to_string(&request).map_err(|e| format!("serialize error: {e}"))?;
    payload.push('\n');
    writer
        .write_all(payload.as_bytes())
        .map_err(|e| format!("write error: {e}"))?;
    writer.flush().map_err(|e| format!("flush error: {e}"))?;

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let _ = out.write_all(line.as_bytes());
                let _ = out.flush();
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    Ok(())
}
