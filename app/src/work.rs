//! What was done on a day, outside the task list: requests typed into Claude Code and commits.
//!
//! Claude Code keeps one JSON line per event in `~/.claude/projects/<folder>/<session>.jsonl`. The
//! format is internal and changes often, so this reads as little of it as possible: a line's
//! `type`, `cwd`, `timestamp`, `isSidechain`, `isMeta` and the text of the user's own messages.
//! Anything else in a line, or a line that does not parse, is skipped, never an error.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};

/// Requests shorter than this many words ("continue", "yes") say nothing about the work.
const MIN_WORDS: usize = 3;
/// Characters kept from one request: the start says what it was about.
const REQUEST_CHARS: usize = 300;
/// A pause longer than this between events ends a stretch of work.
const IDLE_MINUTES: i64 = 10;

/// One folder (a git repository when it is in one) with work on that day.
#[derive(Serialize, Debug, Clone, Default, PartialEq)]
pub struct Folder {
    pub path: String,
    /// What the user asked Claude Code to do, in order, without repeats.
    pub requests: Vec<String>,
    /// Commit subjects by this repository's user.
    pub commits: Vec<String>,
    /// Time with Claude Code events no more than `IDLE_MINUTES` apart; parallel sessions count once.
    pub active_minutes: i64,
}

/// Work on `day` (local time): every folder Claude Code worked in, plus `repos` checked for commits
/// even without a session.
pub fn day(day: NaiveDate, repos: &[String]) -> Vec<Folder> {
    let (start, end) = bounds(day);
    let mut by_root: HashMap<String, (Folder, Vec<DateTime<Utc>>)> = HashMap::new();
    let mut roots: HashMap<String, String> = HashMap::new();
    if let Some(dir) = sessions_dir() {
        for (cwd, activity) in read_sessions(&dir, start, end) {
            let root = roots.entry(cwd.clone()).or_insert_with(|| repo_root(&cwd).unwrap_or(cwd)).clone();
            let (folder, times) = by_root.entry(key(&root)).or_insert_with(|| (Folder { path: root, ..Default::default() }, vec![]));
            for r in activity.requests {
                if !folder.requests.contains(&r) {
                    folder.requests.push(r);
                }
            }
            times.extend(activity.times);
        }
    }
    for repo in repos {
        by_root.entry(key(repo)).or_insert_with(|| (Folder { path: repo.clone(), ..Default::default() }, vec![]));
    }
    let mut folders: Vec<Folder> = by_root
        .into_values()
        .filter_map(|(mut folder, mut times)| {
            times.sort();
            folder.active_minutes = active_minutes(&times);
            folder.commits = commits(&folder.path, day);
            (!folder.requests.is_empty() || !folder.commits.is_empty()).then_some(folder)
        })
        .collect();
    folders.sort_by(|a, b| a.path.cmp(&b.path));
    folders
}

/// Paths compare without case and with one kind of slash, as Windows does.
pub fn key(path: &str) -> String {
    path.trim_end_matches(['\\', '/']).replace('/', "\\").to_lowercase()
}

fn bounds(day: NaiveDate) -> (DateTime<Utc>, DateTime<Utc>) {
    let at = |d: NaiveDate| {
        let midnight = d.and_hms_opt(0, 0, 0).unwrap_or_default();
        // A day starting in a DST gap begins at the first time that exists.
        Local.from_local_datetime(&midnight).earliest().map_or_else(|| midnight.and_utc(), |t| t.with_timezone(&Utc))
    };
    (at(day), at(day.succ_opt().unwrap_or(day)))
}

fn sessions_dir() -> Option<PathBuf> {
    let base = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".claude")))?;
    Some(base.join("projects"))
}

#[derive(Default, Debug)]
struct Activity {
    requests: Vec<String>,
    times: Vec<DateTime<Utc>>,
}

/// cwd -> activity, from the session files written to since `start`.
fn read_sessions(dir: &Path, start: DateTime<Utc>, end: DateTime<Utc>) -> HashMap<String, Activity> {
    let mut out: HashMap<String, Activity> = HashMap::new();
    let since = SystemTime::from(start);
    let Ok(projects) = std::fs::read_dir(dir) else { return out };
    for project in projects.flatten() {
        // Only the folder's own files: subfolders hold subagents and tool output.
        let Ok(files) = std::fs::read_dir(project.path()) else { continue };
        for file in files.flatten() {
            let path = file.path();
            let fresh = file.metadata().and_then(|m| m.modified()).is_ok_and(|t| t >= since);
            if fresh && path.extension().is_some_and(|e| e == "jsonl") {
                if let Ok(f) = std::fs::File::open(&path) {
                    read_session(BufReader::new(f), start, end, &mut out);
                }
            }
        }
    }
    out
}

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: Option<String>,
    cwd: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "isSidechain", default)]
    sidechain: bool,
    #[serde(rename = "isMeta", default)]
    meta: bool,
    #[serde(rename = "toolUseResult")]
    tool_result: Option<serde::de::IgnoredAny>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    content: Option<serde_json::Value>,
}

fn read_session(file: impl BufRead, start: DateTime<Utc>, end: DateTime<Utc>, out: &mut HashMap<String, Activity>) {
    for line in file.lines() {
        let Ok(line) = line else { break };
        let Ok(e) = serde_json::from_str::<Event>(&line) else { continue };
        let (Some(kind), Some(cwd), Some(at)) = (e.kind.as_deref(), e.cwd, e.timestamp) else { continue };
        if !matches!(kind, "user" | "assistant") || e.sidechain {
            continue;
        }
        let Ok(at) = DateTime::parse_from_rfc3339(&at).map(|t| t.with_timezone(&Utc)) else { continue };
        if at < start || at >= end {
            continue;
        }
        let activity = out.entry(cwd).or_default();
        activity.times.push(at);
        if kind == "user" && !e.meta && e.tool_result.is_none() {
            if let Some(r) = e.message.and_then(|m| m.content).as_ref().and_then(request) {
                if !activity.requests.contains(&r) {
                    activity.requests.push(r);
                }
            }
        }
    }
}

