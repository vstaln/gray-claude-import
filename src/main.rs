//! gray-claude-import — import Claude Code plugins into gray.
//!
//! `claude_import` (tool) or `/cc-import <path>` imports a Claude Code plugin
//! directory (local path or GitHub URL):
//!   commands/*.md → ~/.gray/claude-import/<plugin>/commands/ — invoked through
//!                   the single `/cc <plugin> <cmd>` dispatcher (slash commands
//!                   can't be registered at runtime)
//!   agents/*.md   → ~/.gray/subagents/agents/cc-<plugin>-<agent>.md
//!   skills/<s>/   → ~/.gray/skills/cc-<plugin>-<s>/
//!   .mcp.json     → reported as a merge snippet for ~/.gray/mcp.json
//!   hooks/hooks.json → translated to ~/.gray/claude-import/<plugin>/hooks.json
//!                   and executed by this sidecar: PreToolUse → tool/before,
//!                   PostToolUse → tool/after, UserPromptSubmit → prompt/context,
//!                   SessionStart → first prompt/context of each session.
//!                   Events with no gray equivalent (Stop, …) are reported.
//!
//! With no arguments it speaks gray's NDJSON wire protocol on stdio.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

use serde_json::{Value, json};

fn manifest() -> Value {
    json!({
        "name": "claude-import",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "2.0",
        "tools": [{
            "name": "claude_import",
            "description": "Import a Claude Code plugin directory into gray. Copies commands/*.md (runnable via /cc <plugin> <cmd>), agents/*.md (-> gray-subagents profiles cc-<plugin>-<agent>), skills/* (-> ~/.gray/skills/cc-*), prints the .mcp.json merge snippet for ~/.gray/mcp.json, and bridges hooks/hooks.json so this sidecar runs PreToolUse/PostToolUse/UserPromptSubmit/SessionStart hook commands. Argument: a local directory path or a GitHub URL like https://github.com/owner/repo[/tree/branch/subdir].",
            "parameters": {
                "type": "object",
                "properties": {
                    "path_or_github_url": {
                        "type": "string",
                        "description": "Local path to the Claude Code plugin directory, or a GitHub/git URL to clone."
                    }
                },
                "required": ["path_or_github_url"]
            }
        }],
        "commands": ["/cc", "/cc-import"],
        "hooks": ["tool/before", "tool/after", "prompt/context"],
    })
}

// --- paths ---------------------------------------------------------------

fn gray_home() -> PathBuf {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".gray")))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn state_root(home: &Path) -> PathBuf {
    home.join("claude-import")
}
fn agents_dir(home: &Path) -> PathBuf {
    home.join("subagents").join("agents")
}
fn skills_dir(home: &Path) -> PathBuf {
    home.join("skills")
}

/// Lowercase filesystem-safe slug: [a-z0-9_-], anything else folds to '-'.
fn safe_name(s: &str) -> String {
    let mut out = String::new();
    let mut dashed = true; // trims leading dashes
    for c in s.trim().chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c.to_ascii_lowercase());
            dashed = false;
        } else if !dashed {
            out.push('-');
            dashed = true;
        }
    }
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "x".into() } else { out }
}

fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)? {
        let e = e?;
        let (s, d) = (e.path(), dst.join(e.file_name()));
        if s.is_dir() {
            copy_dir(&s, &d)?;
        } else if s.is_file() {
            std::fs::copy(&s, &d)?;
        }
    }
    Ok(())
}

// --- source resolution ----------------------------------------------------

/// Split a URL into (repo_url, branch, subdir) honoring github /tree/ links.
fn parse_clone_url(input: &str) -> (String, Option<String>, Option<String>) {
    if input.starts_with("git@") {
        return (input.to_string(), None, None);
    }
    let trimmed = input.trim_end_matches('/');
    let (base, tail) = match trimmed.split_once("/tree/") {
        Some((b, t)) => (b.to_string(), Some(t)),
        None => (trimmed.to_string(), None),
    };
    let (branch, sub) = match tail {
        Some(t) => {
            let mut it = t.splitn(2, '/');
            (
                it.next().filter(|s| !s.is_empty()).map(str::to_string),
                it.next().filter(|s| !s.is_empty()).map(str::to_string),
            )
        }
        None => (None, None),
    };
    (base, branch, sub)
}

