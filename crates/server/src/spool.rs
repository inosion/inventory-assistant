//! Durable on-disk job queue. One `<id>.json` (+ `<id>.jpg` while the photo is still needed) per capture.
//!
//! InvenTree is the source of truth; the spool is only a crash-safe queue and a recent-activity log.

use std::path::{Path, PathBuf};

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

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Job {
    pub id: String,
    pub created: DateTime<Utc>,
    pub location: i64,
    pub location_name: String,
    pub qty: f64,
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
}

impl Job {
    pub fn new(location: i64, location_name: String, qty: f64) -> Self {
        Job {
            id: uuid::Uuid::new_v4().simple().to_string(),
            created: Utc::now(),
            location,
            location_name,
            qty,
            status: Status::Queued,
            part: None,
            name: None,
            confidence: None,
            error: None,
            attempts: 0,
            markings: None,
            sources: vec![],
        }
    }

    /// Records a failed attempt.
    pub fn fail(&mut self, err: impl std::fmt::Display) {
        self.attempts += 1;
        self.error = Some(err.to_string());
    }
}

pub struct Spool {
    dir: PathBuf,
}

impl Spool {
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Spool { dir })
    }

    fn json_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    pub fn photo_path(&self, job: &Job) -> PathBuf {
        self.dir.join(format!("{}.jpg", job.id))
    }

    /// Atomically writes the job (write to temp file, then rename).
    pub fn save(&self, job: &Job) -> Result<()> {
        let tmp = self.dir.join(format!("{}.json.tmp", job.id));
        std::fs::write(&tmp, serde_json::to_vec_pretty(job)?)?;
        std::fs::rename(tmp, self.json_path(&job.id))?;
        Ok(())
    }

    pub fn put_photo(&self, job: &Job, bytes: &[u8]) -> Result<()> {
        Ok(std::fs::write(self.photo_path(job), bytes)?)
    }

    /// The photo is only kept until identification is final.
    pub fn drop_photo(&self, job: &Job) {
        let _ = std::fs::remove_file(self.photo_path(job));
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
        let mut a = Job::new(1, "A1".into(), 1.0);
        a.created = Utc::now() - chrono::Duration::seconds(10);
        let b = Job::new(2, "B1/cables".into(), 3.0);
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
    }
}
