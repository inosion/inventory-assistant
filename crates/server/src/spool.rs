//! Durable on-disk job queue. One `<id>.json` per capture, plus its photos (`<id>.jpg`, `<id>-2.jpg`, …) while
//! they are still needed and a small `<id>.thumb.jpg` for the activity log.
//!
//! InvenTree is the source of truth; the spool is only a crash-safe queue and a recent-activity log.

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Lifecycle of a capture: `queued → stored → (research →) named`, or `unnamed` if identification gives up.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Photo received, not yet in InvenTree.
    Queued,
    /// Part + stock exist in InvenTree's Inbox category; waiting for the vision model.
    Stored,
    /// Named once; markings are being web-searched for a second opinion.
    Research,
    /// Identified (final).
    Named,
    /// Identification failed repeatedly; the part stays in Inbox for a human (final).
    Unnamed,
}

/// How the capture was taken.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// The box was chosen before the photo ("Store" tab).
    #[default]
    Store,
    /// "Don't know" tab: parked in the Inbox location until a box is chosen from the AI's suggestions.
    Sort,
}

/// A box the AI suggests for a `Sort` capture.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub location: i64,
    pub name: String,
    pub reason: String,
}

/// Photos per capture: more add little for identification and slow the model down.
pub const MAX_PHOTOS: usize = 4;

/// How many finished entries each log keeps (Store captures, and filed Sort captures).
pub const KEEP: usize = 100;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Job {
    pub id: String,
    pub created: DateTime<Utc>,
    #[serde(default)]
    pub mode: Mode,
    /// The box. For `Sort` captures it is the Inbox location until the item is filed.
    pub location: Option<i64>,
    pub location_name: String,
    pub qty: f64,
    /// Photos of the item (all shown to the model together). Job files from before this existed had one.
    #[serde(default = "one")]
    pub photos: u32,
    /// How many of the extra photos (2nd onwards) are attached to the part, so a retry never repeats one.
    #[serde(default)]
    pub attached: u32,
    pub status: Status,
    pub part: Option<i64>,
    pub name: Option<String>,
    pub confidence: Option<String>,
    pub error: Option<String>,
    pub attempts: u32,
    #[serde(default)]
    pub markings: Option<String>,
    #[serde(default)]
    pub sources: Vec<String>,
    /// The InvenTree stock item created by the store step.
    #[serde(default)]
    pub stock: Option<i64>,
    /// `Sort` only: `None` until the suggest step has run (it may find nothing).
    #[serde(default)]
    pub suggestions: Option<Vec<Suggestion>>,
    /// `Sort` only: when the item was moved from the Inbox into its box.
    #[serde(default)]
    pub filed: Option<DateTime<Utc>>,
    /// Whether the duplicate check has run (job files from before it existed count as checked).
    #[serde(default = "yes")]
    pub dup_checked: bool,
    /// The capture was a part already in InvenTree: its stock and photo went to this part instead.
    #[serde(default)]
    pub merged_into: Option<i64>,
}

fn yes() -> bool {
    true
}

fn one() -> u32 {
    1
}

impl Job {
    pub fn new(mode: Mode, location: Option<i64>, location_name: String, qty: f64, photos: u32) -> Self {
        Job {
            id: uuid::Uuid::new_v4().simple().to_string(),
            created: Utc::now(),
            mode,
            location,
            location_name,
            qty,
            photos: photos.max(1),
            attached: 0,
            status: Status::Queued,
            part: None,
            name: None,
            confidence: None,
            error: None,
            attempts: 0,
            markings: None,
            sources: vec![],
            stock: None,
            suggestions: None,
            filed: None,
            dup_checked: false,
            merged_into: None,
        }
    }

    /// Identification has finished (successfully or not).
    pub fn is_final(&self) -> bool {
        matches!(self.status, Status::Named | Status::Unnamed)
    }

    /// A `Sort` capture still waiting in the Inbox for a box.
    pub fn in_inbox(&self) -> bool {
        self.mode == Mode::Sort && self.filed.is_none()
    }

    /// Records a failed attempt.
    pub fn fail(&mut self, err: impl std::fmt::Display) {
        self.attempts += 1;
        self.error = Some(err.to_string());
    }
}

pub struct Spool {
    dir: PathBuf,
    /// Serialises read-modify-write of job files between the worker and the API.
    lock: Mutex<()>,
}

