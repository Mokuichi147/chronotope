use crate::command::Revision;
use chronotope_core::{Error, Result};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

pub trait RevisionLog: Send + Sync {
    fn append(&mut self, rev: &Revision) -> Result<()>;
    fn read_all(&self) -> Result<Vec<Revision>>;
}

#[derive(Default)]
pub struct MemoryLog {
    revisions: Vec<Revision>,
}

impl RevisionLog for MemoryLog {
    fn append(&mut self, rev: &Revision) -> Result<()> {
        self.revisions.push(rev.clone());
        Ok(())
    }
    fn read_all(&self) -> Result<Vec<Revision>> {
        Ok(self.revisions.clone())
    }
}

/// JSON Lines 形式の追記ログ。1 行 = 1 Revision。
pub struct JsonlLog {
    path: PathBuf,
    writer: BufWriter<File>,
    fsync: bool,
}

impl JsonlLog {
    pub fn open(path: &Path, fsync: bool) -> Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path).map_err(|e| Error::Storage(format!("open {}: {e}", path.display())))?;
        Ok(JsonlLog { path: path.to_path_buf(), writer: BufWriter::new(file), fsync })
    }
}

impl RevisionLog for JsonlLog {
    fn append(&mut self, rev: &Revision) -> Result<()> {
        let line = serde_json::to_string(rev).map_err(|e| Error::Storage(e.to_string()))?;
        let io = |e: std::io::Error| Error::Storage(format!("write {}: {e}", self.path.display()));
        self.writer.write_all(line.as_bytes()).map_err(io)?;
        self.writer.write_all(b"\n").map_err(io)?;
        self.writer.flush().map_err(io)?;
        if self.fsync {
            self.writer.get_ref().sync_data().map_err(io)?;
        }
        Ok(())
    }

    fn read_all(&self) -> Result<Vec<Revision>> {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(Error::Storage(e.to_string())),
        };
        let mut out = Vec::new();
        for (i, line) in BufReader::new(file).lines().enumerate() {
            let line = line.map_err(|e| Error::Storage(e.to_string()))?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Revision>(&line) {
                Ok(r) => out.push(r),
                // 末尾の書きかけ行（クラッシュ時）は捨てる。途中の破損はエラー。
                Err(e) => {
                    return Err(Error::Storage(format!("corrupt revision log line {}: {e}", i + 1)));
                }
            }
        }
        Ok(out)
    }
}
