use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::path::Path;

#[derive(Default, Clone)]
pub struct Job {
    pub scope: String,
    pub activity_id: String,
    pub status: String,
    pub filename: String,
    pub sha256: Option<String>,
    pub oss_id: Option<String>,
    pub ride_id: Option<String>,
    pub expected_start: Option<String>,
    pub max_speed: Option<f64>,
    pub power_metrics: Option<String>,
    pub compat_version: i64,
}
pub struct Db {
    pub db: Connection,
}
pub fn status_label(status: &str) -> &str {
    match status {
        "confirmed" => "已确认完成",
        "duplicate" => "重复记录",
        "rejected" => "导入被拒绝",
        "submitted" => "已提交，待确认",
        "submitting" => "提交中，待核对",
        "uncertain" => "结果不确定，待核对",
        "uploaded" => "文件已上传，待提交",
        "downloaded" => "已下载",
        "failed" => "处理失败",
        _ => status,
    }
}
impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS jobs (
            scope TEXT NOT NULL, activity_id TEXT NOT NULL, status TEXT NOT NULL, filename TEXT NOT NULL,
            sha256 TEXT, oss_id TEXT, ride_id TEXT, error TEXT, updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY(scope,activity_id));")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS source_activity_cache (
            source_id TEXT NOT NULL, activity_id TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0,
            updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY(source_id,activity_id));",
        )?;
        let columns: Vec<String> = db
            .prepare("PRAGMA table_info(jobs)")?
            .query_map([], |r| r.get(1))?
            .collect::<rusqlite::Result<_>>()?;
        for (name, kind) in [
            ("compat_version", "INTEGER DEFAULT 0"),
            ("expected_start", "TEXT"),
            ("max_speed", "REAL"),
            ("power_metrics", "TEXT"),
        ] {
            if !columns.iter().any(|c| c == name) {
                db.execute_batch(&format!("ALTER TABLE jobs ADD COLUMN {name} {kind}"))?;
            }
        }
        Ok(Self { db })
    }
    pub fn get(&self, scope: &str, id: &str) -> Result<Option<Job>> {
        Ok(self.db.query_row("SELECT scope,activity_id,status,filename,sha256,oss_id,ride_id,expected_start,max_speed,power_metrics,compat_version FROM jobs WHERE scope=? AND activity_id=?",params![scope,id],|r|Ok(Job {
            scope:r.get(0)?,activity_id:r.get(1)?,status:r.get(2)?,filename:r.get(3)?,sha256:r.get(4)?,oss_id:r.get(5)?,ride_id:r.get(6)?,expected_start:r.get(7)?,max_speed:r.get(8)?,power_metrics:r.get(9)?,compat_version:r.get::<_,Option<i64>>(10)?.unwrap_or(0)
        })).optional()?)
    }
    pub fn save(&self, j: &Job) -> Result<()> {
        self.db.execute("INSERT INTO jobs(scope,activity_id,status,filename,sha256,oss_id,ride_id,expected_start,max_speed,power_metrics,compat_version)
            VALUES(?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(scope,activity_id) DO UPDATE SET status=excluded.status,filename=excluded.filename,
            sha256=excluded.sha256,oss_id=excluded.oss_id,ride_id=excluded.ride_id,expected_start=excluded.expected_start,
            max_speed=excluded.max_speed,power_metrics=excluded.power_metrics,compat_version=excluded.compat_version,error=NULL,updated_at=CURRENT_TIMESTAMP",
            params![j.scope,j.activity_id,j.status,j.filename,j.sha256,j.oss_id,j.ride_id,j.expected_start,j.max_speed,j.power_metrics,j.compat_version])?;
        Ok(())
    }
    pub fn duplicate(&self, scope: &str, id: &str, hash: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE scope=? AND activity_id!=? AND sha256=? AND status IN ('confirmed','submitted','submitting','uncertain','duplicate'))",params![scope,id,hash],|r|r.get(0))?)
    }
    pub fn queue_source_rows(
        &self,
        source_id: &str,
        rows: Vec<Value>,
        dry_run: bool,
    ) -> Result<Vec<Value>> {
        let mut pending = Vec::new();
        for row in rows {
            let id = row["activityId"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("MyWhoosh 活动缺少 activityId"))?;
            if !dry_run {
                self.db.execute(
                    "INSERT OR IGNORE INTO source_activity_cache(source_id,activity_id) VALUES(?,?)",
                    params![source_id, id],
                )?;
            }
            let done: Option<bool> = self
                .db
                .query_row(
                    "SELECT done FROM source_activity_cache WHERE source_id=? AND activity_id=?",
                    params![source_id, id],
                    |r| r.get(0),
                )
                .optional()?;
            if done != Some(true) {
                pending.push(row);
            }
        }
        Ok(pending)
    }
    pub fn complete_source_activity(&self, source_id: &str, activity_id: &str) -> Result<()> {
        self.db.execute(
            "INSERT INTO source_activity_cache(source_id,activity_id,done) VALUES(?,?,1)
             ON CONFLICT(source_id,activity_id) DO UPDATE SET done=1,updated_at=CURRENT_TIMESTAMP",
            params![source_id, activity_id],
        )?;
        Ok(())
    }
    pub fn reopen_source_activity(&self, source_id: &str, activity_id: &str) -> Result<()> {
        self.db.execute(
            "INSERT INTO source_activity_cache(source_id,activity_id,done) VALUES(?,?,0)
             ON CONFLICT(source_id,activity_id) DO UPDATE SET done=0,updated_at=CURRENT_TIMESTAMP",
            params![source_id, activity_id],
        )?;
        Ok(())
    }
    pub fn status(&self) -> Result<()> {
        println!("本地同步记录统计：");
        let mut stmt = self
            .db
            .prepare("SELECT status,COUNT(*) FROM jobs GROUP BY status")?;
        for item in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, u64>(1)?)))? {
            let (status, count) = item?;
            println!("  {}：{count} 条", status_label(&status));
        }
        println!("最近 20 条同步记录：");
        let mut stmt = self
            .db
            .prepare("SELECT activity_id,status FROM jobs ORDER BY updated_at DESC LIMIT 20")?;
        for item in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (id, status) = item?;
            println!("  {id}  {}", status_label(&status));
        }
        Ok(())
    }
}

