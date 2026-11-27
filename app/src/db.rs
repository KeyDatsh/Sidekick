//! Local storage: projects, tasks by day, report rows, settings. Days are `YYYY-MM-DD`, local time.

use std::path::Path;

use chrono::{Datelike, Days, NaiveDate, Weekday};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;

pub type Res<T> = rusqlite::Result<T>;

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS projects (
    id INTEGER PRIMARY KEY,
    client TEXT NOT NULL,
    name TEXT NOT NULL DEFAULT '',
    archived INTEGER NOT NULL DEFAULT 0,
    last_used INTEGER NOT NULL DEFAULT 0,
    UNIQUE (client, name)
);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    project_id INTEGER REFERENCES projects (id),
    day TEXT,                -- NULL: later, no day yet
    carried_from TEXT,       -- first day it was planned for, once carried over
    started_on TEXT,
    done_on TEXT
);
CREATE INDEX IF NOT EXISTS tasks_day ON tasks (day);
CREATE TABLE IF NOT EXISTS report_rows (
    day TEXT NOT NULL,
    project_id INTEGER NOT NULL,   -- 0: tasks without a project
    minutes INTEGER NOT NULL DEFAULT 0,
    memo TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (day, project_id)
);
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS meetings (
    id INTEGER PRIMARY KEY,
    started_at TEXT NOT NULL,      -- local time, YYYY-MM-DD HH:MM:SS
    ended_at TEXT                  -- NULL while it runs (or if the app crashed)
);
CREATE TABLE IF NOT EXISTS meeting_lines (
    id INTEGER PRIMARY KEY,
    meeting_id INTEGER NOT NULL REFERENCES meetings (id),
    at_ms INTEGER NOT NULL,        -- since the meeting started
    speaker TEXT NOT NULL,         -- 'them' (call audio) or 'me' (pause mode); no names
    original TEXT NOT NULL,
    translation TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS meeting_lines_meeting ON meeting_lines (meeting_id, at_ms);
CREATE TABLE IF NOT EXISTS folders (
    path TEXT PRIMARY KEY,         -- work in it and its subfolders goes to the project
    project_id INTEGER             -- NULL: not client work, left out of the report
);
";

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Project {
    pub id: i64,
    pub client: String,
    pub name: String,
}

impl Project {
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            self.client.clone()
        } else {
            format!("{} · {}", self.client, self.name)
        }
    }
}

#[derive(Serialize, Debug, Clone)]
pub struct Task {
    pub id: i64,
    pub title: String,
    pub body: String,
    pub project_id: Option<i64>,
    pub day: Option<String>,
    pub carried_from: Option<String>,
    pub started: bool,
    pub done: bool,
}

#[derive(Serialize, Debug)]
pub struct DayTab {
    /// None: the "later" tab.
    pub day: Option<String>,
    pub open: i64,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Line {
    pub at_ms: i64,
    pub speaker: String,
    pub original: String,
    pub translation: String,
}

#[derive(Serialize, Debug, Clone)]
pub struct ReportRow {
    pub project_id: i64,
    pub label: String,
    pub minutes: i64,
    pub memo: String,
    pub tasks: Vec<Task>,
    /// Claude Code requests and commits in the project's folders.
    pub work: Vec<crate::work::Folder>,
}

pub fn iso(day: NaiveDate) -> String {
    day.format("%Y-%m-%d").to_string()
}

/// Today and the next three working days.
pub fn day_strip(today: NaiveDate) -> Vec<NaiveDate> {
    let mut days = vec![today];
    let mut d = today;
    while days.len() < 4 {
        d = d + Days::new(1);
        if !matches!(d.weekday(), Weekday::Sat | Weekday::Sun) {
            days.push(d);
        }
    }
    days
}

/// First non-empty line becomes the title, the rest the text.
pub fn draft_from_text(text: &str) -> (String, String) {
    let text = text.trim().replace("\r\n", "\n");
    match text.split_once('\n') {
        Some((title, body)) => (title.trim().to_owned(), body.trim().to_owned()),
        None => (text, String::new()),
    }
}

pub struct Db(Connection);

impl Db {
    pub fn open(path: &Path) -> Res<Db> {
        Db::init(Connection::open(path)?)
    }

    fn init(c: Connection) -> Res<Db> {
        c.execute_batch(SCHEMA)?;
        Ok(Db(c))
    }