/// Local dir → canonical path; git/http URL → shallow clone into the state
/// cache (keyed by URL so re-imports reuse it). Friendly errors otherwise.
fn resolve_source(home: &Path, input: &str) -> Result<PathBuf, String> {
    let p = Path::new(input);
    if p.is_dir() {
        return p.canonicalize().map_err(|e| format!("{input}: {e}"));
    }
    if input.starts_with("http://") || input.starts_with("https://") || input.starts_with("git@") {
        let (repo, branch, sub) = parse_clone_url(input);
        let cache = state_root(home)
            .join(".cache")
            .join(format!("{}-{}", safe_name(&repo), &short_hash(input)[..8]));
        if !cache.join(".git").exists() {
            let _ = std::fs::remove_dir_all(&cache);
            let _ = std::fs::create_dir_all(cache.parent().unwrap());
            let mut c = Command::new("git");
            c.args(["clone", "--depth", "1"]);
            if let Some(b) = &branch {
                c.args(["--branch", b]);
            }
            c.arg(&repo).arg(&cache);
            let out = c.output().map_err(|e| format!("git clone failed: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "git clone failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
        }
        let src = match &sub {
            Some(s) => cache.join(s),
            None => cache,
        };
        if !src.is_dir() {
            return Err(format!(
                "cloned {repo} but subdirectory '{}' does not exist",
                sub.as_deref().unwrap_or("")
            ));
        }
        return src.canonicalize().map_err(|e| e.to_string());
    }
    Err(format!("{input}: not a directory or a git URL"))
}

/// Plugin name: .claude-plugin/plugin.json `name`, else the directory name.
fn plugin_name(src: &Path) -> String {
    for cand in [
        src.join(".claude-plugin").join("plugin.json"),
        src.join("plugin.json"),
    ] {
        if let Ok(s) = std::fs::read_to_string(&cand)
            && let Ok(v) = serde_json::from_str::<Value>(&s)
            && let Some(n) = v.get("name").and_then(Value::as_str)
        {
            return safe_name(n);
        }
    }
    safe_name(
        &src.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "plugin".into()),
    )
}

// --- hook bridge ----------------------------------------------------------

#[derive(Clone)]
struct Rule {
    matcher: String,
    command: String,
    timeout: u64,
}

#[derive(Default)]
struct Bridge {
    plugin: String,
    root: PathBuf,
    before: Vec<Rule>,
    after: Vec<Rule>,
    context: Vec<Rule>,
    session_start: Vec<Rule>,
    unmapped: Vec<String>,
}

fn rules_from(v: &Value, key: &str) -> Vec<Rule> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    Some(Rule {
                        matcher: r.get("matcher").and_then(Value::as_str).unwrap_or("").into(),
                        command: r.get("command").and_then(Value::as_str).unwrap_or("").into(),
                        timeout: r.get("timeout").and_then(Value::as_u64).unwrap_or(30),
                    })
                })
                .filter(|r| !r.command.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn load_bridges(home: &Path) -> Vec<Bridge> {
    let mut out = vec![];
    let Ok(dirs) = std::fs::read_dir(state_root(home)) else {
        return out;
    };
    for e in dirs.flatten() {
        let Ok(s) = std::fs::read_to_string(e.path().join("hooks.json")) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&s) else {
            continue;
        };
        out.push(Bridge {
            plugin: v.get("plugin").and_then(Value::as_str).unwrap_or("?").into(),
            root: v
                .get("plugin_root")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .unwrap_or_else(|| e.path()),
            before: rules_from(&v, "before"),
            after: rules_from(&v, "after"),
            context: rules_from(&v, "context"),
            session_start: rules_from(&v, "session_start"),
            unmapped: v
                .get("unmapped")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|u| u.get("event").and_then(Value::as_str).map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        });
    }
    out
}

/// Claude matcher is a regex on the tool name; empty matches everything.
/// An invalid regex falls back to substring matching so a bad import can
/// never crash the guard path.
fn matcher_matches(matcher: &str, tool_name: &str) -> bool {
    let m = matcher.trim();
    if m.is_empty() {
        return true;
    }
    match regex::Regex::new(m) {
        Ok(re) => re.is_match(tool_name),
        Err(_) => tool_name.contains(m),
    }
}

