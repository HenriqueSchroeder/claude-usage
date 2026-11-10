use crate::pricing::Tokens;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use time::{Duration, OffsetDateTime};

/// How long a transcript may go without an assistant turn before the session
/// behind it counts as finished.
///
/// `discover_active` returns the newest transcript whether or not anything is
/// still happening in it, so without this the panel presents Friday's project
/// as Monday's current session. Claude Code appends an assistant line on every
/// turn, so a session actually being worked on is never quiet for long; half an
/// hour survives a lunch break taken mid-task and still clears overnight.
const IDLE_CUTOFF: Duration = Duration::minutes(30);

#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    pub project: String,
    pub session_id: String,
    pub started_at: Option<OffsetDateTime>,
    pub last_at: Option<OffsetDateTime>,
    /// Every model's tokens, summed separately, because one session can mix
    /// them and they are not priced alike.
    pub by_model: HashMap<String, Tokens>,
    /// The same tokens again, summed across models. The menu's token line is
    /// model-agnostic; only the cost needs the split.
    pub tokens: Tokens,
    pub model: String,
}

impl SessionInfo {
    /// Whether this transcript is still being written to.
    ///
    /// A transcript with no assistant line at all has no activity to measure,
    /// so it is not active either — that is the brand-new file holding nothing
    /// but the user's first prompt, which would otherwise render as a live
    /// session with no project and no cost.
    pub fn is_active(&self, now: OffsetDateTime) -> bool {
        self.last_at.is_some_and(|t| now - t < IDLE_CUTOFF)
    }