/// Job ids are uuid "simple" strings; anything else must never reach a file path.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Spool {
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Spool { dir, lock: Mutex::new(()) })
    }

    fn json_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// The `n`th photo (0-based). The first keeps the original single-photo name.
    fn photo_file(&self, id: &str, n: u32) -> PathBuf {
        match n {
            0 => self.dir.join(format!("{id}.jpg")),
            _ => self.dir.join(format!("{id}-{}.jpg", n + 1)),
        }
    }

    /// All of the job's photos, in the order they were taken.
    pub fn photo_paths(&self, job: &Job) -> Vec<PathBuf> {
        (0..job.photos.max(1)).map(|n| self.photo_file(&job.id, n)).collect()
    }

    /// Small JPEG kept for the activity log after the photo itself is gone.
    pub fn thumb_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.thumb.jpg"))
    }

    pub fn load(&self, id: &str) -> Result<Job> {
        anyhow::ensure!(valid_id(id), "bad job id");
        read_job(&self.json_path(id))
    }

    /// Saves a job the worker has been holding. Filing is done by the user through the API while the
    /// worker may be busy with the same job, so a filed state already on disk wins over the worker's copy.
    pub fn save_from_worker(&self, job: &Job) -> Result<()> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut job = job.clone();
        if let Ok(disk) = read_job(&self.json_path(&job.id))
            && disk.filed.is_some()
        {
            job.filed = disk.filed;
            job.location = disk.location;
            job.location_name = disk.location_name;
        }
        self.write(&job)
    }

    /// Re-reads a job, applies `f` and saves it, all under the spool lock.
    pub fn update<T>(&self, id: &str, f: impl FnOnce(&mut Job) -> Result<T>) -> Result<(Job, T)> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut job = self.load(id)?;
        let out = f(&mut job)?;
        self.write(&job)?;
        Ok((job, out))
    }

    /// Writes a new job (the API's capture handler).
    pub fn save(&self, job: &Job) -> Result<()> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.write(job)
    }

    /// Atomically writes the job (write to temp file, then rename).
    fn write(&self, job: &Job) -> Result<()> {
        let tmp = self.dir.join(format!("{}.json.tmp", job.id));
        std::fs::write(&tmp, serde_json::to_vec_pretty(job)?)?;
        std::fs::rename(tmp, self.json_path(&job.id))?;
        Ok(())
    }

    pub fn put_photos(&self, job: &Job, photos: &[impl AsRef<[u8]>]) -> Result<()> {
        for (n, p) in photos.iter().enumerate() {
            std::fs::write(self.photo_file(&job.id, n as u32), p)?;
        }
        Ok(())
    }

    /// The photos are only kept until the duplicate check is done.
    pub fn drop_photos(&self, job: &Job) {
        for p in self.photo_paths(job) {
            let _ = std::fs::remove_file(p);
        }
    }

    /// Drops the oldest finished entries so each log keeps at most `keep`: Store captures that are
    /// final, and Sort captures that have been filed. Work in progress and the Inbox are never pruned.
    pub fn prune(&self, keep: usize) -> Result<usize> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let jobs = self.load_all()?;
        let mut doomed = Vec::new();
        let mut stored: Vec<&Job> = jobs.iter().filter(|j| j.mode == Mode::Store && j.is_final()).collect();
        stored.sort_by_key(|j| std::cmp::Reverse(j.created));
        doomed.extend(stored.into_iter().skip(keep));
        let mut filed: Vec<&Job> = jobs.iter().filter(|j| j.mode == Mode::Sort && j.filed.is_some() && j.is_final()).collect();
        filed.sort_by_key(|j| std::cmp::Reverse(j.filed));
        doomed.extend(filed.into_iter().skip(keep));
        for j in &doomed {
            self.drop_photos(j);
            let _ = std::fs::remove_file(self.thumb_path(&j.id));
            std::fs::remove_file(self.json_path(&j.id))?;
        }
        Ok(doomed.len())
    }

    /// All jobs, oldest first. Unreadable files are logged and skipped.
    pub fn load_all(&self) -> Result<Vec<Job>> {
        let mut jobs = Vec::new();
        for e in std::fs::read_dir(&self.dir)? {
            let p = e?.path();
            if p.extension().is_some_and(|x| x == "json") {
                match read_job(&p) {
                    Ok(j) => jobs.push(j),
                    Err(err) => tracing::warn!("bad job file {}: {err:#}", p.display()),
                }
            }
        }
        jobs.sort_by_key(|j| j.created);
        Ok(jobs)
    }
}