struct HookOut {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Run a claude hook command: `timeout <s> sh -c <cmd>` with the synthesized
/// claude stdin JSON on stdin. Falls back to untimed `sh` if `timeout` is
/// missing. Never panics; spawn failures become code -1.
fn run_hook(cmd: &str, timeout: u64, stdin_json: &Value, envs: &[(String, String)]) -> HookOut {
    let secs = timeout.clamp(1, 120).to_string();
    let build = |timed: bool| {
        let mut c = if timed {
            let mut c = Command::new("timeout");
            c.arg(&secs).arg("sh").arg("-c").arg(cmd);
            c
        } else {
            let mut c = Command::new("sh");
            c.arg("-c").arg(cmd);
            c
        };
        c.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in envs {
            c.env(k, v);
        }
        c
    };
    let mut child = match build(true).spawn().or_else(|_| build(false).spawn()) {
        Ok(c) => c,
        Err(e) => {
            return HookOut { code: -1, stdout: String::new(), stderr: e.to_string() };
        }
    };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(stdin_json.to_string().as_bytes());
    }
    match child.wait_with_output() {
        Ok(o) => HookOut {
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        },
        Err(e) => HookOut { code: -1, stdout: String::new(), stderr: e.to_string() },
    }
}

fn hook_envs(b: &Bridge, session: &Value) -> Vec<(String, String)> {
    vec![
        ("CLAUDE_PLUGIN_ROOT".into(), b.root.to_string_lossy().into_owned()),
        (
            "CLAUDE_PROJECT_DIR".into(),
            session
                .get("cwd")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        ),
    ]
}

/// Claude's blocking signals: exit code 2 (stderr = reason), or stdout JSON
/// {decision:"block",reason} / {hookSpecificOutput:{permissionDecision}}.
/// Returns Some(reason) when the tool should be denied.
fn claude_block(out: &HookOut) -> Option<String> {
    if out.code == 2 {
        let r = out.stderr.trim();
        return Some(if r.is_empty() { "blocked by claude hook".into() } else { r.into() });
    }
    let text = out.stdout.trim();
    // Whole stdout first, then each line bottom-up: hooks may log noise
    // around the decision JSON.
    let cands = std::iter::once(text).chain(text.lines().rev());
    for cand in cands {
        let Ok(v) = serde_json::from_str::<Value>(cand) else {
            continue;
        };
        if v.get("decision").and_then(Value::as_str) == Some("block") {
            return Some(
                v.get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("blocked by claude hook")
                    .into(),
            );
        }
        if let Some(hs) = v.get("hookSpecificOutput") {
            match hs.get("permissionDecision").and_then(Value::as_str) {
                Some("deny") => {
                    return Some(
                        hs.get("permissionDecisionReason")
                            .and_then(Value::as_str)
                            .unwrap_or("denied by claude hook")
                            .into(),
                    );
                }
                Some("ask") => {
                    return Some("claude hook requested interactive approval (unsupported)".into());
                }
                _ => {}
            }
        }
    }
    None
}

fn hook_stdin(event: &str, name: &str, args: &Value, session: &Value) -> Value {
    json!({
        "hook_event_name": event,
        "tool_name": name,
        "tool_input": args,
        "session_id": session.get("id").and_then(Value::as_str).unwrap_or(""),
        "cwd": session.get("cwd").and_then(Value::as_str).unwrap_or(""),
    })
}

fn tool_before(home: &Path, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let session = params.get("session").cloned().unwrap_or(json!({}));
    let stdin_json = hook_stdin(
        "PreToolUse",
        name,
        &params.get("args").cloned().unwrap_or(Value::Null),
        &session,
    );
    for b in load_bridges(home) {
        for r in &b.before {
            if !matcher_matches(&r.matcher, name) {
                continue;
            }
            let out = run_hook(&r.command, r.timeout, &stdin_json, &hook_envs(&b, &session));
            if let Some(reason) = claude_block(&out) {
                return json!({
                    "decision": "deny",
                    "reason": format!("claude-import[{}]: {reason}", b.plugin),
                });
            }
        }
    }
    json!({ "decision": "allow" })
}

fn tool_after(home: &Path, params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let session = params.get("session").cloned().unwrap_or(json!({}));
    let mut stdin_json = hook_stdin(
        "PostToolUse",
        name,
        &params.get("args").cloned().unwrap_or(Value::Null),
        &session,
    );
    stdin_json["tool_response"] = params.get("content").cloned().unwrap_or(Value::Null);
    stdin_json["is_error"] = params.get("is_error").cloned().unwrap_or(json!(false));
    for b in load_bridges(home) {
        for r in &b.after {
            if !matcher_matches(&r.matcher, name) {
                continue;
            }
            let out = run_hook(&r.command, r.timeout, &stdin_json, &hook_envs(&b, &session));
            if let Some(reason) = claude_block(&out) {
                // PostToolUse "block" shows the reason to the model.
                return json!({
                    "is_error": true,
                    "content": format!("claude-import[{}] hook: {reason}", b.plugin),
                });
            }
        }
    }
    json!({})
}

