//! Devices to run apps on: what `flutter devices` sees (phones, emulators,
//! simulators, desktop, Chrome), Android emulators (AVDs) and iOS
//! simulators. Lists are read with the tools' own CLIs, locally or on the
//! SSH host, on a background thread.

use std::path::Path;
use std::sync::mpsc::{channel, Receiver};

use crate::remote::RemoteSpec;

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// "android", "ios", "web", "linux", "macos", "windows", …
    pub platform: String,
    pub emulator: bool,
}

impl Device {
    pub fn icon(&self) -> &'static str {
        match self.platform.as_str() {
            "android" => "▲",
            "ios" => "◆",
            "web" => "◎",
            _ => "▭",
        }
    }

    pub fn label(&self) -> String {
        let kind = if self.emulator { " (emulator)" } else { "" };
        format!("{} {}{kind}", self.icon(), self.name)
    }
}

/// An emulator / simulator that can be started.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Emulator {
    pub id: String,
    pub name: String,
    /// "android" or "ios".
    pub platform: String,
    /// Already running (simulators only report this).
    pub booted: bool,
}

/// Flutter's "web-server" target: serves the app on a local URL, which the
/// web preview can show inside the editor.
pub fn web_server() -> Device {
    Device {
        id: "web-server".into(),
        name: "Web server (preview in Zenith)".into(),
        platform: "web".into(),
        emulator: false,
    }
}

/// `flutter devices --machine`.
pub fn parse_flutter_devices(json: &str) -> Vec<Device> {
    // Flutter may print a banner before the JSON.
    let start = json.find('[').unwrap_or(0);
    let v: serde_json::Value = serde_json::from_str(&json[start..]).unwrap_or_default();
    v.as_array()
        .into_iter()
        .flatten()
        .filter(|d| d["isSupported"].as_bool().unwrap_or(true))
        .filter_map(|d| {
            let tp = d["targetPlatform"].as_str().unwrap_or_default();
            let platform = if tp.starts_with("android") {
                "android"
            } else if tp.starts_with("ios") {
                "ios"
            } else if tp.starts_with("web") {
                "web"
            } else if tp.starts_with("darwin") {
                "macos"
            } else if tp.starts_with("windows") {
                "windows"
            } else if tp.starts_with("linux") {
                "linux"
            } else {
                tp
            };
            Some(Device {
                id: d["id"].as_str()?.to_string(),
                name: d["name"].as_str()?.to_string(),
                platform: platform.to_string(),
                emulator: d["emulator"].as_bool().unwrap_or(false),
            })
        })
        .collect()
}

/// `flutter emulators`: lines like `Pixel_8 • Pixel 8 • Google • android`.
pub fn parse_flutter_emulators(text: &str) -> Vec<Emulator> {
    text.lines()
        .filter_map(|l| {
            let parts: Vec<&str> = l.split('•').map(str::trim).collect();
            if parts.len() < 4 || parts[0].is_empty() || parts[0] == "Id" {
                return None;
            }
            Some(Emulator {
                id: parts[0].to_string(),
                name: parts[1].to_string(),
                platform: parts[3].to_string(),
                booted: false,
            })
        })
        .collect()
}

/// `emulator -list-avds`: one AVD name per line.
pub fn parse_avds(text: &str) -> Vec<Emulator> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains(' ') && !l.starts_with("INFO"))
        .map(|n| Emulator { id: n.to_string(), name: n.replace('_', " "), platform: "android".into(), booted: false })
        .collect()
}

/// `adb devices -l`: connected devices and running emulators.
pub fn parse_adb(text: &str) -> Vec<Device> {
    text.lines()
        .skip_while(|l| !l.starts_with("List of devices"))
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let id = it.next()?;
            if it.next()? != "device" {
                return None; // offline / unauthorized
            }
            let model = it.find_map(|w| w.strip_prefix("model:")).unwrap_or(id).replace('_', " ");
            Some(Device {
                id: id.into(),
                name: model,
                platform: "android".into(),
                emulator: id.starts_with("emulator-"),
            })
        })
        .collect()
}

