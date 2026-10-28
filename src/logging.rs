//! Bounded diagnostics. Keep recent output instead of erasing a full log on overflow.
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
    process::Child,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{SystemTime, UNIX_EPOCH},
};

pub const LIMIT: u64 = 2 * 1024 * 1024;
const KEEP: u64 = LIMIT / 2;

pub fn write(file: &mut File, bytes: &[u8]) -> std::io::Result<()> {
    let bytes = &bytes[bytes.len().saturating_sub(KEEP as usize)..];
    let length = file.metadata()?.len();
    if length + bytes.len() as u64 > LIMIT {
        file.seek(SeekFrom::Start(length.saturating_sub(KEEP)))?;
        let mut tail = Vec::new();
        file.read_to_end(&mut tail)?;
        // Start at a complete line when possible.
        if let Some(newline) = tail.iter().position(|&b| b == b'\n') {
            tail.drain(..=newline);
        }
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(b"[FlowBar: older log output discarded at 2 MiB]\n")?;
        file.write_all(&tail)?;
    }
    file.seek(SeekFrom::End(0))?;
    file.write_all(bytes)
}

pub fn tail(path: &Path, limit: u64) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(limit)))?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn record(path: &Path, message: impl AsRef<str>) {
    // UI and worker threads can both record session events.
    static LOCK: Mutex<()> = Mutex::new(());
    let Ok(_guard) = LOCK.lock() else { return };
    let mut options = fs::OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    if let Ok(mut file) = options.open(path) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        // JSON escapes newlines, so each diagnostic remains one searchable record.
        let line = serde_json::json!({
            "time_unix_ms": now.as_millis(), "pid": std::process::id(),
            "message": message.as_ref()
        });
        let _ = write(&mut file, format!("{line}\n").as_bytes());
    }
}

/// Drain both pipes concurrently, with fixed-size buffers and a shared bounded file.
pub fn capture(child: &mut Child, log: &File) -> std::io::Result<Vec<JoinHandle<()>>> {
    let file = Arc::new(Mutex::new(log.try_clone()?));
    let mut threads = vec![];
    let streams: Vec<Box<dyn Read + Send>> = vec![
        Box::new(child.stdout.take().expect("stdout pipe")),
        Box::new(child.stderr.take().expect("stderr pipe")),
    ];
    for mut stream in streams {
        let file = file.clone();
        threads.push(thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if let Ok(mut file) = file.lock() {
                            let _ = write(&mut file, &buffer[..count]);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        }));
    }
    Ok(threads)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overflow_keeps_recent_output_and_remains_bounded() {
        let mut file = tempfile::tempfile().unwrap();
        for i in 0..600 {
            write(
                &mut file,
                format!("{i:04} {}\n", "x".repeat(8192)).as_bytes(),
            )
            .unwrap();
            assert!(file.metadata().unwrap().len() <= LIMIT);
        }
        file.rewind().unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert!(text.starts_with("[FlowBar:"));
        assert!(text.contains("0599 ") && text.contains("0598 "));
        assert!(!text.contains("0000 "));
    }
}
