//! AI analytics: configuration, the guarded runner, logs, the setup wizard and the test.
//!
//! The analysis itself is done by `runner.mjs` (Claude Agent SDK) which loads the
//! `weekly-retro` skill. This module starts it, enforces a hard time limit by
//! killing the whole process group, and writes a log file for every run.

use crate::store;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, Utc};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const SKILL: &str = "weekly-retro";
const TEST_TOKEN: &str = "SUPERSIMPLE_OK";
const KEEP_LOGS: usize = 100;

// Files installed into ~/.supersimple/analytics (kept in sync on every run).
const ASSETS: &[(&str, &str)] = &[
    ("package.json", include_str!("../assets/analytics/package.json")),
    ("runner.mjs", include_str!("../assets/analytics/runner.mjs")),
    (".claude/skills/weekly-retro/SKILL.md", include_str!("../assets/analytics/skill/SKILL.md")),
    (
        ".claude/skills/weekly-retro/scripts/completed_work.py",
        include_str!("../assets/analytics/skill/scripts/completed_work.py"),
    ),
    (
        ".claude/skills/weekly-retro/scripts/transcripts.py",
        include_str!("../assets/analytics/skill/scripts/transcripts.py"),
    ),
];

// ---------------------------------------------------------------- config

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    /// Hard wall-clock limit for one run. The run is killed when it is reached.
    pub timeout_secs: u64,
    /// Maximum number of agent turns in one run.
    pub max_turns: u32,
    /// Maximum estimated spend for one run, in USD.
    pub max_budget_usd: f64,
    pub model: String,
    /// Where Claude Code transcripts live. Default: ~/.claude/projects
    pub transcripts_dir: Option<String>,
    /// Set when the setup test last passed.
    pub tested_ok_at: Option<DateTime<Utc>>,
}

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig {
            timeout_secs: 300,
            max_turns: 15,
            max_budget_usd: 1.0,
            model: "claude-opus-5-5".into(),
            transcripts_dir: None,
            tested_ok_at: None,
        }
    }
}

fn config_path() -> PathBuf {
    store::home().join("config.json")
}