/// `xcrun simctl list devices available -j`: iOS simulators.
pub fn parse_simctl(json: &str) -> Vec<Emulator> {
    let v: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
    let mut out = vec![];
    if let Some(runtimes) = v["devices"].as_object() {
        for (runtime, list) in runtimes {
            // com.apple.CoreSimulator.SimRuntime.iOS-18-0 → iOS 18.0
            let rt = runtime.rsplit('.').next().unwrap_or(runtime);
            if !rt.starts_with("iOS") {
                continue;
            }
            let ver = rt.trim_start_matches("iOS-").replace('-', ".");
            for d in list.as_array().into_iter().flatten() {
                if d["isAvailable"].as_bool() == Some(false) {
                    continue;
                }
                let (Some(id), Some(name)) = (d["udid"].as_str(), d["name"].as_str()) else { continue };
                out.push(Emulator {
                    id: id.into(),
                    name: format!("{name} (iOS {ver})"),
                    platform: "ios".into(),
                    booted: d["state"].as_str() == Some("Booted"),
                });
            }
        }
    }
    out.sort_by(|a, b| b.booted.cmp(&a.booted).then(a.name.cmp(&b.name)));
    out
}

/// Run a shell command line (locally, or on the SSH host in its folder)
/// and return its standard output.
pub fn capture(remote: Option<&RemoteSpec>, root: &Path, line: &str) -> Result<String, String> {
    let mut cmd = match remote {
        Some(r) => {
            let mut c = std::process::Command::new("ssh");
            c.args(crate::remote::ssh_args(r, false));
            c.arg(format!("cd {} && exec \"${{SHELL:-sh}}\" -lc {}", r.qpath(), crate::remote::sh_quote(line)));
            c
        }
        None if cfg!(windows) => {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", line]).current_dir(root);
            c
        }
        None => {
            let sh = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            let mut c = std::process::Command::new(sh);
            c.args(["-lc", line]).current_dir(root);
            c
        }
    };
    cmd.stdin(std::process::Stdio::null());
    let out = cmd.output().map_err(|e| e.to_string())?;
    if !out.status.success() && out.stdout.is_empty() {
        return Err(String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("failed").to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Everything found, from one scan.
#[derive(Clone, Debug, Default)]
pub struct Found {
    pub devices: Vec<Device>,
    pub emulators: Vec<Emulator>,
    /// Which tools were there ("flutter", "adb", "emulator", "simctl").
    pub tools: Vec<&'static str>,
}

/// Scan for devices and emulators on a thread.
pub fn scan(remote: Option<RemoteSpec>, root: std::path::PathBuf, flutter: bool) -> Receiver<Found> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let r = remote.as_ref();
        let mut f = Found::default();
        let has = |tool: &str| -> bool {
            capture(r, &root, &format!("command -v {tool}")).is_ok_and(|o| !o.trim().is_empty())
                || (r.is_none() && which::which(tool).is_ok())
        };
        if flutter && has("flutter") {
            f.tools.push("flutter");
            if let Ok(o) = capture(r, &root, "flutter devices --machine") {
                f.devices = parse_flutter_devices(&o);
            }
            if let Ok(o) = capture(r, &root, "flutter emulators") {
                f.emulators = parse_flutter_emulators(&o);
            }
        }
        if has("adb") {
            f.tools.push("adb");
            if let Ok(o) = capture(r, &root, "adb devices -l") {
                for d in parse_adb(&o) {
                    if !f.devices.iter().any(|x| x.id == d.id) {
                        f.devices.push(d);
                    }
                }
            }
        }
        if has("emulator") {
            f.tools.push("emulator");
            if let Ok(o) = capture(r, &root, "emulator -list-avds") {
                for e in parse_avds(&o) {
                    if !f.emulators.iter().any(|x| x.id == e.id) {
                        f.emulators.push(e);
                    }
                }
            }
        }
        if cfg!(target_os = "macos") || r.is_some() {
            if let Ok(o) = capture(r, &root, "xcrun simctl list devices available -j") {
                f.tools.push("simctl");
                for e in parse_simctl(&o) {
                    if let Some(x) = f.emulators.iter_mut().find(|x| x.platform == "ios" && x.name.starts_with(&e.name))
                    {
                        x.booted |= e.booted;
                    } else {
                        f.emulators.push(e);
                    }
                }
            }
        }
        let _ = tx.send(f);
    });
    rx
}

