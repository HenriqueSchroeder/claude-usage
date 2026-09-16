use crate::pricing::Tokens;
use serde::Deserialize;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    pub project: String,
    pub session_id: String,
    pub started_at: Option<OffsetDateTime>,
    pub last_at: Option<OffsetDateTime>,
    pub tokens: Tokens,
    pub model: String,
}

#[derive(Deserialize)]
struct RawUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

#[derive(Deserialize)]
struct RawMessage {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<RawUsage>,
}

#[derive(Deserialize)]
struct RawLine {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    message: Option<RawMessage>,
}

pub struct Tailer {
    path: PathBuf,
    offset: u64,
    info: SessionInfo,
}

/// Finds the most recently modified session transcript.
pub fn discover_active(root: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for project in std::fs::read_dir(root).ok()?.flatten() {
        let Ok(entries) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(m) = e.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            if best.as_ref().is_none_or(|(bt, _)| m > *bt) {
                best = Some((m, p));
            }
        }
    }
    best.map(|(_, p)| p)
}

impl Tailer {
    pub fn new(path: PathBuf) -> Self {
        let session_id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        Tailer {
            path,
            offset: 0,
            info: SessionInfo {
                session_id,
                ..Default::default()
            },
        }
    }

    /// The transcript this tailer is following. Callers compare it against a
    /// fresh `discover_active` to notice the active session moved.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reads whatever was appended since the last call and folds it in.
    ///
    /// A trailing partial line is left unconsumed: the offset only advances
    /// past complete lines, so the next call sees the line whole. Without
    /// this, a line split across two writes would be dropped or
    /// double-counted.
    pub fn poll(&mut self) -> &SessionInfo {
        let Ok(file) = std::fs::File::open(&self.path) else {
            return &self.info;
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);

        if len < self.offset {
            self.offset = 0;
            // Reset the whole accumulated content state, not just tokens:
            // a rotated file starts a new session, so nothing derived from
            // the old file's lines (started_at, last_at, model, project)
            // should survive. session_id is derived from the path, not the
            // content, so it is carried over.
            self.info = SessionInfo {
                session_id: std::mem::take(&mut self.info.session_id),
                ..Default::default()
            };
        }

        let mut reader = BufReader::new(file);
        if reader.seek(SeekFrom::Start(self.offset)).is_err() {
            return &self.info;
        }

        let mut consumed = self.offset;
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if !buf.ends_with(b"\n") {
                        break; // partial line: leave the offset before it
                    }
                    consumed += n as u64;
                    self.fold(&buf);
                }
                Err(_) => break,
            }
        }
        self.offset = consumed;
        &self.info
    }

    fn fold(&mut self, raw: &[u8]) {
        let Ok(line) = serde_json::from_slice::<RawLine>(raw) else {
            return;
        };
        if line.kind.as_deref() != Some("assistant") {
            return;
        }
        if let Some(cwd) = line.cwd {
            if let Some(name) = Path::new(&cwd).file_name().and_then(|s| s.to_str()) {
                self.info.project = name.to_string();
            }
        }
        if let Some(ts) = line.timestamp.and_then(|t| {
            OffsetDateTime::parse(&t, &time::format_description::well_known::Rfc3339).ok()
        }) {
            self.info.started_at.get_or_insert(ts);
            self.info.last_at = Some(ts);
        }
        let Some(msg) = line.message else { return };
        if let Some(m) = msg.model {
            self.info.model = m;
        }
        if let Some(u) = msg.usage {
            self.info.tokens.add(&Tokens {
                input: u.input_tokens,
                output: u.output_tokens,
                cache_read: u.cache_read_input_tokens,
                cache_creation: u.cache_creation_input_tokens,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn line(model: &str, out: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"2026-09-15T14:43:12.045Z","cwd":"/home/x/proj",
                "message":{{"model":"{model}","usage":{{"input_tokens":2,"output_tokens":{out},
                "cache_read_input_tokens":100,"cache_creation_input_tokens":50}}}}}}"#
        )
        .replace('\n', "")
    }

    fn line_with_ts(model: &str, ts: &str, out: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","cwd":"/home/x/proj",
                "message":{{"model":"{model}","usage":{{"input_tokens":2,"output_tokens":{out},
                "cache_read_input_tokens":100,"cache_creation_input_tokens":50}}}}}}"#
        )
        .replace('\n', "")
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("cu-{}-{}.jsonl", name, std::process::id()));
        std::fs::remove_file(&p).ok();
        p
    }

    #[test]
    fn sums_tokens_from_assistant_lines() {
        let p = scratch("sum");
        std::fs::write(
            &p,
            format!(
                "{}\n{}\n",
                line("claude-opus-5", 10),
                line("claude-opus-5", 5)
            ),
        )
        .unwrap();
        let mut t = Tailer::new(p.clone());
        let info = t.poll();
        assert_eq!(info.tokens.output, 15);
        assert_eq!(info.tokens.cache_read, 200);
        assert_eq!(info.project, "proj");
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn appends_are_incremental_not_recounted() {
        let p = scratch("append");
        std::fs::write(&p, format!("{}\n", line("claude-opus-5", 10))).unwrap();
        let mut t = Tailer::new(p.clone());
        assert_eq!(t.poll().tokens.output, 10);

        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, "{}", line("claude-opus-5", 7)).unwrap();
        assert_eq!(t.poll().tokens.output, 17);
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn partial_trailing_line_is_not_consumed() {
        let p = scratch("partial");
        std::fs::write(
            &p,
            format!("{}\n{{\"type\":\"assis", line("claude-opus-5", 10)),
        )
        .unwrap();
        let mut t = Tailer::new(p.clone());
        assert_eq!(t.poll().tokens.output, 10);

        // completing the line later must count it exactly once
        std::fs::write(
            &p,
            format!(
                "{}\n{}\n",
                line("claude-opus-5", 10),
                line("claude-opus-5", 3)
            ),
        )
        .unwrap();
        assert_eq!(t.poll().tokens.output, 13);
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn truncation_resets_the_offset() {
        let p = scratch("trunc");
        std::fs::write(&p, format!("{}\n", line("claude-opus-5", 10))).unwrap();
        let mut t = Tailer::new(p.clone());
        assert_eq!(t.poll().tokens.output, 10);

        std::fs::write(&p, format!("{}\n", line("claude-opus-5", 4))).unwrap();
        assert_eq!(t.poll().tokens.output, 4);
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn truncation_resets_started_at() {
        let p = scratch("trunc_ts");
        std::fs::write(
            &p,
            format!(
                "{}\n",
                line_with_ts("claude-opus-5", "2026-09-15T14:43:12.045Z", 10)
            ),
        )
        .unwrap();
        let mut t = Tailer::new(p.clone());
        assert!(t.poll().started_at.is_some());

        // Shorter new content forces len < offset, i.e. a real truncation/rotation.
        std::fs::write(
            &p,
            format!(
                "{}\n",
                line_with_ts("claude-opus-5", "2026-09-16T09:00:00.000Z", 4)
            ),
        )
        .unwrap();
        let expected = OffsetDateTime::parse(
            "2026-09-16T09:00:00.000Z",
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert_eq!(t.poll().started_at, Some(expected));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn non_assistant_lines_are_skipped() {
        let p = scratch("skip");
        std::fs::write(
            &p,
            format!("{{\"type\":\"user\"}}\n{}\n", line("claude-opus-5", 9)),
        )
        .unwrap();
        let mut t = Tailer::new(p.clone());
        assert_eq!(t.poll().tokens.output, 9);
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn malformed_line_does_not_abort_the_rest() {
        let p = scratch("bad");
        std::fs::write(&p, format!("{{oops\n{}\n", line("claude-opus-5", 6))).unwrap();
        let mut t = Tailer::new(p.clone());
        assert_eq!(t.poll().tokens.output, 6);
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn missing_file_yields_empty_not_panic() {
        let mut t = Tailer::new("/nonexistent/x.jsonl".into());
        assert_eq!(t.poll().tokens.output, 0);
    }
}