pub fn load_config() -> AiConfig {
    fs::read_to_string(config_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_config(cfg: &AiConfig) -> Result<()> {
    fs::create_dir_all(store::home())?;
    fs::write(config_path(), serde_json::to_string_pretty(cfg)? + "\n")?;
    Ok(())
}

pub fn analytics_dir() -> PathBuf {
    store::home().join("analytics")
}

pub fn logs_dir() -> PathBuf {
    store::home().join("logs")
}

fn env_file() -> PathBuf {
    store::home().join("env")
}

/// KEY=VALUE lines from ~/.supersimple/env (API key etc.).
fn read_env_file() -> Vec<(String, String)> {
    let Ok(text) = fs::read_to_string(env_file()) else {
        return vec![];
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect()
}

fn write_env_var(key: &str, value: &str) -> Result<()> {
    fs::create_dir_all(store::home())?;
    let mut vars: Vec<(String, String)> =
        read_env_file().into_iter().filter(|(k, _)| k != key).collect();
    vars.push((key.to_string(), value.to_string()));
    let body: String = vars.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    let path = env_file();
    fs::write(&path, body)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn has_api_key() -> bool {
    let set = |k: &str| std::env::var(k).map(|v| !v.is_empty()).unwrap_or(false);
    set("ANTHROPIC_API_KEY")
        || set("ANTHROPIC_AUTH_TOKEN")
        || read_env_file()
            .iter()
            .any(|(k, v)| (k == "ANTHROPIC_API_KEY" || k == "ANTHROPIC_AUTH_TOKEN") && !v.is_empty())
}

pub fn install_assets() -> Result<()> {
    let dir = analytics_dir();
    for (rel, body) in ASSETS {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap())?;
        if fs::read_to_string(&path).map(|cur| cur != *body).unwrap_or(true) {
            fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        }
    }
    Ok(())
}

fn sdk_installed() -> bool {
    analytics_dir().join("node_modules/@anthropic-ai/claude-agent-sdk/package.json").exists()
}

fn tool_version(cmd: &str, arg: &str) -> Option<String> {
    let out = Command::new(cmd).arg(arg).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(text.lines().next().unwrap_or("").trim().to_string())
}

fn transcripts_dir(cfg: &AiConfig) -> PathBuf {
    match &cfg.transcripts_dir {
        Some(d) => PathBuf::from(d),
        None => {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".claude/projects")
        }
    }
}

// ---------------------------------------------------------------- running

#[derive(Debug, Clone, Copy)]
pub enum Mode {
    Test,
    Retro { days: u32 },
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Test => "test",
            Mode::Retro { .. } => "retro",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub ok: bool,
    pub text: String,
    pub error: Option<String>,
    pub log_path: PathBuf,
    pub cost_usd: Option<f64>,
    pub turns: Option<u64>,
    pub secs: f64,
    pub skills: Vec<String>,
}

impl Outcome {
    pub fn summary_line(&self) -> String {
        let mut parts = vec![format!("{:.0}s", self.secs)];
        if let Some(t) = self.turns {
            parts.push(format!("{t} turns"));
        }
        if let Some(c) = self.cost_usd {
            parts.push(format!("${c:.3}"));
        }
        match (&self.error, self.ok) {
            (_, true) => format!("ok · {}", parts.join(" · ")),
            (Some(e), false) => format!("failed: {e} · {}", parts.join(" · ")),
            (None, false) => format!("failed · {}", parts.join(" · ")),
        }
    }
}

struct Log {
    file: Option<File>,
    start: Instant,
}

impl Log {
    fn line(&mut self, tag: &str, text: &str) {
        if let Some(f) = &mut self.file {
            let mut text = text.trim_end();
            if text.len() > 200_000 {
                let mut cut = 200_000;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text = &text[..cut];
            }
            let _ = writeln!(f, "[+{:7.1}s] {tag}: {text}", self.start.elapsed().as_secs_f64());
        }
    }
}

fn new_log(mode: Mode) -> (PathBuf, Option<File>) {
    let dir = logs_dir();
    let _ = fs::create_dir_all(&dir);
    // keep the folder from growing forever
    if let Ok(rd) = fs::read_dir(&dir) {
        let mut names: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        names.sort();
        if names.len() >= KEEP_LOGS {
            for old in &names[..names.len() + 1 - KEEP_LOGS] {
                let _ = fs::remove_file(old);
            }
        }
    }
    let name = format!("ai-{}-{}.log", Local::now().format("%Y%m%d-%H%M%S"), mode.name());
    let path = dir.join(name);
    let file = File::create(&path).ok();
    (path, file)
}

fn kill_group(pid: u32) {
    // The runner is started in its own process group, so this also stops the
    // Claude process and any script it launched.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

enum Piped {
    Out(String),
    Err(String),
}

/// Run the AI runner with all limits applied. Never panics; failures are in the Outcome.
pub fn run(
    mode: Mode,
    cfg: &AiConfig,
    cancel: &AtomicBool,
    mut progress: impl FnMut(String),
) -> Outcome {
    let start = Instant::now();
    let (log_path, file) = new_log(mode);
    let mut log = Log { file, start };
    let mut out = Outcome {
        ok: false,
        text: String::new(),
        error: None,
        log_path,
        cost_usd: None,
        turns: None,
        secs: 0.0,
        skills: vec![],
    };
    if let Err(e) = run_inner(mode, cfg, cancel, &mut progress, &mut log, &mut out) {
        out.ok = false;
        out.error = Some(format!("{e:#}"));
    }
    out.secs = start.elapsed().as_secs_f64();
    log.line("end", &out.summary_line());
    out
}

fn run_inner(
    mode: Mode,
    cfg: &AiConfig,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(String),
    log: &mut Log,
    out: &mut Outcome,
) -> Result<()> {
    install_assets()?;
    if !sdk_installed() {
        bail!("the Claude Agent SDK is not installed yet - run `supersimple setup`");
    }
    let limit_secs = match mode {
        Mode::Test => cfg.timeout_secs.clamp(20, 90),
        Mode::Retro { .. } => cfg.timeout_secs.max(20),
    };
    let days = match mode {
        Mode::Retro { days } => days,
        Mode::Test => 7,
    };
    let payload = serde_json::json!({
        "timeout_secs": limit_secs,
        "max_turns": cfg.max_turns,
        "max_budget_usd": cfg.max_budget_usd,
        "model": cfg.model,
        "days": days,
    });
    log.line(
        "start",
        &format!(
            "mode={} limits: {}s wall clock, {} turns, ${} · model={} · days={}",
            mode.name(),
            limit_secs,
            cfg.max_turns,
            cfg.max_budget_usd,
            cfg.model,
            days
        ),
    );

    let dir = analytics_dir();
    let mut cmd = Command::new("node");
    cmd.arg(dir.join("runner.mjs"))
        .arg(mode.name())
        .current_dir(&dir)
        .env("SUPERSIMPLE_AI", payload.to_string())
        .env("SUPERSIMPLE_HOME", store::home())
        .env("SUPERSIMPLE_TRANSCRIPTS_DIR", transcripts_dir(cfg))
        // do not let a surrounding Claude Code session leak into this run
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for (k, v) in read_env_file() {
        if std::env::var_os(&k).is_none() {
            cmd.env(k, v); // values are never written to the log
        }
    }
    let mut child = cmd.spawn().context("could not start `node` - is Node.js installed?")?;
    let pid = child.id();

    let (tx, rx) = mpsc::channel::<Piped>();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let tx_out = tx.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx_out.send(Piped::Out(line)).is_err() {
                break;
            }
        }
    });
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(Piped::Err(line)).is_err() {
                break;
            }
        }
    });

    let limit = Duration::from_secs(limit_secs);
    let started = Instant::now();
    let mut final_msg: Option<Value> = None;
    let mut final_at: Option<Instant> = None;
    let mut stopped: Option<String> = None;
    let mut last_err: Vec<String> = vec![];

    let mut handle = |p: Piped,
                      log: &mut Log,
                      final_msg: &mut Option<Value>,
                      final_at: &mut Option<Instant>,
                      last_err: &mut Vec<String>| match p {
        Piped::Err(line) => {
            log.line("stderr", &line);
            last_err.push(line);
            if last_err.len() > 6 {
                last_err.remove(0);
            }
        }
        Piped::Out(line) => {
            log.line("out", &line);
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                return;
            };
            match v.get("type").and_then(Value::as_str) {
                Some("supersimple_progress") => {
                    progress(v["text"].as_str().unwrap_or("").to_string())
                }
                Some("supersimple_stderr") => {
                    last_err.push(v["data"].as_str().unwrap_or("").trim().to_string());
                    if last_err.len() > 6 {
                        last_err.remove(0);
                    }
                }
                Some("supersimple_final") => {
                    *final_msg = Some(v);
                    *final_at = Some(Instant::now());
                }
                _ => {}
            }
        }
    };

    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(p) => handle(p, log, &mut final_msg, &mut final_at, &mut last_err),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break, // both pipes closed: it exited
        }
        if cancel.load(Ordering::Relaxed) {
            stopped = Some("cancelled".into());
        } else if started.elapsed() >= limit {
            stopped = Some(format!("stopped: the {limit_secs}s time limit was reached"));
        }
        if stopped.is_some() {
            log.line("kill", stopped.as_deref().unwrap());
            kill_group(pid);
            break;
        }
        // The result is in; give the runner a moment to exit, then make sure it is gone.
        if final_at.is_some_and(|t| t.elapsed() > Duration::from_secs(3)) {
            kill_group(pid);
            break;
        }
    }
    let status = child.wait().ok();
    kill_group(pid); // anything left in the group
    let drain_until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < drain_until {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(p) => handle(p, log, &mut final_msg, &mut final_at, &mut last_err),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    log.line("exit", &format!("{status:?}"));

    if let Some(f) = &final_msg {
        out.cost_usd = f["cost_usd"].as_f64();
        out.turns = f["turns"].as_u64();
        out.skills = f["skills"]
            .as_array()
            .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
            .unwrap_or_default();
    }
    if let Some(reason) = stopped {
        bail!(reason);
    }
    let Some(f) = final_msg else {
        let hint = last_err.join(" | ");
        bail!(
            "the runner ended without a result{}",
            if hint.is_empty() { String::new() } else { format!(": {hint}") }
        );
    };
    if f["ok"].as_bool() == Some(true) {
        out.ok = true;
        out.text = f["text"].as_str().unwrap_or("").to_string();
        if f["denials"].as_u64().unwrap_or(0) > 0 {
            log.line("note", "some tool calls were denied by the permission rules");
        }
        Ok(())
    } else {
        let mut msg = f["error"].as_str().unwrap_or("unknown error").to_string();
        if !last_err.is_empty() {
            msg = format!("{msg} ({})", last_err.join(" | "));
        }
        bail!(msg)
    }
}

