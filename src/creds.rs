use serde::Deserialize;
use std::path::Path;

#[derive(Debug)]
#[allow(dead_code)]
pub enum CredsError {
    Missing,
    Unreadable(String),
    Malformed(String),
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct CredsFile {
    #[serde(default, rename = "claudeAiOauth")]
    claude_ai_oauth: Option<OauthSection>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct OauthSection {
    #[serde(default, rename = "accessToken")]
    access_token: String,
}

/// Reads the Claude Code access token.
///
/// This function never writes. Claude Code refreshes the token in place and
/// a concurrent write from here would race it and can destroy the login.
#[allow(dead_code)]
pub fn read_access_token(path: &Path) -> Result<String, CredsError> {
    let raw = match std::fs::read(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(CredsError::Missing),
        Err(e) => return Err(CredsError::Unreadable(e.to_string())),
    };

    let parsed: CredsFile =
        serde_json::from_slice(&raw).map_err(|e| CredsError::Malformed(e.to_string()))?;

    match parsed.claude_ai_oauth {
        Some(o) if !o.access_token.is_empty() => Ok(o.access_token),
        _ => Err(CredsError::Malformed(
            "claudeAiOauth.accessToken absent".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    fn temp_file(contents: &str) -> std::path::PathBuf {
        let idx = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!("creds-test-{}-{}.json", std::process::id(), idx));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        p
    }

    #[test]
    fn reads_the_token() {
        let p = temp_file(r#"{"claudeAiOauth":{"accessToken":"abc123"}}"#);
        assert_eq!(read_access_token(&p).unwrap(), "abc123");
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn missing_file_is_missing_not_panic() {
        let p = std::path::Path::new("/nonexistent/creds.json");
        assert!(matches!(read_access_token(p), Err(CredsError::Missing)));
    }

    #[test]
    fn malformed_json_is_an_error_not_panic() {
        let p = temp_file("{not json");
        assert!(matches!(
            read_access_token(&p),
            Err(CredsError::Malformed(_))
        ));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn absent_oauth_section_is_malformed() {
        let p = temp_file(r#"{"mcpOAuth":{}}"#);
        assert!(matches!(
            read_access_token(&p),
            Err(CredsError::Malformed(_))
        ));
        std::fs::remove_file(p).ok();
    }
}