pub fn verify(detail: &Value, job: &Job) -> Result<()> {
    use anyhow::ensure;
    if let Some(start) = &job.expected_start {
        ensure!(
            detail["startTime"].as_str() == Some(start),
            "iGPSPORT 开始时间与 FIT 不一致"
        );
    }
    if let Some(speed) = job.max_speed {
        ensure!(
            detail["maxSpeed"]
                .as_f64()
                .is_some_and(|v| (v - speed).abs() <= 0.002),
            "最大速度与 FIT 不一致"
        );
    }
    if let Some(metrics) = &job.power_metrics {
        let metrics: Value = serde_json::from_str(metrics)?;
        for (key, name, tolerance) in [
            ("pwrNP", "normalized_power", 0.51),
            ("pwrIF", "intensity_factor", 0.0051),
            ("pwrTSS", "training_stress_score", 0.051),
        ] {
            let expected = metrics[name]
                .as_f64()
                .ok_or_else(|| anyhow::anyhow!("本地功率指标缺失"))?;
            ensure!(
                detail[key]
                    .as_f64()
                    .is_some_and(|v| (v - expected).abs() <= tolerance),
                "iGPSPORT {key} 缺失或与 FIT 不一致"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_cache_keeps_pending_and_isolates_accounts() {
        let path = std::env::temp_dir().join(format!("mw-cache-{}.sqlite3", uuid::Uuid::new_v4()));
        let rows = vec![
            serde_json::json!({"activityId":"a"}),
            serde_json::json!({"activityId":"b"}),
        ];
        let db = Db::open(&path).unwrap();
        assert_eq!(
            db.queue_source_rows("one", rows.clone(), false)
                .unwrap()
                .len(),
            2
        );
        db.complete_source_activity("one", "a").unwrap();
        drop(db);
        let db = Db::open(&path).unwrap();
        assert_eq!(
            db.queue_source_rows("one", rows.clone(), false)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            db.queue_source_rows("two", rows.clone(), false)
                .unwrap()
                .len(),
            2
        );
        db.complete_source_activity("one", "b").unwrap();
        assert!(db
            .queue_source_rows("one", rows.clone(), false)
            .unwrap()
            .is_empty());
        db.reopen_source_activity("one", "a").unwrap();
        assert_eq!(
            db.queue_source_rows("one", rows.clone(), false)
                .unwrap()
                .len(),
            1
        );
        db.queue_source_rows("preview", rows, true).unwrap();
        let count: u32 = db
            .db
            .query_row(
                "SELECT COUNT(*) FROM source_activity_cache WHERE source_id='preview'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        drop(db);
        let _ = std::fs::remove_file(path);
    }
    #[test]
    fn round_trip_job_without_credentials() {
        let path = std::env::temp_dir().join(format!("mw-{}.sqlite3", uuid::Uuid::new_v4()));
        let db = Db::open(&path).unwrap();
        let j = Job {
            scope: "pair".into(),
            activity_id: "id".into(),
            status: "submitting".into(),
            filename: "a.fit".into(),
            ..Job::default()
        };
        db.save(&j).unwrap();
        assert_eq!(db.get("pair", "id").unwrap().unwrap().status, "submitting");
        drop(db);
        let _ = std::fs::remove_file(path);
    }
}