fn seen_sessions() -> &'static Mutex<HashSet<String>> {
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(HashSet::new()))
}

fn prompt_context(home: &Path, params: &Value) -> Value {
    let bridges = load_bridges(home);
    if bridges.is_empty() {
        return json!({ "text": "" });
    }
    let session = params.get("session").cloned().unwrap_or(json!({}));
    let sid = session
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let first = seen_sessions().lock().unwrap().insert(sid);

    let mut parts: Vec<String> = vec![];
    if first {
        let mut notes = vec![];
        for b in &bridges {
            let mut note = format!("claude-import[{}]: hooks bridged", b.plugin);
            if !b.unmapped.is_empty() {
                note.push_str(&format!("; no gray equivalent: {}", b.unmapped.join(", ")));
            }
            notes.push(note);
            let stdin_json = json!({
                "hook_event_name": "SessionStart",
                "session_id": session.get("id").and_then(Value::as_str).unwrap_or(""),
                "cwd": session.get("cwd").and_then(Value::as_str).unwrap_or(""),
            });
            for r in &b.session_start {
                let out = run_hook(&r.command, r.timeout, &stdin_json, &hook_envs(b, &session));
                let t = out.stdout.trim();
                if !t.is_empty() {
                    parts.push(t.chars().take(800).collect());
                }
            }
        }
        parts.insert(0, notes.join("\n"));
    }
    let stdin_json = json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": session.get("id").and_then(Value::as_str).unwrap_or(""),
        "cwd": session.get("cwd").and_then(Value::as_str).unwrap_or(""),
    });
    for b in &bridges {
        for r in &b.context {
            let out = run_hook(&r.command, r.timeout, &stdin_json, &hook_envs(b, &session));
            let t = out.stdout.trim();
            if !t.is_empty() {
                parts.push(t.chars().take(800).collect());
            }
        }
    }
    let mut text = parts.join("\n");
    if text.len() > 4000 {
        text.truncate(4000);
    }
    json!({ "text": text })
}

// --- import ---------------------------------------------------------------

fn md_files(dir: &Path) -> Vec<std::fs::DirEntry> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    e.path().is_file()
                        && e.path().extension().and_then(|x| x.to_str()) == Some("md")
                })
                .collect()
        })
        .unwrap_or_default()
}

