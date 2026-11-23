//! Update check: asks GitHub for the latest release and tells the user when
//! it is newer. Nothing is downloaded or installed automatically.
//!
//! Uses the system `curl` (shipped with Windows 10+, macOS and Linux), so no
//! TLS stack is compiled into the editor.

use std::process::Command;
use std::sync::mpsc::{channel, Receiver};

pub const LATEST_API: &str = "https://api.github.com/repos/abbosjon-dev/code-editor/releases/latest";
pub const RELEASES_PAGE: &str = "https://github.com/abbosjon-dev/code-editor/releases";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
}

/// "v1.2.3", "1.2", "1.2.3-beta" → (1, 2, 3).
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let core = s.split(['-', '+']).next()?;
    let mut it = core.split('.').map(|p| p.parse::<u64>());
    let major = it.next()?.ok()?;
    let minor = it.next().unwrap_or(Ok(0)).ok()?;
    let patch = it.next().unwrap_or(Ok(0)).ok()?;
    Some((major, minor, patch))
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse_version(latest), parse_version(current)), (Some(l), Some(c)) if l > c)
}

/// Parse the `releases/latest` answer.
pub fn parse_release(json: &str) -> Option<Release> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    if v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        return None;
    }
    Some(Release {
        version: v["tag_name"].as_str()?.trim_start_matches('v').to_string(),
        url: v["html_url"].as_str().unwrap_or(RELEASES_PAGE).to_string(),
    })
}

/// The latest published release (None when there is none yet).
pub fn fetch_latest() -> Result<Option<Release>, String> {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sSL",
        "--max-time",
        "10",
        "-H",
        "Accept: application/vnd.github+json",
        "-H",
        "User-Agent: zenith-editor",
        "-w",
        "\n%{http_code}",
        LATEST_API,
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", &text));
    match code.trim() {
        "200" => Ok(parse_release(body)),
        "404" => Ok(None), // no releases published
        c => Err(format!("GitHub answered HTTP {c}")),
    }
}

/// Run [`fetch_latest`] in the background.
pub fn check_async(waker: impl Fn() + Send + 'static) -> Receiver<Result<Option<Release>, String>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(fetch_latest());
        waker();
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.2"), Some((0, 2, 0)));
        assert_eq!(parse_version("1.0.0-beta.1"), Some((1, 0, 0)));
        assert_eq!(parse_version("nightly"), None);
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.12"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    #[test]
    fn release_json() {
        let r = parse_release(
            r#"{"tag_name": "v0.3.0", "html_url": "https://x/r/v0.3.0", "draft": false, "prerelease": false}"#,
        );
        assert_eq!(r, Some(Release { version: "0.3.0".into(), url: "https://x/r/v0.3.0".into() }));
        assert_eq!(parse_release(r#"{"tag_name": "v1", "prerelease": true}"#), None);
        assert_eq!(parse_release("not json"), None);
    }
}
