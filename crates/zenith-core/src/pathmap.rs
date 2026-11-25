//! Path translation between a local mirror of a folder and the folder on
//! another machine (remote development): every string in a JSON message
//! (LSP, DAP) that starts with one side's root is rewritten to the other's.

use serde_json::Value;

/// Pairs of (local prefix, remote prefix): plain paths and `file://` URIs.
#[derive(Clone, Debug, Default)]
pub struct PathMap {
    pairs: Vec<(String, String)>,
}

fn starts_with_dir(s: &str, prefix: &str) -> bool {
    s.starts_with(prefix) && matches!(s[prefix.len()..].chars().next(), None | Some('/') | Some('\\'))
}

impl PathMap {
    /// `local_root` (a path on this machine) is `remote_root` (an absolute
    /// POSIX path) on the other one. `local_uri` / `remote_uri` are their
    /// `file://` URIs (percent-encoded), when the protocol uses URIs.
    pub fn new(local_root: &str, remote_root: &str, uris: Option<(&str, &str)>) -> Self {
        let trim = |s: &str| s.trim_end_matches(['/', '\\']).to_string();
        let mut pairs = vec![];
        if let Some((l, r)) = uris {
            pairs.push((trim(l), trim(r)));
        }
        pairs.push((trim(local_root), trim(remote_root)));
        Self { pairs }
    }

    /// A local path or URI as the remote side knows it.
    pub fn to_remote(&self, s: &str) -> Option<String> {
        for (l, r) in &self.pairs {
            if starts_with_dir(s, l) {
                return Some(format!("{r}{}", s[l.len()..].replace('\\', "/")));
            }
        }
        None
    }

    /// A remote path or URI as a local one.
    pub fn to_local(&self, s: &str) -> Option<String> {
        for (l, r) in &self.pairs {
            if starts_with_dir(s, r) {
                let rest = &s[r.len()..];
                // Plain Windows paths use backslashes; URIs keep slashes.
                let rest =
                    if cfg!(windows) && !l.starts_with("file:") { rest.replace('/', "\\") } else { rest.to_string() };
                return Some(format!("{l}{rest}"));
            }
        }
        None
    }

    fn map(&self, v: &mut Value, out: bool) {
        match v {
            Value::String(s) => {
                if let Some(m) = if out { self.to_remote(s) } else { self.to_local(s) } {
                    *s = m;
                }
            }
            Value::Array(a) => a.iter_mut().for_each(|x| self.map(x, out)),
            Value::Object(o) => o.values_mut().for_each(|x| self.map(x, out)),
            _ => {}
        }
    }

    /// Rewrite a message going to the remote side.
    pub fn outgoing(&self, v: &mut Value) {
        self.map(v, true);
    }

    /// Rewrite a message coming from the remote side.
    pub fn incoming(&self, v: &mut Value) {
        self.map(v, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maps_paths_and_uris_both_ways() {
        let m = PathMap::new(
            "/home/me/.config/zenith/remote/h/1/app",
            "/srv/app",
            Some(("file:///home/me/.config/zenith/remote/h/1/app", "file:///srv/app")),
        );
        let mut v = json!({
            "uri": "file:///home/me/.config/zenith/remote/h/1/app/src/main.rs",
            "rootPath": "/home/me/.config/zenith/remote/h/1/app",
            "other": "/home/me/.config/zenith/remote/h/1/application",
            "list": [{"source": {"path": "/home/me/.config/zenith/remote/h/1/app/x.py"}}],
            "n": 3
        });
        m.outgoing(&mut v);
        assert_eq!(v["uri"], "file:///srv/app/src/main.rs");
        assert_eq!(v["rootPath"], "/srv/app");
        assert_eq!(v["other"], "/home/me/.config/zenith/remote/h/1/application"); // not inside
        assert_eq!(v["list"][0]["source"]["path"], "/srv/app/x.py");
        m.incoming(&mut v);
        assert_eq!(v["uri"], "file:///home/me/.config/zenith/remote/h/1/app/src/main.rs");
        assert_eq!(m.to_local("/usr/lib/x.rs"), None);
    }
}
