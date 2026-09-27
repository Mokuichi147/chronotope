use crate::command::Revision;
use chronotope_core::{Error, Result};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
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
        recover_tail(path)?;
        let file = OpenOptions::new().create(true).append(true).open(path).map_err(|e| Error::Storage(format!("open {}: {e}", path.display())))?;
        Ok(JsonlLog { path: path.to_path_buf(), writer: BufWriter::new(file), fsync })
    }
}

/// クラッシュで末尾に残った書きかけの行（改行で終わっていない行）を処理する。
/// 行として完結した Revision なら改行を補い、読めなければ切り捨てる。
/// 切り捨てた Revision はコミット完了（append の戻り）前だったので、呼び出し元には失敗として見えていた。
fn recover_tail(path: &Path) -> Result<()> {
    let io = |e: std::io::Error| Error::Storage(format!("recover {}: {e}", path.display()));
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io(e)),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(io)?;
    if bytes.is_empty() || bytes.ends_with(b"\n") {
        return Ok(());
    }
    let cut = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    if serde_json::from_slice::<Revision>(&bytes[cut..]).is_ok() {
        file.write_all(b"\n").map_err(io)?;
    } else {
        tracing::warn!(path = %path.display(), bytes = bytes.len() - cut, "discarding a partially written revision at the end of the log");
        file.set_len(cut as u64).map_err(io)?;
    }
    file.sync_all().map_err(io)?;
    Ok(())
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
                // 末尾の書きかけ行は open 時に処理済み。途中の破損はエラー。
                Err(e) => {
                    return Err(Error::Storage(format!("corrupt revision log line {}: {e}", i + 1)));
                }
            }
        }
        Ok(out)
    }
}
