//! Ringbuffer-achtige append-log voor sidecar-stderr met eenvoudige grootte-rotatie.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

pub struct LogRotator {
    dir: PathBuf,
    max_rotations: usize,
    max_bytes: u64,
}

impl LogRotator {
    /// `max_rotations` = aantal oude segmenten (`.1`, `.2`, …) dat we bewaren.
    /// `max_size_mb` = drempel (MiB) voordat we roteren.
    pub fn new(log_dir: PathBuf, max_rotations: usize, max_size_mb: usize) -> Self {
        Self {
            dir: log_dir,
            max_rotations: max_rotations.max(1),
            max_bytes: (max_size_mb as u64).saturating_mul(1024 * 1024),
        }
    }

    fn active_path(&self) -> PathBuf {
        self.dir.join("sidecar_stderr.log")
    }

    fn rotated_path(&self, n: usize) -> PathBuf {
        self.dir.join(format!("sidecar_stderr.log.{n}"))
    }

    fn rotate_if_needed(&self) -> std::io::Result<()> {
        if self.max_bytes == 0 {
            return Ok(());
        }
        let path = self.active_path();
        let len = match path.metadata() {
            Ok(m) => m.len(),
            Err(_) => return Ok(()),
        };
        if len < self.max_bytes {
            return Ok(());
        }

        let oldest = self.rotated_path(self.max_rotations);
        let _ = fs::remove_file(&oldest);

        for i in (1..self.max_rotations).rev() {
            let from = self.rotated_path(i);
            let to = self.rotated_path(i + 1);
            if from.exists() {
                let _ = fs::rename(&from, &to);
            }
        }

        if path.exists() {
            let first = self.rotated_path(1);
            let _ = fs::rename(&path, &first);
        }
        Ok(())
    }

    pub fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        self.rotate_if_needed()?;
        let path = self.active_path();
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(f, "{line}")?;
        f.flush()
    }

    pub fn read_recent(&self, max_lines: usize) -> Vec<String> {
        if max_lines == 0 {
            return Vec::new();
        }
        let path = self.active_path();
        let Ok(file) = fs::File::open(&path) else {
            return Vec::new();
        };
        let reader = BufReader::new(file);
        let mut buf = RecentLines::default();
        for line in reader.lines().map_while(Result::ok) {
            buf.push(line, max_lines);
        }
        buf.into_vec()
    }
}

#[derive(Default)]
struct RecentLines {
    lines: Vec<String>,
}

impl RecentLines {
    fn push(&mut self, line: String, max: usize) {
        if self.lines.len() >= max {
            self.lines.remove(0);
        }
        self.lines.push(line);
    }

    fn into_vec(self) -> Vec<String> {
        self.lines
    }
}
