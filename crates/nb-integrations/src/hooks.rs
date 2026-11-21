//! Claude Code settings helpers. Nothing here runs unless called.

use std::path::Path;

use serde_json::{Value, json};

use crate::Error;

const EVENTS: [&str; 9] = [
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionRequest",
    "Stop",
    "SubagentStart",
    "SubagentStop",
];

fn hook_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/claude/hook")
}

fn handler(port: u16) -> Value {
    json!({ "type": "http", "url": hook_url(port), "timeout": 2 })
}

fn statusline_command(exe: &Path) -> String {
    format!("\"{}\" --statusline", exe.display())
}

/// The `{"hooks": {...}}` fragment to merge into a Claude Code settings file.
pub fn claude_code_hook_snippet(port: u16) -> Value {
    let mut hooks = serde_json::Map::new();
    for event in EVENTS {
        hooks.insert(event.to_string(), json!([{ "matcher": "", "hooks": [handler(port)] }]));
    }
    json!({ "hooks": Value::Object(hooks) })
}

/// What happened to the `statusLine` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusLineOutcome {
    Installed,
    AlreadyInstalled,
    /// Another tool owns `statusLine`; it was left untouched.
    ForeignKept,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MergeReport {
    /// Hook events that received a naelbar entry.
    pub hooks_added: usize,
    pub statusline: StatusLineOutcome,
}

fn is_ours(entry: &Value, url: &str) -> bool {
    entry.get("hooks").and_then(Value::as_array).is_some_and(|hs| {
        hs.iter().any(|h| {
            h.get("type").and_then(Value::as_str) == Some("http")
                && h.get("url").and_then(Value::as_str) == Some(url)
        })
    })
}

fn statusline_is_ours(v: &Value) -> bool {
    v.get("type").and_then(Value::as_str) == Some("command")
        && v.get("command").and_then(Value::as_str).is_some_and(|c| {
            c.trim_end().ends_with("--statusline") && c.to_ascii_lowercase().contains("naelbar")
        })
}

