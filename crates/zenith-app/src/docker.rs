//! Containers: what `docker ps -a` shows, grouped by Compose project, with
//! start / stop / restart / remove, logs and a shell (both in Run tabs).
//! Commands run locally or on the SSH host.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use crate::remote::RemoteSpec;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    /// running, exited, paused, restarting, created…
    pub state: String,
    /// "Up 3 minutes", "Exited (0) 2 hours ago".
    pub status: String,
    pub ports: String,
    /// Compose project (empty for plain containers).
    pub project: String,
}

impl Container {
    pub fn running(&self) -> bool {
        self.state == "running"
    }

    /// Host ports it publishes (`0.0.0.0:8090->80/tcp` → 8090).
    pub fn host_ports(&self) -> Vec<u16> {
        let mut v: Vec<u16> = self
            .ports
            .split(',')
            .filter_map(|p| p.split("->").next()?.rsplit(':').next()?.trim().parse().ok())
            .collect();
        v.dedup();
        v
    }
}

/// `docker ps -a --format '{{json .}}'`: one JSON object per line.
pub fn parse_ps(out: &str) -> Vec<Container> {
    let mut v: Vec<Container> = out
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .map(|j| {
            let s = |k: &str| j[k].as_str().unwrap_or_default().to_string();
            let project = s("Labels")
                .split(',')
                .find_map(|l| l.strip_prefix("com.docker.compose.project="))
                .unwrap_or_default()
                .to_string();
            Container {
                id: s("ID"),
                name: s("Names"),
                image: s("Image"),
                state: s("State"),
                status: s("Status"),
                ports: s("Ports"),
                project,
            }
        })
        .collect();
    // Projects together, running first, then by name.
    v.sort_by(|a, b| {
        (a.project.is_empty(), &a.project, !a.running(), &a.name).cmp(&(
            b.project.is_empty(),
            &b.project,
            !b.running(),
            &b.name,
        ))
    });
    v
}

/// The output (or error) of a `docker start|stop|…` run in the background.
pub type ActionResult = Receiver<Result<String, String>>;

pub enum Outcome {
    List(Vec<Container>),
    Error(String),
}

#[derive(Default)]
pub struct Docker {
    pub containers: Vec<Container>,
    pub error: Option<String>,
    pending: Option<Receiver<Outcome>>,
    last: Option<Instant>,
    /// An action finished: list again right away.
    dirty: bool,
}

impl Docker {
    /// List again (in the background) when it's time.
    pub fn poll(&mut self, remote: Option<&RemoteSpec>, root: &std::path::Path, every: Duration) {
        if let Some(rx) = &self.pending {
            if let Ok(o) = rx.try_recv() {
                self.pending = None;
                match o {
                    Outcome::List(c) => {
                        self.containers = c;
                        self.error = None;
                    }
                    Outcome::Error(e) => self.error = Some(e),
                }
            }
            return;
        }
        if !self.dirty && self.last.is_some_and(|t| t.elapsed() < every) {
            return;
        }
        self.dirty = false;
        self.last = Some(Instant::now());
        let (tx, rx) = channel();
        let (remote, root) = (remote.cloned(), root.to_path_buf());
        std::thread::spawn(move || {
            let o = match crate::devices::capture(remote.as_ref(), &root, "docker ps -a --format '{{json .}}'") {
                Ok(out) => Outcome::List(parse_ps(&out)),
                Err(e) => Outcome::Error(e),
            };
            let _ = tx.send(o);
        });
        self.pending = Some(rx);
    }

    /// `docker start|stop|restart|rm` in the background, then list again.
    pub fn action(
        &mut self,
        remote: Option<&RemoteSpec>,
        root: PathBuf,
        verb: &str,
        id: &str,
    ) -> Receiver<Result<String, String>> {
        let line = format!("docker {verb} {}", crate::remote::sh_quote(id));
        let (tx, rx) = channel();
        let remote = remote.cloned();
        std::thread::spawn(move || {
            let _ = tx.send(crate::devices::capture(remote.as_ref(), &root, &line));
        });
        self.dirty = true;
        self.last = None;
        rx
    }

    pub fn refresh(&mut self) {
        self.dirty = true;
    }
}

/// The Run command that follows a container's log.
pub fn logs_command(name: &str) -> String {
    format!("docker logs -f --tail 300 {}", crate::remote::sh_quote(name))
}

/// The Run command that opens a shell in a container (bash when it has one).
pub fn shell_command(name: &str) -> String {
    format!(
        "docker exec -it {} sh -c 'command -v bash >/dev/null && exec bash || exec sh'",
        crate::remote::sh_quote(name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_ps() {
        let out = r#"{"ID":"cdb6","Image":"alpine:3.20","Labels":"","Names":"old-job","Ports":"","State":"exited","Status":"Exited (0) 1 minute ago"}
{"ID":"58c1","Image":"alpine:3.20","Labels":"com.docker.compose.project=shop,com.docker.compose.service=worker","Names":"worker","Ports":"","State":"running","Status":"Up 1 minute"}
{"ID":"9c00","Image":"nginx","Labels":"com.docker.compose.project=shop","Names":"web1","Ports":"0.0.0.0:8090->80/tcp, :::8090->80/tcp","State":"running","Status":"Up 2 minutes"}
not json
"#;
        let c = parse_ps(out);
        assert_eq!(c.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["web1", "worker", "old-job"]);
        assert_eq!(c[0].project, "shop");
        assert_eq!(c[0].host_ports(), [8090]);
        assert!(c[0].running() && !c[2].running());
        assert!(shell_command("web 1").contains("'web 1'"));
    }
}