/// The small end-to-end test: SDK starts, login works, Claude answers, the skill is found.
pub fn run_test(cancel: &AtomicBool, progress: impl FnMut(String)) -> Outcome {
    let mut cfg = load_config();
    let mut out = run(Mode::Test, &cfg, cancel, progress);
    if out.ok && !out.text.contains(TEST_TOKEN) {
        out.ok = false;
        out.error = Some(format!("unexpected reply from Claude: {:?}", out.text));
    } else if out.ok && !out.skills.iter().any(|s| s == SKILL) {
        out.ok = false;
        out.error = Some(format!("Claude answered, but the `{SKILL}` skill was not loaded"));
    }
    cfg.tested_ok_at = if out.ok { Some(Utc::now()) } else { None };
    let _ = save_config(&cfg);
    out
}

// ---------------------------------------------------------------- last report

fn last_report_path() -> PathBuf {
    store::home().join("last-analysis.md")
}

/// Every successful analysis is kept here, one file each, named by its time.
pub fn retros_dir() -> PathBuf {
    store::home().join("retros")
}

fn retro_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(retros_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    files.sort();
    files.reverse(); // names start with the time, so this is newest first
    files
}

/// Save a finished analysis: as the latest one and into the history folder.
pub fn save_last_report(out: &Outcome) {
    let now = Local::now();
    let meta = format!(
        "{} · {}",
        now.format("%a %b %d %Y %H:%M"),
        out.summary_line().trim_start_matches("ok · ")
    );
    let dir = retros_dir();
    let _ = fs::create_dir_all(&dir);
    // an analysis made before the history existed: keep it as the first entry
    if retro_files().is_empty() {
        if let Ok(meta) = fs::metadata(last_report_path()) {
            let at: DateTime<Local> = meta.modified().map(Into::into).unwrap_or(now);
            let _ = fs::copy(last_report_path(), dir.join(format!("{}.md", at.format("%Y%m%d-%H%M%S-%3f"))));
        }
    }
    let content = format!("{meta}\n{}\n", out.text);
    let _ = fs::write(dir.join(format!("{}.md", now.format("%Y%m%d-%H%M%S-%3f"))), &content);
    let _ = fs::write(last_report_path(), content);
}