    // Projects

    /// Most recently used first.
    pub fn projects(&self) -> Res<Vec<Project>> {
        let mut q = self.0.prepare(
            "SELECT id, client, name FROM projects WHERE archived = 0 ORDER BY last_used DESC, client, name",
        )?;
        let rows = q.query_map([], project)?.collect();
        rows
    }

    /// Returns the existing project (un-archived) when it is already there.
    pub fn add_project(&self, client: &str, name: &str) -> Res<i64> {
        self.0.query_row(
            "INSERT INTO projects (client, name) VALUES (?1, ?2)
             ON CONFLICT (client, name) DO UPDATE SET archived = 0 RETURNING id",
            params![client.trim(), name.trim()],
            |r| r.get(0),
        )
    }

    pub fn archive_project(&self, id: i64) -> Res<()> {
        self.0.execute("UPDATE projects SET archived = 1 WHERE id = ?1", [id])?;
        Ok(())
    }

    fn touch_project(&self, id: Option<i64>) -> Res<()> {
        self.0.execute(
            "UPDATE projects SET last_used = (SELECT MAX(last_used) + 1 FROM projects) WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    // Tasks

    pub fn add_task(&self, title: &str, body: &str, project: Option<i64>, day: Option<&str>) -> Res<i64> {
        self.0.execute(
            "INSERT INTO tasks (title, body, project_id, day) VALUES (?1, ?2, ?3, ?4)",
            params![title.trim(), body.trim(), project, day],
        )?;
        let id = self.0.last_insert_rowid();
        self.touch_project(project)?;
        Ok(id)
    }

    pub fn update_task(&self, id: i64, title: &str, body: &str, project: Option<i64>) -> Res<()> {
        self.0.execute(
            "UPDATE tasks SET title = ?2, body = ?3, project_id = ?4 WHERE id = ?1",
            params![id, title.trim(), body.trim(), project],
        )?;
        Ok(())
    }

    /// Moving by hand is a new plan, so the "carried over" note goes away.
    pub fn move_task(&self, id: i64, day: Option<&str>) -> Res<()> {
        self.0.execute(
            "UPDATE tasks SET day = ?2, carried_from = NULL WHERE id = ?1",
            params![id, day],
        )?;
        Ok(())
    }

    /// Closing a task also files it under today: that is the day it counts for.
    pub fn set_done(&self, id: i64, done: bool, today: &str) -> Res<()> {
        self.0.execute(
            "UPDATE tasks SET done_on = CASE WHEN ?2 THEN ?3 END, day = CASE WHEN ?2 THEN ?3 ELSE day END
             WHERE id = ?1",
            params![id, done, today],
        )?;
        Ok(())
    }

    pub fn set_started(&self, id: i64, started: bool, today: &str) -> Res<()> {
        self.0.execute(
            "UPDATE tasks SET started_on = CASE WHEN ?2 THEN ?3 END WHERE id = ?1",
            params![id, started, today],
        )?;
        Ok(())
    }

    pub fn delete_task(&self, id: i64) -> Res<()> {
        self.0.execute("DELETE FROM tasks WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Unfinished tasks from past days move to today. Returns how many moved.
    pub fn carry_over(&self, today: &str) -> Res<usize> {
        self.0.execute(
            "UPDATE tasks SET carried_from = COALESCE(carried_from, day), day = ?1
             WHERE day < ?1 AND done_on IS NULL",
            [today],
        )
    }

    /// None: tasks for later.
    pub fn tasks(&self, day: Option<&str>) -> Res<Vec<Task>> {
        let mut q = self.0.prepare(
            "SELECT id, title, body, project_id, day, carried_from, started_on IS NOT NULL, done_on IS NOT NULL
             FROM tasks WHERE day IS ?1 ORDER BY done_on IS NOT NULL, id",
        )?;
        let rows = q.query_map([day], task)?.collect();
        rows
    }

    pub fn day_tabs(&self, today: NaiveDate) -> Res<Vec<DayTab>> {
        let mut days: Vec<Option<String>> = day_strip(today).into_iter().map(|d| Some(iso(d))).collect();
        days.push(None);
        days.into_iter()
            .map(|day| {
                let open = self.0.query_row(
                    "SELECT COUNT(*) FROM tasks WHERE day IS ?1 AND done_on IS NULL",
                    [&day],
                    |r| r.get(0),
                )?;
                Ok(DayTab { day, open })
            })
            .collect()
    }

    // Report

    /// One row per project with tasks closed or worked on that day.
    pub fn report(&self, day: &str, today: &str) -> Res<Vec<ReportRow>> {
        let mut q = self.0.prepare(
            "SELECT id, title, body, project_id, day, carried_from, started_on IS NOT NULL, done_on IS NOT NULL
             FROM tasks
             WHERE done_on = ?1 OR started_on = ?1
                OR (?1 = ?2 AND started_on IS NOT NULL AND done_on IS NULL)
             ORDER BY project_id, done_on IS NULL, id",
        )?;
        let tasks: Vec<Task> = q.query_map([day, today], task)?.collect::<Res<_>>()?;
        let mut rows: Vec<ReportRow> = vec![];
        for t in tasks {
            let project_id = t.project_id.unwrap_or(0);
            if rows.last().map(|r| r.project_id) != Some(project_id) {
                rows.push(self.report_row(day, project_id)?);
            }
            rows.last_mut().unwrap().tasks.push(t);
        }
        Ok(rows)
    }

    /// Folder -> project, None for folders left out of the report.
    pub fn folders(&self) -> Res<Vec<(String, Option<i64>)>> {
        let mut q = self.0.prepare("SELECT path, project_id FROM folders")?;
        let rows = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect();
        rows
    }

    pub fn set_folder(&self, path: &str, project_id: Option<i64>) -> Res<()> {
        self.0.execute(
            "INSERT INTO folders (path, project_id) VALUES (?1, ?2)
             ON CONFLICT (path) DO UPDATE SET project_id = excluded.project_id",
            params![path, project_id],
        )?;
        Ok(())
    }

    /// The report row of a project, without its tasks.
    pub fn report_row(&self, day: &str, project_id: i64) -> Res<ReportRow> {
        let label = self
            .0
            .query_row("SELECT id, client, name FROM projects WHERE id = ?1", [project_id], project)
            .optional()?
            .map_or("Без проєкту".to_owned(), |p| p.label());
        let (minutes, memo) = self
            .0
            .query_row(
                "SELECT minutes, memo FROM report_rows WHERE day = ?1 AND project_id = ?2",
                params![day, project_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or_default();
        Ok(ReportRow { project_id, label, minutes, memo, tasks: vec![], work: vec![] })
    }

    pub fn save_memo(&self, day: &str, project_id: i64, memo: &str) -> Res<()> {
        self.0.execute(
            "INSERT INTO report_rows (day, project_id, memo) VALUES (?1, ?2, ?3)
             ON CONFLICT (day, project_id) DO UPDATE SET memo = excluded.memo",
            params![day, project_id, memo],
        )?;
        Ok(())
    }

    pub fn save_minutes(&self, day: &str, project_id: i64, minutes: i64) -> Res<()> {
        self.0.execute(
            "INSERT INTO report_rows (day, project_id, minutes) VALUES (?1, ?2, ?3)
             ON CONFLICT (day, project_id) DO UPDATE SET minutes = excluded.minutes",
            params![day, project_id, minutes.clamp(0, 24 * 60)],
        )?;
        Ok(())
    }

    // Settings

    pub fn setting(&self, key: &str) -> Res<Option<String>> {
        self.0
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0))
            .optional()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Res<()> {
        self.0.execute(
            "INSERT INTO settings VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    // Meetings

    pub fn start_meeting(&self, now: &str) -> Res<i64> {
        self.0.execute("INSERT INTO meetings (started_at) VALUES (?1)", [now])?;
        Ok(self.0.last_insert_rowid())
    }

    /// With `id` None, closes every meeting left open, e.g. by a crash.
    pub fn end_meeting(&self, id: Option<i64>, now: &str) -> Res<()> {
        self.0.execute(
            "UPDATE meetings SET ended_at = ?2 WHERE ended_at IS NULL AND (?1 IS NULL OR id = ?1)",
            params![id, now],
        )?;
        Ok(())
    }

    pub fn add_line(&self, meeting: i64, line: &Line) -> Res<()> {
        self.0.execute(
            "INSERT INTO meeting_lines (meeting_id, at_ms, speaker, original, translation) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![meeting, line.at_ms, line.speaker, line.original, line.translation],
        )?;
        Ok(())
    }

    /// The last `limit` lines, oldest first.
    pub fn lines(&self, meeting: i64, limit: i64) -> Res<Vec<Line>> {
        let mut q = self.0.prepare(
            "SELECT at_ms, speaker, original, translation FROM
               (SELECT * FROM meeting_lines WHERE meeting_id = ?1 ORDER BY at_ms DESC, id DESC LIMIT ?2)
             ORDER BY at_ms, id",
        )?;
        let rows = q
            .query_map(params![meeting, limit], |r| {
                Ok(Line { at_ms: r.get(0)?, speaker: r.get(1)?, original: r.get(2)?, translation: r.get(3)? })
            })?
            .collect();
        rows
    }
}

fn project(r: &Row) -> Res<Project> {
    Ok(Project { id: r.get(0)?, client: r.get(1)?, name: r.get(2)? })
}

fn task(r: &Row) -> Res<Task> {
    Ok(Task {
        id: r.get(0)?,
        title: r.get(1)?,
        body: r.get(2)?,
        project_id: r.get(3)?,
        day: r.get(4)?,
        carried_from: r.get(5)?,
        started: r.get(6)?,
        done: r.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MON: &str = "2026-11-23";
    const TUE: &str = "2026-11-24";
    const WED: &str = "2026-11-25";

    fn db() -> Db {
        Db::init(Connection::open_in_memory().unwrap()).unwrap()
    }

    fn titles(tasks: &[Task]) -> Vec<&str> {
        tasks.iter().map(|t| t.title.as_str()).collect()
    }

    #[test]
    fn unfinished_tasks_carry_over_and_keep_the_first_day() {
        let db = db();
        db.add_task("open", "", None, Some(MON)).unwrap();
        let done = db.add_task("done", "", None, Some(MON)).unwrap();
        db.add_task("later", "", None, None).unwrap();
        db.add_task("future", "", None, Some(WED)).unwrap();
        db.set_done(done, true, MON).unwrap();

        assert_eq!(db.carry_over(TUE).unwrap(), 1);
        assert_eq!(db.carry_over(WED).unwrap(), 1);
        assert_eq!(db.carry_over(WED).unwrap(), 0);
        let wed = db.tasks(Some(WED)).unwrap();
        assert_eq!(titles(&wed), ["open", "future"]);
        assert_eq!(wed[0].carried_from.as_deref(), Some(MON));
        assert_eq!(titles(&db.tasks(Some(MON)).unwrap()), ["done"]);
        assert_eq!(titles(&db.tasks(None).unwrap()), ["later"]);
    }

    #[test]
    fn moving_by_hand_clears_the_carried_note() {
        let db = db();
        let id = db.add_task("t", "", None, Some(MON)).unwrap();
        db.carry_over(TUE).unwrap();
        db.move_task(id, Some(WED)).unwrap();
        assert_eq!(db.tasks(Some(WED)).unwrap()[0].carried_from, None);
        db.move_task(id, None).unwrap();
        assert_eq!(titles(&db.tasks(None).unwrap()), ["t"]);
    }

    #[test]
    fn a_task_closed_early_is_filed_under_the_day_it_was_closed() {
        let db = db();
        let id = db.add_task("t", "", None, Some(WED)).unwrap();
        db.set_done(id, true, MON).unwrap();
        assert_eq!(titles(&db.tasks(Some(MON)).unwrap()), ["t"]);
        db.set_done(id, false, MON).unwrap();
        assert!(!db.tasks(Some(MON)).unwrap()[0].done);
    }

    #[test]
    fn report_groups_closed_and_started_tasks_by_project() {
        let db = db();
        let a = db.add_project("A", "Mobile").unwrap();
        let b = db.add_project("B", "").unwrap();
        let done = db.add_task("done", "", Some(a), Some(TUE)).unwrap();
        let started = db.add_task("started", "", Some(a), Some(TUE)).unwrap();
        let long = db.add_task("running since monday", "", Some(b), Some(MON)).unwrap();
        db.add_task("untouched", "", Some(b), Some(TUE)).unwrap();
        let loose = db.add_task("no project", "", None, Some(TUE)).unwrap();
        db.set_done(done, true, TUE).unwrap();
        db.set_started(started, true, TUE).unwrap();
        db.set_started(long, true, MON).unwrap();
        db.set_done(loose, true, TUE).unwrap();
        db.save_minutes(TUE, a, 90).unwrap();
        db.save_memo(TUE, a, "memo").unwrap();

        let rows = db.report(TUE, TUE).unwrap();
        let got: Vec<_> = rows.iter().map(|r| (r.label.as_str(), titles(&r.tasks))).collect();
        assert_eq!(
            got,
            [
                ("Без проєкту", vec!["no project"]),
                ("A · Mobile", vec!["done", "started"]),
                ("B", vec!["running since monday"]),
            ]
        );
        assert_eq!((rows[1].minutes, rows[1].memo.as_str()), (90, "memo"));
        // Looking back, a task that is still running only counts for the day it was started.
        assert_eq!(db.report(TUE, WED).unwrap().len(), 2);
        assert_eq!(titles(&db.report(MON, WED).unwrap()[0].tasks), ["running since monday"]);
    }

    #[test]
    fn last_used_project_comes_first_and_archiving_hides_it() {
        let db = db();
        let a = db.add_project("A", "").unwrap();
        let b = db.add_project("B", "x").unwrap();
        assert_eq!(db.add_project(" A ", "").unwrap(), a);
        db.add_task("t", "", Some(b), None).unwrap();
        assert_eq!(db.projects().unwrap()[0].id, b);
        db.add_task("t", "", Some(a), None).unwrap();
        assert_eq!(db.projects().unwrap()[0].id, a);
        db.archive_project(a).unwrap();
        assert_eq!(db.projects().unwrap().len(), 1);
        assert_eq!(db.add_project("A", "").unwrap(), a);
        assert_eq!(db.projects().unwrap().len(), 2);
    }

    #[test]
    fn minutes_and_memo_are_saved_independently_and_clamped() {
        let db = db();
        let id = db.add_task("t", "", None, Some(TUE)).unwrap();
        db.set_done(id, true, TUE).unwrap();
        db.save_memo(TUE, 0, "m").unwrap();
        db.save_minutes(TUE, 0, -30).unwrap();
        let row = &db.report(TUE, TUE).unwrap()[0];
        assert_eq!((row.minutes, row.memo.as_str()), (0, "m"));
    }

    #[test]
    fn day_strip_skips_weekends_but_keeps_today() {
        let day = |s: &str| s.parse::<NaiveDate>().unwrap();
        let strip = |s: &str| day_strip(day(s)).into_iter().map(iso).collect::<Vec<_>>();
        assert_eq!(strip(WED), [WED, "2026-11-26", "2026-11-27", "2026-11-30"]);
        assert_eq!(strip("2026-11-28"), ["2026-11-28", "2026-11-30", "2026-12-01", "2026-12-02"]);
    }

    #[test]
    fn meeting_keeps_the_last_lines_in_order_and_closes() {
        let db = db();
        let id = db.start_meeting("2026-11-26 10:00:00").unwrap();
        for i in 0..5 {
            let line = Line { at_ms: i * 1000, speaker: "them".into(), original: format!("en {i}"), translation: format!("uk {i}") };
            db.add_line(id, &line).unwrap();
        }
        let last: Vec<String> = db.lines(id, 2).unwrap().into_iter().map(|l| l.original).collect();
        assert_eq!(last, ["en 3", "en 4"]);
        db.start_meeting("2026-11-26 11:00:00").unwrap();
        db.end_meeting(Some(id), "2026-11-26 10:30:00").unwrap();
        db.end_meeting(None, "2026-11-26 12:00:00").unwrap();
        let ends: Vec<String> =
            db.0.prepare("SELECT ended_at FROM meetings ORDER BY id").unwrap().query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        assert_eq!(ends, ["2026-11-26 10:30:00", "2026-11-26 12:00:00"]);
    }

    #[test]
    fn clipboard_text_splits_into_title_and_body() {
        assert_eq!(draft_from_text("\r\n Title \r\nline 1\r\nline 2\n"), ("Title".into(), "line 1\nline 2".into()));
        assert_eq!(draft_from_text("only"), ("only".into(), "".into()));
        assert_eq!(draft_from_text("  "), ("".into(), "".into()));
    }
}