/// How much of a device's log to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    All,
    Warnings,
    Errors,
}

impl LogLevel {
    pub fn label(self) -> &'static str {
        match self {
            LogLevel::All => "Everything",
            LogLevel::Warnings => "Warnings and errors",
            LogLevel::Errors => "Errors only",
        }
    }
}

/// The app's Android package / iOS bundle id, from the project: Flutter's
/// android/app/build.gradle(.kts) or Expo's app.json.
pub fn app_id(dir: &Path, ios: bool) -> Option<String> {
    if let Ok(t) = std::fs::read_to_string(dir.join("app.json")) {
        let v: serde_json::Value = serde_json::from_str(&t).ok()?;
        let key = if ios { "/expo/ios/bundleIdentifier" } else { "/expo/android/package" };
        if let Some(id) = v.pointer(key).and_then(|x| x.as_str()) {
            return Some(id.to_string());
        }
    }
    if ios {
        return None;
    }
    for f in ["android/app/build.gradle.kts", "android/app/build.gradle"] {
        let Ok(t) = std::fs::read_to_string(dir.join(f)) else { continue };
        for l in t.lines() {
            let l = l.trim();
            if let Some(rest) = l.strip_prefix("applicationId") {
                let id = rest.trim().trim_start_matches('=').trim().trim_matches(['"', '\'']);
                if !id.is_empty() && !id.contains(' ') {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

/// The command that streams a device's log: `adb logcat` for Android,
/// `simctl … log stream` for an iOS simulator; only `app`'s messages when
/// given (it must be running for Android).
pub fn log_command(platform: &str, id: &str, level: LogLevel, app: Option<&str>) -> String {
    let q = crate::remote::sh_quote;
    if platform == "ios" {
        let lvl = match level {
            LogLevel::All => "debug",
            _ => "default",
        };
        let mut pred = vec![];
        if let Some(a) = app {
            pred.push(format!("subsystem BEGINSWITH \"{a}\" OR process == \"Runner\""));
        }
        match level {
            LogLevel::Errors => pred.push("messageType == error OR messageType == fault".into()),
            LogLevel::Warnings => pred.push("messageType >= default".into()),
            LogLevel::All => {}
        }
        let pred = if pred.is_empty() {
            String::new()
        } else {
            format!(" --predicate {}", q(&pred.iter().map(|p| format!("({p})")).collect::<Vec<_>>().join(" AND ")))
        };
        return format!("xcrun simctl spawn {} log stream --style compact --level {lvl}{pred}", q(id));
    }
    let filter = match level {
        LogLevel::All => "",
        LogLevel::Warnings => " '*:W'",
        LogLevel::Errors => " '*:E'",
    };
    let pid = app.map(|a| format!(" --pid=$(adb -s {} shell pidof -s {})", q(id), q(a))).unwrap_or_default();
    format!("adb -s {} logcat -v color{pid}{filter}", q(id))
}

/// The command that starts an emulator / simulator.
pub fn launch_command(e: &Emulator, tools: &[&str]) -> String {
    let q = crate::remote::sh_quote;
    match e.platform.as_str() {
        "ios" if e.id.len() == 36 => format!("xcrun simctl boot {} ; open -a Simulator", q(&e.id)),
        _ if tools.contains(&"flutter") && !tools.contains(&"emulator") => {
            format!("flutter emulators --launch {}", q(&e.id))
        }
        "android" => format!("emulator -avd {}", q(&e.id)),
        _ => format!("flutter emulators --launch {}", q(&e.id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flutter_devices_and_emulators() {
        let json = r#"Some banner
[{"name":"sdk gphone64","id":"emulator-5554","isSupported":true,"targetPlatform":"android-arm64","emulator":true},
 {"name":"iPhone 16","id":"ABCD","isSupported":true,"targetPlatform":"ios","emulator":true},
 {"name":"Chrome","id":"chrome","isSupported":true,"targetPlatform":"web-javascript","emulator":false},
 {"name":"Old","id":"x","isSupported":false,"targetPlatform":"android-arm","emulator":false}]"#;
        let d = parse_flutter_devices(json);
        assert_eq!(
            d.iter().map(|d| (d.id.as_str(), d.platform.as_str())).collect::<Vec<_>>(),
            [("emulator-5554", "android"), ("ABCD", "ios"), ("chrome", "web")]
        );
        assert!(d[0].emulator);
        let e = parse_flutter_emulators(
            "2 available emulators:\n\nId                    • Name                  • Manufacturer • Platform\n\nPixel_8_API_35 • Pixel 8 API 35 • Google • android\napple_ios_simulator • iOS Simulator • Apple • ios\n",
        );
        assert_eq!(e.len(), 2);
        assert_eq!((e[0].id.as_str(), e[1].platform.as_str()), ("Pixel_8_API_35", "ios"));
    }

    #[test]
    fn log_commands_and_app_ids() {
        assert_eq!(
            log_command("android", "emulator-5554", LogLevel::Errors, None),
            "adb -s emulator-5554 logcat -v color '*:E'"
        );
        assert_eq!(
            log_command("android", "R58M", LogLevel::All, Some("com.example.app")),
            "adb -s R58M logcat -v color --pid=$(adb -s R58M shell pidof -s com.example.app)"
        );
        let ios = log_command("ios", "ABCD", LogLevel::Errors, None);
        assert!(ios.starts_with("xcrun simctl spawn ABCD log stream") && ios.contains("messageType == error"));
        let d = std::env::temp_dir().join(format!("zenith-appid-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("android/app")).unwrap();
        std::fs::write(
            d.join("android/app/build.gradle.kts"),
            "android {\n  defaultConfig {\n    applicationId = \"uz.zenith.demo\"\n  }\n}\n",
        )
        .unwrap();
        assert_eq!(app_id(&d, false).as_deref(), Some("uz.zenith.demo"));
        std::fs::write(
            d.join("app.json"),
            r#"{"expo":{"android":{"package":"uz.expo.app"},"ios":{"bundleIdentifier":"uz.expo.ios"}}}"#,
        )
        .unwrap();
        assert_eq!(app_id(&d, false).as_deref(), Some("uz.expo.app"));
        assert_eq!(app_id(&d, true).as_deref(), Some("uz.expo.ios"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn parses_adb_avds_simctl() {
        let adb = "* daemon started\nList of devices attached\nemulator-5554 device product:sdk model:sdk_gphone64 device:emu64a\nR58M unauthorized usb:1\n";
        let d = parse_adb(adb);
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].name.as_str(), d[0].emulator), ("sdk gphone64", true));
        assert_eq!(parse_avds("Pixel_8_API_35\nMedium_Phone\n").len(), 2);
        let sim = r#"{"devices":{"com.apple.CoreSimulator.SimRuntime.iOS-18-0":[
            {"udid":"11111111-2222-3333-4444-555555555555","name":"iPhone 16","state":"Shutdown","isAvailable":true},
            {"udid":"22222222-2222-3333-4444-555555555555","name":"iPhone 16 Pro","state":"Booted","isAvailable":true}],
            "com.apple.CoreSimulator.SimRuntime.watchOS-11-0":[{"udid":"w","name":"Watch","state":"Shutdown"}]}}"#;
        let s = parse_simctl(sim);
        assert_eq!(
            s.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["iPhone 16 Pro (iOS 18.0)", "iPhone 16 (iOS 18.0)"]
        );
        assert!(s[0].booted);
        assert!(launch_command(&s[1], &["simctl"]).starts_with("xcrun simctl boot"));
        let avd = Emulator { id: "Pixel_8".into(), platform: "android".into(), ..Default::default() };
        assert_eq!(launch_command(&avd, &["emulator"]), "emulator -avd Pixel_8");
    }
}