fn read_report(path: &std::path::Path) -> Option<(String, String)> {
    let text = fs::read_to_string(path).ok()?;
    let (meta, body) = text.split_once('\n')?;
    Some((meta.to_string(), body.trim().to_string()))
}

/// (meta line, report text) of every saved analysis, newest first.
pub fn load_reports() -> Vec<(String, String)> {
    let all: Vec<_> = retro_files().iter().filter_map(|p| read_report(p)).collect();
    if all.is_empty() {
        return read_report(&last_report_path()).into_iter().collect();
    }
    all
}

// ---------------------------------------------------------------- status + wizard

struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn checks(cfg: &AiConfig) -> Vec<Check> {
    let node = tool_version("node", "--version");
    let py = tool_version("python3", "--version");
    let tdir = transcripts_dir(cfg);
    vec![
        Check {
            name: "Node.js",
            ok: node.is_some(),
            detail: node.unwrap_or_else(|| "not found - install with `brew install node`".into()),
        },
        Check {
            name: "Python 3",
            ok: py.is_some(),
            detail: py.unwrap_or_else(|| "not found - install with `brew install python`".into()),
        },
        Check {
            name: "Skill + scripts",
            ok: analytics_dir().join(".claude/skills").join(SKILL).join("SKILL.md").exists(),
            detail: analytics_dir().display().to_string(),
        },
        Check {
            name: "Claude Agent SDK",
            ok: sdk_installed(),
            detail: if sdk_installed() { "installed".into() } else { "not installed".into() },
        },
        Check {
            name: "Credentials",
            ok: has_api_key() || tool_version("claude", "--version").is_some(),
            detail: if has_api_key() {
                "API key found".into()
            } else if tool_version("claude", "--version").is_some() {
                "no API key; will use your Claude Code login".into()
            } else {
                "no API key and no Claude Code login".into()
            },
        },
        Check {
            name: "Transcripts",
            ok: tdir.is_dir(),
            detail: tdir.display().to_string(),
        },
        Check {
            name: "Test run",
            ok: cfg.tested_ok_at.is_some(),
            detail: match cfg.tested_ok_at {
                Some(at) => {
                    format!("passed {}", at.with_timezone(&Local).format("%Y-%m-%d %H:%M"))
                }
                None => "not passed yet".into(),
            },
        },
    ]
}