fn read_job(p: &Path) -> Result<Job> {
    Ok(serde_json::from_slice(&std::fs::read(p)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("ia-spool-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn save_and_reload_in_creation_order() {
        let dir = tmpdir();
        let spool = Spool::open(&dir).unwrap();
        let mut a = Job::new(Mode::Store, Some(1), "A1".into(), 1.0, 1);
        a.created = Utc::now() - chrono::Duration::seconds(10);
        let b = Job::new(Mode::Store, Some(2), "B1/cables".into(), 3.0, 1);
        spool.save(&b).unwrap();
        spool.save(&a).unwrap();
        std::fs::write(dir.join("garbage.json"), b"not json").unwrap();

        let jobs = spool.load_all().unwrap();
        assert_eq!(jobs.iter().map(|j| j.id.as_str()).collect::<Vec<_>>(), vec![a.id.as_str(), b.id.as_str()]);
        assert_eq!(jobs[1].location_name, "B1/cables");
        assert_eq!(jobs[0].status, Status::Queued);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn status_serialises_lowercase() {
        assert_eq!(serde_json::to_string(&Status::Research).unwrap(), "\"research\"");
    }

    #[test]
    fn old_job_files_without_newer_fields_still_load() {
        let j: Job = serde_json::from_str(
            r#"{"id":"x","created":"2026-11-08T00:00:00Z","location":1,"location_name":"A1","qty":1.0,
                "status":"named","part":5,"name":"HC-05","confidence":"high","error":null,"attempts":0}"#,
        )
        .unwrap();
        assert!(j.markings.is_none() && j.sources.is_empty());
        assert_eq!(j.mode, Mode::Store);
        assert_eq!(j.location, Some(1));
        assert!(j.suggestions.is_none() && j.filed.is_none() && j.stock.is_none());
        assert!(j.dup_checked && j.merged_into.is_none(), "old jobs must not be re-checked for duplicates");
        assert_eq!((j.photos, j.attached), (1, 0));
    }

    #[test]
    fn multi_photo_files_are_named_in_order_and_dropped_together() {
        let dir = tmpdir();
        let spool = Spool::open(&dir).unwrap();
        let j = Job::new(Mode::Store, Some(1), "A1".into(), 1.0, 3);
        spool.put_photos(&j, &[b"a".as_slice(), b"b", b"c"]).unwrap();
        let names: Vec<String> = spool.photo_paths(&j).iter().map(|p| p.file_name().unwrap().to_string_lossy().into()).collect();
        assert_eq!(names, vec![format!("{}.jpg", j.id), format!("{}-2.jpg", j.id), format!("{}-3.jpg", j.id)]);
        assert_eq!(std::fs::read(&spool.photo_paths(&j)[1]).unwrap(), b"b");
        spool.drop_photos(&j);
        assert!(spool.photo_paths(&j).iter().all(|p| !p.exists()));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_path_like_ids() {
        assert!(valid_id("0123abcdef"));
        assert!(!valid_id("../etc/passwd") && !valid_id("") && !valid_id("a.json"));
    }

    #[test]
    fn worker_save_keeps_a_filing_done_meanwhile() {
        let dir = tmpdir();
        let spool = Spool::open(&dir).unwrap();
        let mut j = Job::new(Mode::Sort, Some(9), "Inbox".into(), 1.0, 1);
        spool.save(&j).unwrap();
        spool
            .update(&j.id, |d| {
                d.filed = Some(Utc::now());
                d.location = Some(3);
                d.location_name = "B3".into();
                Ok(())
            })
            .unwrap();
        j.status = Status::Named; // the worker's stale copy
        spool.save_from_worker(&j).unwrap();
        let d = spool.load(&j.id).unwrap();
        assert_eq!((d.status, d.location, d.location_name.as_str()), (Status::Named, Some(3), "B3"));
        assert!(d.filed.is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prune_keeps_newest_finished_and_all_open_work() {
        let dir = tmpdir();
        let spool = Spool::open(&dir).unwrap();
        let t0 = Utc::now() - chrono::Duration::hours(1);
        let mk = |mode, i: i64, status, filed: bool| {
            let mut j = Job::new(mode, Some(1), "A1".into(), 1.0, 1);
            j.created = t0 + chrono::Duration::seconds(i);
            j.status = status;
            j.filed = filed.then(|| t0 + chrono::Duration::seconds(i));
            spool.save(&j).unwrap();
            std::fs::write(spool.thumb_path(&j.id), b"x").unwrap();
            j
        };
        let old = mk(Mode::Store, 0, Status::Named, false);
        for i in 1..=3 {
            mk(Mode::Store, i, Status::Named, false);
        }
        let busy = mk(Mode::Store, -5, Status::Stored, false);
        let inbox_old = mk(Mode::Sort, -9, Status::Named, false);
        let filed_old = mk(Mode::Sort, 10, Status::Named, true);
        mk(Mode::Sort, 11, Status::Named, true);
        mk(Mode::Sort, 12, Status::Named, true);
        mk(Mode::Sort, 13, Status::Named, true);

        assert_eq!(spool.prune(3).unwrap(), 2);
        let ids: Vec<String> = spool.load_all().unwrap().into_iter().map(|j| j.id).collect();
        assert!(!ids.contains(&old.id) && !ids.contains(&filed_old.id));
        assert!(ids.contains(&busy.id) && ids.contains(&inbox_old.id));
        assert!(!spool.thumb_path(&old.id).exists());
        assert_eq!(ids.len(), 3 + 1 + 1 + 3);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