/// The text the user typed, or None for tool results, commands, notices and one-word replies.
fn request(content: &serde_json::Value) -> Option<String> {
    let text = match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    // Tags are slash commands and their output; the brackets are interruptions.
    if text.starts_with('<') || text.starts_with('[') || text.split(' ').count() < MIN_WORDS {
        return None;
    }
    Some(text.chars().take(REQUEST_CHARS).collect())
}

fn active_minutes(sorted: &[DateTime<Utc>]) -> i64 {
    let seconds: i64 = sorted
        .windows(2)
        .map(|w| (w[1] - w[0]).num_seconds())
        .filter(|&s| s <= IDLE_MINUTES * 60)
        .sum();
    (seconds + 59) / 60
}

fn git(dir: &str, args: &[&str]) -> Option<String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let out = command.output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The repository's top folder, so that work in its subfolders counts as one.
fn repo_root(dir: &str) -> Option<String> {
    let root = git(dir, &["rev-parse", "--show-toplevel"]).filter(|r| !r.is_empty())?;
    Some(if cfg!(windows) { root.replace('/', "\\") } else { root })
}

/// Subjects of the commits made on `day` by the user set in that repository, on any branch.
fn commits(dir: &str, day: NaiveDate) -> Vec<String> {
    let since = format!("--since={} 00:00:00", day.format("%Y-%m-%d"));
    let until = format!("--until={} 00:00:00", day.succ_opt().unwrap_or(day).format("%Y-%m-%d"));
    let mut args = vec!["log", "--all", "--no-merges", "--format=%s", since.as_str(), until.as_str()];
    let author = git(dir, &["config", "user.email"]).filter(|a| !a.is_empty()).map(|a| format!("--author={a}"));
    if let Some(a) = &author {
        args.push(a);
    }
    let mut subjects: Vec<String> = vec![];
    for s in git(dir, &args).unwrap_or_default().lines().rev() {
        let s = s.trim().to_owned();
        if !s.is_empty() && !subjects.contains(&s) {
            subjects.push(s);
        }
    }
    subjects
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn reads_only_the_users_own_requests_in_the_day() {
        let lines = [
            r#"{"type":"user","cwd":"C:\\p","timestamp":"2026-11-27T09:00:00Z","message":{"content":"add a crash guard please"}}"#,
            r#"{"type":"assistant","cwd":"C:\\p","timestamp":"2026-11-27T09:04:00Z","message":{"content":[{"type":"text","text":"ok"}]}}"#,
            r#"{"type":"user","cwd":"C:\\p","timestamp":"2026-11-27T09:05:00Z","toolUseResult":{},"message":{"content":[{"type":"tool_result"}]}}"#,
            r#"{"type":"user","cwd":"C:\\p","timestamp":"2026-11-27T09:06:00Z","message":{"content":"continue"}}"#,
            r#"{"type":"user","cwd":"C:\\p","timestamp":"2026-11-27T09:07:00Z","message":{"content":"<command-name>/clear</command-name> x y"}}"#,
            r#"{"type":"user","cwd":"C:\\p","timestamp":"2026-11-27T09:08:00Z","isSidechain":true,"message":{"content":"subagent task text here"}}"#,
            r#"{"type":"user","cwd":"C:\\p","timestamp":"2026-11-27T09:09:00Z","message":{"content":[{"type":"image"},{"type":"text","text":"look at   this\nscreenshot"}]}}"#,
            r#"{"type":"user","cwd":"C:\\p","timestamp":"2026-11-28T09:00:00Z","message":{"content":"next day request text"}}"#,
            r#"{"type":"user","cwd":"C:\\p","timestamp":"2026-11-27T09:10:00Z","message":{"content":"add a crash guard please"}}"#,
            r#"not json"#,
            r#"{"type":"ai-title","aiTitle":"x"}"#,
        ];
        let mut out = HashMap::new();
        let (start, end) = (at("2026-11-27T00:00:00Z"), at("2026-11-28T00:00:00Z"));
        read_session(lines.join("\n").as_bytes(), start, end, &mut out);
        let a = &out["C:\\p"];
        assert_eq!(a.requests, ["add a crash guard please", "look at this screenshot"]);
        assert_eq!(a.times.len(), 7);
    }

    #[test]
    fn long_pauses_do_not_count_as_work() {
        let times = ["2026-11-27T09:00:00Z", "2026-11-27T09:09:00Z", "2026-11-27T11:00:00Z", "2026-11-27T11:00:30Z"];
        let times: Vec<_> = times.iter().map(|t| at(t)).collect();
        assert_eq!(active_minutes(&times), 10);
        assert_eq!(active_minutes(&[]), 0);
    }

    #[test]
    fn folders_match_without_case_or_slash_differences() {
        assert_eq!(key("c:/users/k/proj/"), key(r"C:\Users\k\Proj"));
        assert_ne!(key(r"C:\Users\k\Project2"), key(r"C:\Users\k\Proj"));
    }

    /// Prints today's real work on this machine: `cargo test work::tests::real -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real() {
        for f in day(Local::now().date_naive(), &[]) {
            println!("{} ~{} min, {} requests, commits {:?}", f.path, f.active_minutes, f.requests.len(), f.commits);
        }
    }
}