    /// How long the session has been running: first assistant turn to last.
    pub fn elapsed(&self) -> Option<Duration> {
        self.started_at.zip(self.last_at).map(|(a, b)| b - a)
    }
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
    id: Option<String>,
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
    /// What each message id has already contributed, and under which model.
    /// Claude Code writes one line per content block of a response (thinking,
    /// text, each tool call), every one carrying the whole response's usage,
    /// so summing lines counted a three-block turn three times.
    counted: HashMap<String, (String, Tokens)>,
    notes: crate::diag::Notes,
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
            counted: HashMap::new(),
            notes: crate::diag::Notes::default(),
        }
    }

    fn note(&mut self, what: &str) {
        let msg = format!("transcript {}: {what}", self.path.display());
        self.notes.say(msg);
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
    ///
    /// Nothing here is fatal — a transcript that cannot be read leaves the
    /// last known info standing — but nothing here is silent either: an
    /// unreadable transcript used to be indistinguishable from an idle one.
    pub fn poll(&mut self) -> &SessionInfo {
        let file = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) => {
                self.note(&format!("cannot open: {e}"));
                return &self.info;
            }
        };
        let len = match file.metadata().map(|m| m.len()) {
            Ok(n) => n,
            // Treating a failed stat as len 0 would read as a truncation and
            // throw away everything accumulated so far.
            Err(e) => {
                self.note(&format!("cannot read size: {e}"));
                return &self.info;
            }
        };

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
            self.counted.clear();
        }

        let mut reader = BufReader::new(file);
        if let Err(e) = reader.seek(SeekFrom::Start(self.offset)) {
            self.note(&format!("cannot seek to {}: {e}", self.offset));
            return &self.info;
        }

        let mut consumed = self.offset;
        let mut buf = Vec::new();
        let mut unparsable = false;
        let mut failure = None;
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if !buf.ends_with(b"\n") {
                        break; // partial line: leave the offset before it
                    }
                    consumed += n as u64;
                    unparsable |= !self.fold(&buf);
                }
                Err(e) => {
                    failure = Some(format!("read failed: {e}"));
                    break;
                }
            }
        }
        self.offset = consumed;

        match failure {
            Some(f) => self.note(&f),
            // No count in the message: a transcript producing junk would
            // otherwise print a fresh line every time the tally moved.
            None if unparsable => self.note("skipped unparsable line(s)"),
            // A clean read forgets the last complaint, so the same problem
            // coming back is reported again rather than deduped away.
            None => self.notes.clear(),
        }
        &self.info
    }

    /// Folds one complete line in. `false` means the line was not JSON at
    /// all, which the caller reports; a well-formed line this module has no
    /// use for is not a problem and returns `true`.
    fn fold(&mut self, raw: &[u8]) -> bool {
        let Ok(line) = serde_json::from_slice::<RawLine>(raw) else {
            return false;
        };
        if line.kind.as_deref() != Some("assistant") {
            return true;
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
        let Some(msg) = line.message else { return true };
        if let Some(m) = msg.model {
            self.info.model = m;
        }
        if let Some(u) = msg.usage {
            let t = Tokens {
                input: u.input_tokens,
                output: u.output_tokens,
                cache_read: u.cache_read_input_tokens,
                cache_creation: u.cache_creation_input_tokens,
            };
            // A repeated id replaces what it counted before rather than adding
            // to it. Lines without an id are summed, as there is nothing to
            // tell them apart by.
            if let Some((model, old)) = msg.id.as_ref().and_then(|id| self.counted.get(id)) {
                self.info.tokens.sub(old);
                if let Some(m) = self.info.by_model.get_mut(model) {
                    m.sub(old);
                }
            }
            self.info.tokens.add(&t);
            let model = self.info.model.clone();
            self.info.by_model.entry(model.clone()).or_default().add(&t);
            if let Some(id) = msg.id {
                self.counted.insert(id, (model, t));
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use time::macros::datetime;

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
    fn a_response_split_over_several_lines_is_counted_once() {
        // Real transcripts write one line per content block, each carrying
        // the full usage of the response, under one message id.
        let with_id = |id: &str, out: u64| {
            line("claude-opus-5", out)
                .replace(r#""message":{"#, &format!(r#""message":{{"id":"{id}","#))
        };
        let p = scratch("dedupe");
        std::fs::write(
            &p,
            format!(
                "{}\n{}\n{}\n{}\n",
                with_id("msg_a", 10),
                with_id("msg_a", 10),
                with_id("msg_a", 12),
                with_id("msg_b", 5)
            ),
        )
        .unwrap();
        let mut t = Tailer::new(p.clone());
        let info = t.poll();
        // msg_a's last line (12) replaces its earlier ones; msg_b adds 5.
        assert_eq!(info.tokens.output, 17);
        assert_eq!(info.tokens.cache_read, 200);
        assert_eq!(info.by_model["claude-opus-5"].output, 17);
        std::fs::remove_file(p).ok();
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

    #[test]
    fn an_unreadable_transcript_is_reported_once_not_once_per_poll() {
        let mut t = Tailer::new("/nonexistent/x.jsonl".into());
        t.poll();
        let first = t.notes.last_message().map(str::to_string);
        assert!(
            first.as_deref().is_some_and(|m| m.contains("cannot open")),
            "an unreadable transcript must not look like an idle session: {first:?}"
        );

        // Same failure next poll: remembered, so the journal gets one line.
        t.poll();
        assert_eq!(t.notes.last_message().map(str::to_string), first);
    }

    #[test]
    fn junk_lines_are_reported_and_a_clean_read_clears_it() {
        let p = scratch("junkreport");
        std::fs::write(&p, format!("{{oops\n{}\n", line("claude-opus-5", 6))).unwrap();
        let mut t = Tailer::new(p.clone());
        t.poll();
        assert!(t
            .notes
            .last_message()
            .is_some_and(|m| m.contains("unparsable")));

        // Nothing new to read, nothing wrong: the complaint is withdrawn.
        t.poll();
        assert_eq!(t.notes.last_message(), None);
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn a_session_quiet_for_too_long_is_not_active() {
        let p = scratch("idle");
        std::fs::write(
            &p,
            format!(
                "{}\n",
                line_with_ts("claude-opus-5", "2026-09-15T14:43:12.045Z", 10)
            ),
        )
        .unwrap();
        let mut t = Tailer::new(p.clone());
        let info = t.poll().clone();

        // Ten minutes after the last turn: still the session in front of you.
        assert!(info.is_active(datetime!(2026-09-15 14:53:12 UTC)));
        // The next morning: Friday's project is not today's session.
        assert!(!info.is_active(datetime!(2026-09-16 09:00:00 UTC)));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn a_transcript_with_no_assistant_line_is_not_active() {
        let p = scratch("useronly");
        std::fs::write(&p, "{\"type\":\"user\"}\n{\"type\":\"user\"}\n").unwrap();
        let mut t = Tailer::new(p.clone());
        let info = t.poll().clone();

        assert!(info.last_at.is_none());
        assert!(!info.is_active(datetime!(2026-09-15 14:43:12 UTC)));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn tokens_are_split_per_model_as_well_as_totalled() {
        let p = scratch("bymodel");
        std::fs::write(
            &p,
            format!(
                "{}\n{}\n{}\n",
                line("claude-opus-5", 10),
                line("claude-opus-5", 5),
                line("claude-haiku-4-5", 3)
            ),
        )
        .unwrap();
        let mut t = Tailer::new(p.clone());
        let info = t.poll();

        // The menu's token line stays a plain total...
        assert_eq!(info.tokens.output, 18);
        // ...while the cost has the split it needs to price each model right.
        assert_eq!(info.by_model["claude-opus-5"].output, 15);
        assert_eq!(info.by_model["claude-haiku-4-5"].output, 3);
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn elapsed_spans_the_first_turn_to_the_last() {
        let p = scratch("elapsed");
        std::fs::write(
            &p,
            format!(
                "{}\n{}\n",
                line_with_ts("claude-opus-5", "2026-09-15T14:05:00.000Z", 10),
                line_with_ts("claude-opus-5", "2026-09-15T14:43:00.000Z", 5)
            ),
        )
        .unwrap();
        let mut t = Tailer::new(p.clone());
        assert_eq!(t.poll().elapsed().map(|d| d.whole_minutes()), Some(38));
        std::fs::remove_file(p).ok();
    }
}