/// Merges naelbar's HTTP hooks and status line into the settings file at
/// `path` without touching any other key. Creates the file if missing; before
/// changing an existing one keeps a `.bak` copy, and writes through a temp
/// file then a rename. Running it twice changes nothing the second time. A
/// `statusLine` owned by something else is kept and reported.
pub fn merge_claude_code_settings(path: &Path, port: u16, exe: &Path) -> Result<MergeReport, Error> {
    let existing = match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => None,
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let mut root: Value = match &existing {
        Some(s) => serde_json::from_str(s).map_err(|e| Error::Json(e.to_string()))?,
        None => json!({}),
    };
    let obj = root.as_object_mut().ok_or_else(|| Error::Json("settings root is not an object".into()))?;

    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().ok_or_else(|| Error::Json("\"hooks\" is not an object".into()))?;
    let url = hook_url(port);
    let mut hooks_added = 0;
    for event in EVENTS {
        let list = hooks.entry(event).or_insert_with(|| json!([]));
        let list = list
            .as_array_mut()
            .ok_or_else(|| Error::Json(format!("hooks.{event} is not an array")))?;
        if !list.iter().any(|g| is_ours(g, &url)) {
            list.push(json!({ "matcher": "", "hooks": [handler(port)] }));
            hooks_added += 1;
        }
    }

    let wanted = json!({ "type": "command", "command": statusline_command(exe) });
    let statusline = match obj.get("statusLine") {
        None | Some(Value::Null) => {
            obj.insert("statusLine".into(), wanted);
            StatusLineOutcome::Installed
        }
        Some(cur) if statusline_is_ours(cur) => {
            if *cur == wanted {
                StatusLineOutcome::AlreadyInstalled
            } else {
                obj.insert("statusLine".into(), wanted);
                StatusLineOutcome::Installed
            }
        }
        Some(_) => StatusLineOutcome::ForeignKept,
    };

    let report = MergeReport { hooks_added, statusline };
    if hooks_added == 0 && statusline != StatusLineOutcome::Installed {
        return Ok(report);
    }

    let text = serde_json::to_string_pretty(&root).map_err(|e| Error::Json(e.to_string()))?;
    if existing.is_some() {
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        std::fs::copy(path, bak)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".naelbar.tmp");
    std::fs::write(&tmp, text + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpfile(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nbint-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&d);
        d
    }

    fn bak(p: &Path) -> PathBuf {
        let mut b = p.as_os_str().to_owned();
        b.push(".bak");
        PathBuf::from(b)
    }

    fn read(p: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    const EXE: &str = r"C:\Apps\naelbar.exe";

    #[test]
    fn snippet_shape() {
        let s = claude_code_hook_snippet(47911);
        let h = &s["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(*h, json!({"type":"http","url":"http://127.0.0.1:47911/claude/hook","timeout":2}));
        assert_eq!(s["hooks"].as_object().unwrap().len(), 9);
    }

    #[test]
    fn merge_preserves_keys_and_is_idempotent() {
        let p = tmpfile("settings.json");
        let orig = json!({
            "model": "x",
            "permissions": { "allow": ["Bash(ls)"] },
            "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "echo mine" }] }], "Other": [1] }
        });
        std::fs::write(&p, orig.to_string()).unwrap();
        let r = merge_claude_code_settings(&p, 47911, Path::new(EXE)).unwrap();
        assert_eq!(r, MergeReport { hooks_added: 9, statusline: StatusLineOutcome::Installed });
        let v = read(&p);
        assert_eq!(v["model"], "x");
        assert_eq!(v["permissions"], orig["permissions"]);
        assert_eq!(v["hooks"]["Other"], json!([1]));
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "echo mine");
        assert_eq!(v["statusLine"], json!({"type":"command","command":"\"C:\\Apps\\naelbar.exe\" --statusline"}));
        assert_eq!(read(&bak(&p)), orig);

        let before = std::fs::read_to_string(&p).unwrap();
        let r = merge_claude_code_settings(&p, 47911, Path::new(EXE)).unwrap();
        assert_eq!(r, MergeReport { hooks_added: 0, statusline: StatusLineOutcome::AlreadyInstalled });
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before);
        assert!(!p.with_extension("json.naelbar.tmp").exists());
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(bak(&p));
    }

    #[test]
    fn foreign_statusline_is_kept() {
        let p = tmpfile("foreign.json");
        let foreign = json!({"type":"command","command":"ccusage statusline"});
        std::fs::write(&p, json!({ "statusLine": foreign }).to_string()).unwrap();
        let r = merge_claude_code_settings(&p, 47911, Path::new(EXE)).unwrap();
        assert_eq!(r, MergeReport { hooks_added: 9, statusline: StatusLineOutcome::ForeignKept });
        let v = read(&p);
        assert_eq!(v["statusLine"], foreign);
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 1);
        // Second run: nothing to do.
        let r = merge_claude_code_settings(&p, 47911, Path::new(EXE)).unwrap();
        assert_eq!(r, MergeReport { hooks_added: 0, statusline: StatusLineOutcome::ForeignKept });
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(bak(&p));
    }

    #[test]
    fn moved_exe_updates_own_statusline() {
        let p = tmpfile("moved.json");
        merge_claude_code_settings(&p, 1, Path::new(r"C:\Old\naelbar.exe")).unwrap();
        let r = merge_claude_code_settings(&p, 1, Path::new(EXE)).unwrap();
        assert_eq!(r, MergeReport { hooks_added: 0, statusline: StatusLineOutcome::Installed });
        assert!(read(&p)["statusLine"]["command"].as_str().unwrap().contains(r"C:\Apps"));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(bak(&p));
    }

    #[test]
    fn merge_creates_missing_and_rejects_invalid() {
        let p = tmpfile("new.json");
        let r = merge_claude_code_settings(&p, 1234, Path::new(EXE)).unwrap();
        assert_eq!(r.hooks_added, 9);
        assert!(!bak(&p).exists());
        let _ = std::fs::remove_file(&p);
        std::fs::write(&p, "{ not json").unwrap();
        assert!(matches!(merge_claude_code_settings(&p, 1234, Path::new(EXE)), Err(Error::Json(_))));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{ not json");
        let _ = std::fs::remove_file(&p);
    }
}