fn stem_of(e: &std::fs::DirEntry) -> String {
    e.path()
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Translate claude hooks/hooks.json into this sidecar's bridge config and
/// return (config_json, bridged_count, skipped_notes).
fn translate_hooks(plugin: &str, src: &Path, raw: &Value) -> (Value, usize, Vec<String>) {
    let obj = raw.get("hooks").cloned().unwrap_or_else(|| raw.clone());
    let (mut before, mut after, mut context, mut start, mut unmapped) =
        (vec![], vec![], vec![], vec![], vec![]);
    let mut skipped = vec![];
    if let Some(map) = obj.as_object() {
        for (event, list) in map {
            let Some(entries) = list.as_array() else { continue };
            for e in entries {
                let matcher = e.get("matcher").and_then(Value::as_str).unwrap_or("");
                let Some(hooks) = e.get("hooks").and_then(Value::as_array) else {
                    continue;
                };
                for h in hooks {
                    if h.get("type").and_then(Value::as_str) != Some("command") {
                        skipped.push(format!("{event}: non-command hook type"));
                        continue;
                    }
                    let cmd = h.get("command").and_then(Value::as_str).unwrap_or("");
                    if cmd.is_empty() {
                        continue;
                    }
                    let rule = json!({
                        "matcher": matcher,
                        "command": cmd,
                        "timeout": h.get("timeout").and_then(Value::as_u64).unwrap_or(30),
                    });
                    match event.as_str() {
                        "PreToolUse" => before.push(rule),
                        "PostToolUse" => after.push(rule),
                        "UserPromptSubmit" => context.push(rule),
                        "SessionStart" => start.push(rule),
                        other => {
                            skipped.push(format!("{other}: no gray hook equivalent"));
                            unmapped.push(json!({
                                "event": other,
                                "matcher": matcher,
                                "command": cmd,
                                "reason": "no gray hook equivalent",
                            }));
                        }
                    }
                }
            }
        }
    }
    let bridged = before.len() + after.len() + context.len() + start.len();
    let cfg = json!({
        "plugin": plugin,
        "plugin_root": src.to_string_lossy(),
        "before": before,
        "after": after,
        "context": context,
        "session_start": start,
        "unmapped": unmapped,
    });
    (cfg, bridged, skipped)
}

fn cap_list(items: &[String]) -> String {
    if items.len() > 20 {
        format!("{} … (+{} more)", items[..20].join(", "), items.len() - 20)
    } else {
        items.join(", ")
    }
}

/// The whole import. Returns the per-artifact report text.
fn import_plugin(home: &Path, src: &Path) -> Result<String, String> {
    let name = plugin_name(src);
    let dest = state_root(home).join(&name);
    let mut lines = vec![format!(
        "imported claude plugin \"{name}\" from {}",
        src.display()
    )];
    let mut found = false;

    // commands/*.md — stored; run through the /cc dispatcher.
    let cmds: Vec<String> = {
        let dir = src.join("commands");
        let files = md_files(&dir);
        let mut names = vec![];
        if !files.is_empty() {
            found = true;
            let d = dest.join("commands");
            std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
            for f in &files {
                std::fs::copy(f.path(), d.join(f.file_name())).map_err(|e| e.to_string())?;
                names.push(stem_of(f));
            }
        }
        names
    };
    if cmds.is_empty() {
        lines.push("commands: none".into());
    } else {
        lines.push(format!(
            "commands: {} imported ({}) — run via /cc {} <cmd>",
            cmds.len(),
            cap_list(&cmds),
            name
        ));
    }

    // agents/*.md — gray-subagents profiles.
    let agents: Vec<String> = {
        let files = md_files(&src.join("agents"));
        let mut names = vec![];
        if !files.is_empty() {
            found = true;
            let d = agents_dir(home);
            std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
            for f in &files {
                let prof = format!("cc-{name}-{}.md", safe_name(&stem_of(f)));
                std::fs::copy(f.path(), d.join(prof)).map_err(|e| e.to_string())?;
                names.push(stem_of(f));
            }
        }
        names
    };
    if agents.is_empty() {
        lines.push("agents: none".into());
    } else {
        lines.push(format!(
            "agents: {} imported ({}) → ~/.gray/subagents/agents/cc-{name}-*.md",
            agents.len(),
            cap_list(&agents)
        ));
    }

    // skills/<s>/ — full directory copies, SKILL.md required.
    let skills: Vec<String> = {
        let dir = src.join("skills");
        let mut names = vec![];
        if dir.is_dir() {
            let d = skills_dir(home);
            for e in std::fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
                if !e.path().is_dir() {
                    continue;
                }
                let sname = safe_name(&e.file_name().to_string_lossy());
                if !e.path().join("SKILL.md").is_file() {
                    lines.push(format!("skills: skipped {sname} (no SKILL.md)"));
                    continue;
                }
                found = true;
                std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
                copy_dir(&e.path(), &d.join(format!("cc-{name}-{sname}")))
                    .map_err(|e| e.to_string())?;
                names.push(sname);
            }
        }
        names
    };
    if !skills.is_empty() {
        lines.push(format!(
            "skills: {} imported ({}) → ~/.gray/skills/cc-{name}-*/",
            skills.len(),
            cap_list(&skills)
        ));
    }

    // .mcp.json — print the merge snippet.
    let mcp = src.join(".mcp.json");
    if mcp.is_file() {
        found = true;
        let raw = std::fs::read_to_string(&mcp).unwrap_or_default();
        let snippet: String = match serde_json::from_str::<Value>(&raw) {
            Ok(v) => serde_json::to_string_pretty(&v).unwrap_or(raw),
            Err(_) => raw,
        };
        let snippet: String = snippet.chars().take(1500).collect();
        lines.push(format!(
            "mcp: merge this into ~/.gray/mcp.json (or `gray mcp add` each server):\n{snippet}"
        ));
    }

    // hooks/hooks.json — bridge config this sidecar executes.
    let hooks_file = src.join("hooks").join("hooks.json");
    if hooks_file.is_file() {
        found = true;
        let raw = std::fs::read_to_string(&hooks_file).unwrap_or_default();
        match serde_json::from_str::<Value>(&raw) {
            Ok(v) => {
                let (cfg, bridged, skipped) = translate_hooks(&name, src, &v);
                std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
                std::fs::write(dest.join("hooks.json"), cfg.to_string())
                    .map_err(|e| e.to_string())?;
                lines.push(format!(
                    "hooks: {bridged} bridged (PreToolUse→tool/before, PostToolUse→tool/after, UserPromptSubmit+SessionStart→prompt/context)"
                ));
                for s in skipped {
                    lines.push(format!("hooks: skipped {s}"));
                }
            }
            Err(_) => lines.push("hooks: skipped hooks/hooks.json (invalid JSON)".into()),
        }
    }

    if !found {
        return Err(format!(
            "{name}: no claude plugin artifacts found (expected commands/, agents/, skills/, .mcp.json, or hooks/hooks.json)"
        ));
    }
    let mut report = lines.join("\n");
    if report.len() > 7800 {
        report.truncate(7800);
        report.push_str("\n…(truncated)");
    }
    Ok(report)
}

