//! Global text-only prompt history, separate from conversation replay.
//! OS file locking prevents concurrent sessions from interleaving JSON records.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const MAX_READ_BYTES: u64 = 4 * 1024 * 1024;
pub(crate) const MAX_ENTRIES: usize = 1000;

/// Keep disk I/O off the input thread, but drain accepted prompts before session exit.
pub struct HistoryWriter {
    sender: tokio::sync::mpsc::UnboundedSender<String>,
    stop: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl HistoryWriter {
    pub fn start(history: PromptHistory) -> Self {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<String>();
        let stop = tokio_util::sync::CancellationToken::new();
        let stopping = stop.clone();
        let task = tokio::spawn(async move {
            loop {
                let text = tokio::select! {
                    biased;
                    _ = stopping.cancelled() => {
                        receiver.close();
                        break;
                    }
                    text = receiver.recv() => text,
                };
                let Some(text) = text else { return };
                let writer = history.clone();
                let _ = tokio::task::spawn_blocking(move || writer.append(&text)).await;
            }
            while let Some(text) = receiver.recv().await {
                let writer = history.clone();
                let _ = tokio::task::spawn_blocking(move || writer.append(&text)).await;
            }
        });
        Self { sender, stop, task }
    }

    pub fn recorder(&self) -> std::sync::Arc<dyn Fn(String) + Send + Sync> {
        let sender = self.sender.clone();
        std::sync::Arc::new(move |text| {
            let _ = sender.send(text);
        })
    }

    pub async fn finish(self) {
        self.stop.cancel();
        let _ = self.task.await;
    }
}

#[derive(Serialize, Deserialize)]
struct Entry {
    session_id: String,
    ts: i64,
    text: String,
}

#[derive(Clone)]
pub struct PromptHistory {
    path: PathBuf,
    session_id: String,
    secrets: Vec<String>,
}

impl PromptHistory {
    pub fn new(data_dir: &Path, session_id: &str, secrets: Vec<String>) -> Self {
        Self { path: data_dir.join("history.jsonl"), session_id: session_id.into(), secrets }
    }

    pub fn load(&self) -> Result<Vec<String>> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(error) => return Err(error.into()),
        };
        file.lock_shared()?;
        let start = file.metadata()?.len().saturating_sub(MAX_READ_BYTES);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(MAX_READ_BYTES).read_to_end(&mut bytes)?;
        let text = String::from_utf8_lossy(&bytes);
        let mut entries: Vec<String> =
            text.lines().skip(usize::from(start > 0)).filter_map(|line| serde_json::from_str::<Entry>(line).ok()).map(|e| self.clean(&e.text)).filter(|s| !s.trim().is_empty()).collect();
        if entries.len() > MAX_ENTRIES {
            entries.drain(..entries.len() - MAX_ENTRIES);
        }
        Ok(entries)
    }

    fn clean(&self, text: &str) -> String {
        let text = crate::term::terminal_text(text);
        let text = crate::redact::redact_secrets(&text, &self.secrets);
        // Prior sessions cannot restore local image paths or placeholder ownership.
        let text = re!(r#"@"[^"]+\.(?i:png|jpe?g|webp|gif)""#).replace_all(&text, "[image omitted]");
        re!(r"\[(?:Image #\d+|Pasted text #\d+[^\]]*)\]").replace_all(&text, "[attachment omitted]").into_owned()
    }

    pub fn append(&self, text: &str) -> Result<()> {
        if text.trim().is_empty() || text.len() > 1024 * 1024 {
            return Ok(());
        }
        let text = self.clean(text);
        std::fs::create_dir_all(self.path.parent().expect("history parent"))?;
        let mut options = OpenOptions::new();
        // Windows LockFileEx requires a read/write handle (append-only handles fail).
        options.create(true).read(true).write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&self.path)?;
        file.lock()?;
        file.seek(SeekFrom::End(0))?;
        let mut record = serde_json::to_vec(&Entry { session_id: self.session_id.clone(), ts: crate::jsutil::now_millis(), text })?;
        record.push(b'\n');
        file.write_all(&record)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn immediate_shutdown_drains_prompts_even_with_a_live_recorder() {
        let dir = tempfile::tempdir().unwrap();
        let history = PromptHistory::new(dir.path(), "session", vec![]);
        let writer = HistoryWriter::start(history.clone());
        let record = writer.recorder();
        for n in 0..40 {
            record(format!("prompt {n}"));
        }
        tokio::time::timeout(std::time::Duration::from_secs(5), writer.finish()).await.unwrap();
        assert_eq!(history.load().unwrap(), (0..40).map(|n| format!("prompt {n}")).collect::<Vec<_>>());
        record("after shutdown".into());
        assert_eq!(history.load().unwrap().len(), 40);
    }

    #[test]
    fn loading_keeps_the_latest_thousand_entries() {
        let dir = tempfile::tempdir().unwrap();
        let history = PromptHistory::new(dir.path(), "session", vec![]);
        let records = (0..1010).map(|n| serde_json::to_string(&Entry { session_id: "session".into(), ts: n, text: format!("prompt {n}") }).unwrap()).collect::<Vec<_>>().join("\n");
        std::fs::write(&history.path, records).unwrap();
        let loaded = history.load().unwrap();
        assert_eq!(loaded.len(), MAX_ENTRIES);
        assert_eq!(loaded.first().unwrap(), "prompt 10");
        assert_eq!(loaded.last().unwrap(), "prompt 1009");
    }

    #[test]
    fn concurrent_sessions_roundtrip_multiline_and_skip_corrupt_records() {
        let dir = tempfile::tempdir().unwrap();
        let history = PromptHistory::new(dir.path(), "a", vec!["private-token-value".into()]);
        let handles: Vec<_> = (0..8)
            .map(|n| {
                let h = history.clone();
                std::thread::spawn(move || h.append(&format!("prompt {n}\nnext line")).unwrap())
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(history.load().unwrap().len(), 8);
        history.append("private-token-value @\"C:/secret/photo.png\"").unwrap();
        let mut file = OpenOptions::new().append(true).open(&history.path).unwrap();
        file.write_all(b"broken record\n").unwrap();
        let loaded = history.load().unwrap();
        assert_eq!(loaded.len(), 9);
        assert!(!loaded.last().unwrap().contains("private-token-value"));
        assert!(!loaded.last().unwrap().contains("C:/secret"));
    }
}