fn mark(ok: bool) -> &'static str {
    if ok {
        "✓"
    } else {
        "✗"
    }
}

pub fn print_status() {
    let cfg = load_config();
    println!("Super Simple - AI analytics status\n");
    for c in checks(&cfg) {
        println!("  {} {:<17} {}", mark(c.ok), c.name, c.detail);
    }
    println!(
        "\n  Limits per run: {}s · {} turns · ${:.2} · model {}",
        cfg.timeout_secs, cfg.max_turns, cfg.max_budget_usd, cfg.model
    );
    println!("  Config: {}", config_path().display());
    println!("  Logs:   {}", logs_dir().display());
    println!("  Past analyses: {} in {}", load_reports().len(), retros_dir().display());
}

fn ask(prompt: &str) -> String {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut line = String::new();
    let _ = io::stdin().lock().read_line(&mut line);
    line.trim().to_string()
}

fn yes(prompt: &str) -> bool {
    matches!(ask(&format!("{prompt} [Y/n] ")).to_lowercase().as_str(), "" | "y" | "yes")
}

/// Read a line without showing it on screen.
fn read_secret(prompt: &str) -> String {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut s = String::new();
    if enable_raw_mode().is_err() {
        return ask("");
    }
    loop {
        match event::read() {
            Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => match k.code {
                KeyCode::Enter => break,
                KeyCode::Esc => {
                    s.clear();
                    break;
                }
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    s.clear();
                    break;
                }
                KeyCode::Backspace => {
                    s.pop();
                }
                KeyCode::Char(c) => s.push(c),
                _ => {}
            },
            Ok(Event::Paste(p)) => s.push_str(&p),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = disable_raw_mode();
    println!();
    s.trim().to_string()
}

fn step(n: usize, total: usize, name: &str) {
    print!("[{n}/{total}] {name:<18} ");
    let _ = io::stdout().flush();
}

/// Guided setup. Returns whether everything, including the test, passed.
pub fn setup_wizard() -> Result<bool> {
    const N: usize = 7;
    let mut cfg = load_config();
    println!("Super Simple - AI analytics setup");
    println!("Checks what the weekly AI analysis needs, helps add what is missing, then runs a test.\n");

    step(1, N, "Node.js");
    match tool_version("node", "--version") {
        Some(v) => println!("✓ {v}"),
        None => {
            println!("✗ not found");
            println!("      Install it with:  brew install node   then run this setup again.");
            return Ok(false);
        }
    }

    step(2, N, "Python 3");
    match tool_version("python3", "--version") {
        Some(v) => println!("✓ {v}"),
        None => {
            println!("✗ not found");
            println!("      Install it with:  brew install python   then run this setup again.");
            return Ok(false);
        }
    }

    step(3, N, "Skill + scripts");
    install_assets()?;
    println!("✓ installed in {}", analytics_dir().display());

    step(4, N, "Claude Agent SDK");
    if sdk_installed() {
        println!("✓ installed");
    } else {
        println!("✗ not installed");
        if !yes("      Install it now with npm (downloads @anthropic-ai/claude-agent-sdk)?") {
            println!("      Skipped. Run this setup again when you want to install it.");
            return Ok(false);
        }
        let status = Command::new("npm")
            .args(["install", "--no-fund", "--no-audit", "--loglevel=error"])
            .current_dir(analytics_dir())
            .status()
            .context("could not run npm")?;
        if !status.success() || !sdk_installed() {
            println!("      ✗ npm install failed (see the messages above).");
            return Ok(false);
        }
        println!("      ✓ installed");
    }

    step(5, N, "Credentials");
    if has_api_key() {
        println!("✓ API key found");
    } else {
        let claude = tool_version("claude", "--version").is_some();
        println!("no ANTHROPIC_API_KEY in your environment");
        if claude {
            println!("      1) Use your existing Claude Code login (nothing to enter)");
            println!("      2) Enter an Anthropic API key (saved to {} , readable only by you)", env_file().display());
        }
        let choice = if claude { ask("      Choose [1/2]: ") } else { "2".into() };
        if choice == "2" {
            let key = read_secret("      Paste the API key (hidden), then Enter: ");
            if key.is_empty() {
                println!("      No key entered.");
                return Ok(false);
            }
            write_env_var("ANTHROPIC_API_KEY", &key)?;
            println!("      ✓ saved");
        } else {
            println!("      ✓ using the Claude Code login");
        }
    }

    step(6, N, "Transcripts");
    let tdir = transcripts_dir(&cfg);
    if tdir.is_dir() {
        println!("✓ {}", tdir.display());
    } else {
        println!("! {} does not exist yet (the analysis will only use your tasks)", tdir.display());
    }

    step(7, N, "Test run");
    println!("asking Claude for a one-word reply (limit 90s)…");
    save_config(&cfg)?;
    let out = run_test(&AtomicBool::new(false), |p| println!("      … {p}"));
    cfg = load_config();
    println!("      {} {}", mark(out.ok), out.summary_line());
    println!("      log: {}", out.log_path.display());
    if out.ok {
        println!("\nAll set. In the editor press Ctrl+R for the weekly retrospective, then `a` for AI analytics.");
        println!(
            "Limits per run: {}s · {} turns · ${:.2} (change them in {})",
            cfg.timeout_secs,
            cfg.max_turns,
            cfg.max_budget_usd,
            config_path().display()
        );
    } else {
        println!("\nThe test did not pass. Common causes:");
        println!("  - Claude Code login missing or expired: run `claude auth login`, or choose the API key option here");
        println!("  - no network, or the API key is wrong");
        println!("Details are in the log file above. Run `supersimple setup` again after fixing.");
    }
    Ok(out.ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_analysis_is_kept_and_an_older_one_is_not_lost() {
        let home = std::env::temp_dir().join(format!("supersimple-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        std::env::set_var("SUPERSIMPLE_HOME", &home);
        assert!(load_reports().is_empty());
        // an analysis from before the history folder existed
        fs::write(home.join("last-analysis.md"), "Fri Nov 13 19:10 · 11s\nold report\n").unwrap();
        assert_eq!(load_reports().len(), 1);
        let out = |text: &str| Outcome {
            ok: true,
            text: text.into(),
            error: None,
            log_path: PathBuf::new(),
            cost_usd: Some(0.03),
            turns: Some(5),
            secs: 11.0,
            skills: vec![],
        };
        std::thread::sleep(Duration::from_millis(5));
        save_last_report(&out("second report"));
        std::thread::sleep(Duration::from_millis(5));
        save_last_report(&out("third report"));
        let texts: Vec<String> = load_reports().into_iter().map(|(_, t)| t).collect();
        assert_eq!(texts, ["third report", "second report", "old report"]);
        let _ = fs::remove_dir_all(&home);
    }
}