// --- /cc dispatcher --------------------------------------------------------

/// List imported plugins and their command stems.
fn cc_plugins(home: &Path) -> Vec<(String, Vec<String>)> {
    let mut out = vec![];
    let Ok(dirs) = std::fs::read_dir(state_root(home)) else {
        return out;
    };
    for e in dirs.flatten() {
        let pname = e.file_name().to_string_lossy().into_owned();
        if pname.starts_with('.') || !e.path().is_dir() {
            continue;
        }
        let mut cmds: Vec<String> = md_files(&e.path().join("commands"))
            .iter()
            .map(stem_of)
            .collect();
        cmds.sort();
        out.push((pname, cmds));
    }
    out.sort();
    out
}

fn cc_dispatch(home: &Path, argv: &[&str]) -> Value {
    let plugins = cc_plugins(home);
    let Some((&pname, rest)) = argv.split_first() else {
        if plugins.is_empty() {
            return json!({ "text": "no claude plugins imported yet — /cc-import <path>" });
        }
        let list = plugins
            .iter()
            .map(|(p, c)| format!("  {p}: {}", c.join(", ")))
            .collect::<Vec<_>>()
            .join("\n");
        return json!({ "text": format!("imported claude plugins (/cc <plugin> <cmd>):\n{list}") });
    };
    let plugin = safe_name(pname);
    let Some((p, cmds)) = plugins.iter().find(|(p, _)| *p == plugin) else {
        let avail: Vec<&str> = plugins.iter().map(|(p, _)| p.as_str()).collect();
        return json!({ "text": format!("unknown plugin '{pname}' — imported: {}", avail.join(", ")) });
    };
    let Some((&cname, args)) = rest.split_first() else {
        return json!({ "text": format!("{p} commands: {} — run /cc {p} <cmd>", cmds.join(", ")) });
    };
    let Some(cmd) = cmds.iter().find(|c| c.as_str() == safe_name(cname)) else {
        return json!({ "text": format!("no command '{cname}' in {p} — available: {}", cmds.join(", ")) });
    };
    let file = state_root(home).join(p).join("commands").join(format!("{cmd}.md"));
    match std::fs::read_to_string(&file) {
        Ok(text) => {
            let prompt = text.replace("$ARGUMENTS", &args.join(" "));
            json!({ "prompt": prompt })
        }
        Err(e) => json!({ "text": format!("couldn't read {}: {e}", file.display()) }),
    }
}

// --- wire ------------------------------------------------------------------

fn call_tool(home: &Path, name: &str, args: &Value) -> Result<String, String> {
    match name {
        "claude_import" => {
            let input = args
                .get("path_or_github_url")
                .or_else(|| args.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if input.is_empty() {
                return Err("missing required argument: path_or_github_url".into());
            }
            let src = resolve_source(home, input)?;
            import_plugin(home, &src)
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

/// One request → `Some(reply)`, or `None` for notifications. The bool asks
/// the loop to exit after writing the reply.
fn handle(req: &Value) -> (Option<Value>, bool) {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let Some(id) = id else {
        return (None, method == "plugin/shutdown");
    };
    let home = gray_home();
    let result = match method {
        "plugin/manifest" => manifest(),
        "tool/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("args").cloned().unwrap_or(Value::Null);
            match call_tool(&home, name, &args) {
                Ok(text) => json!({ "content": text }),
                Err(text) => json!({ "content": text, "is_error": true }),
            }
        }
        "tool/before" => tool_before(&home, &params),
        "tool/after" => tool_after(&home, &params),
        "prompt/context" => prompt_context(&home, &params),
        "command/run" => {
            let argv: Vec<&str> = params
                .get("argv")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim_start_matches('/');
            match name {
                "cc" => cc_dispatch(&home, &argv),
                "cc-import" => match argv.first() {
                    Some(p) => match resolve_source(&home, p).and_then(|s| import_plugin(&home, &s))
                    {
                        Ok(t) => json!({ "text": t }),
                        Err(e) => json!({ "text": format!("cc-import: {e}") }),
                    },
                    None => json!({ "text": "usage: /cc-import <path-or-github-url>" }),
                },
                _ => json!({ "text": format!("usage: /cc <plugin> <cmd> · /cc-import <path>") }),
            }
        }
        "plugin/shutdown" => return (Some(json!({ "id": id, "result": {} })), true),
        _ => {
            let error = json!({ "code": -32601, "message": "method not found" });
            return (Some(json!({ "id": id, "error": error })), false);
        }
    };
    (Some(json!({ "id": id, "result": result })), false)
}

fn main() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("manifest") {
        println!("{}", manifest());
        return Ok(());
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let (reply, exit) = handle(&req);
        if let Some(reply) = reply {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
        if exit {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(method: &str, params: Value) -> Value {
        handle(&json!({ "id": 1, "method": method, "params": params })).0.unwrap()
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "gray-ci-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn demo_plugin(src: &Path) {
        std::fs::create_dir_all(src.join("commands")).unwrap();
        std::fs::write(src.join("commands/plan.md"), "plan it: $ARGUMENTS").unwrap();
        std::fs::create_dir_all(src.join("agents")).unwrap();
        std::fs::write(src.join("agents/rev.md"), "you are a reviewer").unwrap();
        std::fs::create_dir_all(src.join("skills/skillx")).unwrap();
        std::fs::write(src.join("skills/skillx/SKILL.md"), "---\nname: skillx\n---\n").unwrap();
        std::fs::write(src.join(".mcp.json"), r#"{"mcpServers":{"s":{"command":"x"}}}"#).unwrap();
        std::fs::create_dir_all(src.join("hooks")).unwrap();
        std::fs::write(
            src.join("hooks/hooks.json"),
            r#"{"hooks":{
                "PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo ok"}]}],
                "Stop":[{"matcher":"","hooks":[{"type":"command","command":"echo bye"}]}]
            }}"#,
        )
        .unwrap();
    }

    #[test]
    fn manifest_claims_tool_commands_and_hooks() {
        let m = call("plugin/manifest", Value::Null)["result"].clone();
        assert_eq!(m["name"], "claude-import");
        assert_eq!(m["commands"], json!(["/cc", "/cc-import"]));
        assert_eq!(m["tools"][0]["name"], "claude_import");
        let hooks: Vec<&str> = m["hooks"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        for h in ["tool/before", "tool/after", "prompt/context"] {
            assert!(hooks.contains(&h), "missing hook {h}");
        }
    }

    #[test]
    fn safe_name_slugs() {
        assert_eq!(safe_name("My Plugin!"), "my-plugin");
        assert_eq!(safe_name("  --weird__x  "), "weird__x");
        assert_eq!(safe_name("!!!"), "x");
    }

    #[test]
    fn parse_clone_url_handles_tree_links() {
        let (repo, branch, sub) =
            parse_clone_url("https://github.com/o/r/tree/main/plugins/demo/");
            assert_eq!(repo, "https://github.com/o/r");
        assert_eq!(branch.as_deref(), Some("main"));
        assert_eq!(sub.as_deref(), Some("plugins/demo"));
        let (repo, branch, sub) = parse_clone_url("https://github.com/o/r");
        assert_eq!(repo, "https://github.com/o/r");
        assert!(branch.is_none() && sub.is_none());
    }

    #[test]
    fn import_copies_every_artifact() {
        let home = tmpdir("home");
        let src = tmpdir("src").join("demo");
        std::fs::create_dir_all(&src).unwrap();
        demo_plugin(&src);

        let report = import_plugin(&home, &src).unwrap();
        assert!(report.contains("\"demo\""), "{report}");
        assert!(report.contains("commands: 1 imported"), "{report}");
        assert!(report.contains("agents: 1 imported"), "{report}");
        assert!(report.contains("skipped Stop"), "{report}");

        assert!(state_root(&home).join("demo/commands/plan.md").exists());
        assert_eq!(
            std::fs::read_to_string(agents_dir(&home).join("cc-demo-rev.md")).unwrap(),
            "you are a reviewer"
        );
        assert!(skills_dir(&home).join("cc-demo-skillx/SKILL.md").exists());
        let bridge: Value = serde_json::from_str(
            &std::fs::read_to_string(state_root(&home).join("demo/hooks.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(bridge["before"][0]["command"], "echo ok");
        assert_eq!(bridge["before"][0]["matcher"], "Bash");
        assert_eq!(bridge["unmapped"][0]["event"], "Stop");
    }

    #[test]
    fn cc_dispatch_returns_prompt_with_arguments() {
        let home = tmpdir("home");
        let src = tmpdir("src").join("demo");
        std::fs::create_dir_all(&src).unwrap();
        demo_plugin(&src);
        import_plugin(&home, &src).unwrap();

        let r = cc_dispatch(&home, &["demo", "plan", "the", "thing"]);
        assert_eq!(r["prompt"], "plan it: the thing");
        let r = cc_dispatch(&home, &["demo"]);
        assert!(r["text"].as_str().unwrap().contains("plan"));
        let r = cc_dispatch(&home, &["nope", "plan"]);
        assert!(r["text"].as_str().unwrap().contains("unknown plugin"));
    }

    #[test]
    fn before_hook_blocks_on_decision_json() {
        let home = tmpdir("home");
        let cfg = json!({
            "plugin": "demo", "plugin_root": "/tmp",
            "before": [{
                "matcher": "Bash",
                "command": "echo '{\"decision\":\"block\",\"reason\":\"nope\"}'",
                "timeout": 5
            }],
            "after": [], "context": [], "session_start": [], "unmapped": []
        });
        let d = state_root(&home).join("demo");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("hooks.json"), cfg.to_string()).unwrap();

        let r = tool_before(&home, &json!({"name":"Bash","args":{},"session":{"id":"s","cwd":"/"}}));
        assert_eq!(r["decision"], "deny");
        assert!(r["reason"].as_str().unwrap().contains("nope"));

        let r = tool_before(&home, &json!({"name":"Read","args":{},"session":{}}));
        assert_eq!(r["decision"], "allow");
    }

    #[test]
    fn after_hook_block_marks_result_error() {
        let home = tmpdir("home");
        let cfg = json!({
            "plugin": "demo", "plugin_root": "/tmp",
            "before": [],
            "after": [{"matcher": "", "command": "echo '{\"decision\":\"block\",\"reason\":\"bad output\"}'", "timeout": 5}],
            "context": [], "session_start": [], "unmapped": []
        });
        let d = state_root(&home).join("demo");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("hooks.json"), cfg.to_string()).unwrap();

        let r = tool_after(&home, &json!({"name":"Bash","content":"x","session":{}}));
        assert_eq!(r["is_error"], true);
        assert!(r["content"].as_str().unwrap().contains("bad output"));
    }

    #[test]
    fn context_injects_hook_output_once_for_session_start() {
        let home = tmpdir("home");
        let cfg = json!({
            "plugin": "demo", "plugin_root": "/tmp",
            "before": [], "after": [],
            "context": [{"matcher":"","command":"echo every-call","timeout":5}],
            "session_start": [{"matcher":"","command":"echo first-only","timeout":5}],
            "unmapped": []
        });
        let d = state_root(&home).join("demo");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("hooks.json"), cfg.to_string()).unwrap();

        let s = json!({"session":{"id":"ctx-test-1","cwd":"/"}});
        let first = prompt_context(&home, &s);
        let t = first["text"].as_str().unwrap();
        assert!(t.contains("first-only") && t.contains("every-call"), "{t}");
        let second = prompt_context(&home, &s);
        let t2 = second["text"].as_str().unwrap();
        assert!(!t2.contains("first-only") && t2.contains("every-call"), "{t2}");
    }

    #[test]
    fn empty_state_fails_open() {
        let home = tmpdir("empty");
        assert_eq!(
            tool_before(&home, &json!({"name":"Bash","session":{}}))["decision"],
            "allow"
        );
        assert_eq!(tool_after(&home, &json!({"name":"Bash","session":{}})), json!({}));
        assert_eq!(prompt_context(&home, &json!({"session":{}}))["text"], "");
    }

    #[test]
    fn unknown_methods_and_notifications() {
        assert_eq!(call("nope", Value::Null)["error"]["code"], -32601);
        let (reply, exit) = handle(&json!({ "id": 2, "method": "plugin/shutdown" }));
        assert!(reply.is_some() && exit);
        let (reply, exit) = handle(&json!({
            "method": "event/notify", "params": {"type": "turn_end"}
        }));
        assert!(reply.is_none() && !exit);
    }

    #[test]
    fn tool_requires_a_path() {
        let r = call("tool/call", json!({ "name": "claude_import", "args": {} }));
        assert_eq!(r["result"]["is_error"], true);
    }
}
