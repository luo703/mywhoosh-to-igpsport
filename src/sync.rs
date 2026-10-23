use crate::{
    cli::Args,
    db::{Db, Job},
    fit, net,
    source::Source,
    storage,
    target::Target,
};
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use std::{path::Path, time::Instant};

#[derive(Default)]
pub struct Counts {
    downloaded: u32,
    submitted: u32,
    confirmed: u32,
    skipped: u32,
    errors: u32,
    uncertain: u32,
    planned: u32,
    pending: u32,
}
impl Counts {
    pub fn failed(&self) -> bool {
        self.errors > 0 || self.uncertain > 0
    }
}

pub fn parse_time(value: &str, tz: chrono_tz::Tz) -> Result<DateTime<Utc>> {
    if let Ok(date) = DateTime::parse_from_rfc3339(value) {
        return Ok(date.with_timezone(&Utc));
    }
    let date = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S")
        .context("缺少精确开始时间，无法远端去重")?;
    tz.from_local_datetime(&date)
        .single()
        .map(|d| d.with_timezone(&Utc))
        .context("目标开始时间处于时区歧义区间")
}
#[derive(Clone, Debug)]
struct ActivityKey {
    date: NaiveDate,
    seconds: f64,
    distance: f64,
    ascent: f64,
}
impl ActivityKey {
    fn from_fit(summary: &fit::Summary, tz: chrono_tz::Tz) -> Self {
        Self {
            date: summary.start.with_timezone(&tz).date_naive(),
            seconds: summary.seconds,
            distance: summary.distance,
            ascent: summary.ascent,
        }
    }
    fn from_row(row: &serde_json::Value, tz: chrono_tz::Tz) -> Result<Self> {
        let start = net::text(row, "startTime")?;
        let date = NaiveDate::parse_from_str(&start, "%Y.%m.%d")
            .or_else(|_| NaiveDate::parse_from_str(&start, "%Y-%m-%d"))
            .ok()
            .or_else(|| {
                parse_time(&start, tz)
                    .ok()
                    .map(|time| time.with_timezone(&tz).date_naive())
            })
            .context("目标列表日期无效，无法去重")?;
        let number = |field: &str| -> Result<f64> {
            let value = row[field]
                .as_f64()
                .or_else(|| row[field].as_str().and_then(|v| v.parse().ok()))
                .filter(|v| v.is_finite() && *v >= 0.0)
                .with_context(|| format!("目标列表缺少有效 {field}，无法去重"))?;
            Ok(value)
        };
        Ok(Self {
            date,
            seconds: number("recordTime")?,
            distance: number("rideDistance")?,
            ascent: number("totalAscent")?,
        })
    }
    fn from_source_row(row: &serde_json::Value, tz: chrono_tz::Tz) -> Option<Self> {
        let number = |field: &str| {
            row.get(field)
                .and_then(|v| {
                    v.as_f64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
                .filter(|v: &f64| v.is_finite() && *v >= 0.0)
        };
        let seconds = parse_duration(row.get("rideDuration")?.as_str()?)?;
        let distance_km = number("distance")?;
        let start = row
            .get("startDatetime")
            .and_then(|v| v.as_str())
            .and_then(|s| parse_time(s, tz).ok())?;
        Some(Self {
            date: start.with_timezone(&tz).date_naive(),
            seconds,
            distance: distance_km * 1000.0,
            ascent: number("elevation")?,
        })
    }
    fn matches(&self, other: &Self) -> bool {
        self.date == other.date
            && (self.seconds - other.seconds).abs() <= 2.0
            && (self.distance - other.distance).abs() <= 10.0
            && (self.ascent - other.ascent).abs() <= 2.0
    }
}
fn parse_duration(value: &str) -> Option<f64> {
    let mut parts = value.split(':');
    let hours: u32 = parts.next()?.parse().ok()?;
    let minutes: u32 = parts.next()?.parse().ok()?;
    let seconds: f64 = parts.next()?.parse().ok()?;
    if parts.next().is_some()
        || minutes >= 60
        || !seconds.is_finite()
        || !(0.0..60.0).contains(&seconds)
    {
        return None;
    }
    Some(f64::from(hours) * 3600.0 + f64::from(minutes) * 60.0 + seconds)
}

fn activity_overview(row: &serde_json::Value, tz: chrono_tz::Tz) -> String {
    let start = row["startDatetime"]
        .as_str()
        .and_then(|s| parse_time(s, tz).ok())
        .map(|t| t.with_timezone(&tz).format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "时间未知".into());
    let distance = row["distance"]
        .as_f64()
        .or_else(|| row["distance"].as_str().and_then(|s| s.parse().ok()))
        .map(|v: f64| format!("{v:.2} km"))
        .unwrap_or_else(|| "距离未知".into());
    let title = row["title"].as_str().unwrap_or("未命名活动");
    format!("{start} | {distance} | {title}")
}

fn list_duplicate<'a>(entries: &'a [(ActivityKey, String)], key: &ActivityKey) -> Option<&'a str> {
    let mut matches = entries.iter().filter(|(other, _)| key.matches(other));
    let (_, id) = matches.next()?;
    matches.next().is_none().then_some(id.as_str())
}
fn duplicate<'a>(entries: &'a [(ActivityKey, String)], key: &ActivityKey) -> Option<&'a str> {
    entries
        .iter()
        .find(|(existing, _)| existing.matches(key))
        .map(|(_, id)| id.as_str())
}
fn remote_keys(
    target: &mut Target,
    args: &Args,
    begin: NaiveDate,
    end: NaiveDate,
) -> Result<Vec<(ActivityKey, String)>> {
    println!("正在读取远端列表，按日期、记录时长、距离、总爬升建立去重索引……");
    let rows = target.in_date_range(begin, end, args.max_pages)?;
    let mut times = Vec::new();
    for row in rows {
        let id = net::text(&row, "rideId")?;
        times.push((
            ActivityKey::from_row(&row, args.timezone)
                .with_context(|| format!("远端活动 {id} 去重字段异常"))?,
            id,
        ));
    }
    println!("远端去重索引就绪：{} 条。", times.len());
    Ok(times)
}
fn reconcile(target: &mut Target, job: &Job) -> Result<Option<(String, bool)>> {
    if let Some(id) = &job.ride_id {
        let detail = target.detail(id)?;
        if detail.get("rideId").is_some() {
            return Ok(Some((id.clone(), true)));
        }
    }
    let marker = job
        .filename
        .split('_')
        .take(2)
        .collect::<Vec<_>>()
        .join("_");
    let data = target.activities(1, 20, Some(&marker))?;
    let rows: Vec<_> = data["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["title"].as_str().is_some_and(|t| t.contains(&marker)))
        .collect();
    if rows.len() != 1 {
        return Ok(None);
    }
    let row = rows[0];
    if row["analysisStatus"] == 1 || row["analysisStatus"] == true {
        return Ok(Some((net::text(row, "rideId")?, true)));
    }
    if row["analysisStatus"] == 0 || row["analysisStatus"] == false {
        return Ok(Some((net::text(row, "rideId")?, false)));
    }
    Ok(None)
}
fn finish(
    target: &mut Target,
    db: &Db,
    job: &mut Job,
    id: String,
    valid: bool,
    args: &Args,
) -> Result<bool> {
    job.ride_id = Some(id.clone());
    if !valid {
        job.status = "rejected".into();
        db.save(job)?;
        println!("  导入失败：iGPSPORT 拒绝了该 FIT 文件（活动 {id}）。");
        return Ok(false);
    }
    job.status = "submitted".into();
    db.save(job)?;
    println!("  正在核验导入数据：开始时间、最大速度、NP、IF、TSS……");
    let detail = target.detail(&id)?;
    crate::db::verify(&detail, job)?;
    if detail["title"].as_str() != Some(&args.activity_title) {
        println!("  正在修改活动名称为“{}”……", args.activity_title);
        target.rename(&id, &args.activity_title)?;
        ensure!(
            target.detail(&id)?["title"].as_str() == Some(&args.activity_title),
            "活动名称修改尚未确认"
        );
    }
    if let Some(analysis) = &job.analysis {
        if detail["desc"].as_str() != Some(analysis.as_str()) {
            println!("  正在将 AI 分析写入活动说明……");
            target.describe(&id, analysis)?;
            ensure!(
                target.detail(&id)?["desc"].as_str() == Some(analysis.as_str()),
                "AI 分析说明写入尚未确认"
            );
        }
    }
    job.status = "confirmed".into();
    db.save(job)?;
    println!("  同步完成：数据和活动名称已确认（iGPSPORT 活动 {id}）。");
    Ok(true)
}

pub fn activities_for_cycle(
    args: &Args,
    source: &mut Source,
    db: &Db,
) -> Result<Vec<serde_json::Value>> {
    let rows = source.activities(args.max_pages, args.since)?;
    let source_key = storage::hash(&source.id);
    let rows = if args.resync {
        if !args.dry_run {
            db.queue_source_rows(&source_key, rows.clone(), false)?;
        }
        println!("--resync：已跳过本地活动列表缓存筛选。");
        rows
    } else {
        db.queue_source_rows(&source_key, rows, args.dry_run)?
    };
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|row| {
            args.activity_id
                .as_ref()
                .is_none_or(|id| net::text(row, "activityId").map_or(true, |value| value == *id))
        })
        .collect();
    if args.resync && !args.dry_run {
        for row in &rows {
            db.reopen_source_activity(&source_key, &net::text(row, "activityId")?)?;
        }
    }
    println!("本地活动列表缓存：{} 条待处理。", rows.len());
    Ok(rows)
}

pub fn once(
    args: &Args,
    data_dir: &Path,
    source: &mut Source,
    target: &mut Target,
    db: &Db,
    rows: Vec<serde_json::Value>,
) -> Result<Counts> {
    let began = Instant::now();
    println!(
        "开始本轮同步{}{}。",
        if args.dry_run {
            "（预演，不上传）"
        } else {
            ""
        },
        if args.resync {
            "，忽略本地记录，按远端四项数据去重"
        } else {
            "，检查本地记录和远端四项数据"
        }
    );
    let scope = storage::hash(format!("{}:{}", source.id, target.identity));
    let source_key = storage::hash(&source.id);
    let total = rows.len();
    println!("读取完成：本轮符合筛选条件的活动共 {total} 条。");
    let source_dates: Vec<_> = rows
        .iter()
        .filter_map(|row| {
            row.get("startDatetime")
                .and_then(|v| v.as_str())
                .and_then(|s| parse_time(s, args.timezone).ok())
                .map(|t| t.with_timezone(&args.timezone).date_naive())
        })
        .collect();
    let mut queried_range = None;
    let mut times = if let (Some(begin), Some(latest)) = (
        source_dates.iter().min().copied(),
        source_dates.iter().max().copied(),
    ) {
        let end = Utc::now()
            .with_timezone(&args.timezone)
            .date_naive()
            .max(latest);
        queried_range = Some((begin, end));
        remote_keys(target, args, begin, end)?
    } else {
        println!("MyWhoosh 活动列表没有有效日期，暂不读取 iGPSPORT 日期范围。");
        Vec::new()
    };
    let mut counts = Counts::default();
    let mut candidates = Vec::new();
    for (index, row) in rows.into_iter().enumerate() {
        let activity_id = net::text(&row, "activityId")?;
        println!(
            "[活动 {}/{}] {}",
            index + 1,
            total,
            activity_overview(&row, args.timezone)
        );
        let old = db.get(&scope, &activity_id)?;
        let marker = format!(
            "MW_{}",
            &storage::hash(format!("{scope}:{activity_id}"))[..12]
        );
        let mut job = old.clone().unwrap_or(Job {
            scope: scope.clone(),
            activity_id: activity_id.clone(),
            filename: format!("{marker}_MyWhoosh.fit"),
            ..Job::default()
        });
        if !args.resync {
            if matches!(job.status.as_str(), "confirmed" | "duplicate" | "rejected") {
                println!(
                    "  跳过：本地状态为{}。",
                    crate::db::status_label(&job.status)
                );
                counts.skipped += 1;
                if !args.dry_run {
                    db.complete_source_activity(&source_key, &activity_id)?;
                }
                continue;
            }
            if matches!(
                job.status.as_str(),
                "submitted" | "submitting" | "uncertain"
            ) {
                if args.dry_run {
                    println!("  跳过：上次提交尚待核对，预演不会重传或修改状态。");
                    counts.skipped += 1;
                    continue;
                }
                println!("  正在核对上次提交的导入结果……");
                match reconcile(target, &job) {
                    Ok(Some((id, valid))) => match finish(target, db, &mut job, id, valid, args) {
                        Ok(true) => counts.confirmed += 1,
                        Ok(false) => counts.errors += 1,
                        Err(e) => {
                            counts.errors += 1;
                            eprintln!("核对失败 {activity_id}：{e}");
                        }
                    },
                    Ok(None) => {
                        counts.uncertain += 1;
                        println!("待核对，不重传：{activity_id}");
                    }
                    Err(e) => {
                        counts.errors += 1;
                        eprintln!("核对失败 {activity_id}：{e}");
                    }
                }
                if matches!(job.status.as_str(), "confirmed" | "rejected") {
                    db.complete_source_activity(&source_key, &activity_id)?;
                }
                counts.skipped += 1;
                continue;
            }
        }
        if let Some(list_key) = ActivityKey::from_source_row(&row, args.timezone) {
            if list_duplicate(&times, &list_key).is_some() {
                println!("  iGPSPORT中已存在相同活动,跳过.");
                counts.skipped += 1;
                if !args.dry_run {
                    db.complete_source_activity(&source_key, &activity_id)?;
                }
                continue;
            }
        }
        let prepared = (|| -> Result<_> {
            let cache = data_dir
                .join("fit")
                .join(&scope)
                .join(format!("{}.fit", storage::hash(&activity_id)));
            if !cache.exists() {
                println!("  正在下载原始 FIT 文件……");
                let raw = source.download(&activity_id)?;
                storage::write_private(&cache, &raw)?;
                counts.downloaded += 1;
                println!("  下载完成：{} 字节。", raw.len());
            } else {
                println!("  使用本地原始 FIT 缓存。");
            }
            println!("  正在校验 FIT 并读取运动开始时间……");
            let raw = std::fs::read(&cache)?;
            fit::validate(&raw)?;
            let summary = fit::summary(&raw)?;
            let start = summary.start;
            let key = ActivityKey::from_fit(&summary, args.timezone);
            println!(
                "  远端去重数据：日期 {}，记录时长 {:.1} 秒，距离 {:.2} 米，总爬升 {:.1} 米。",
                key.date, key.seconds, key.distance, key.ascent
            );
            println!(
                "  运动开始时间：{}（{}）。",
                start
                    .with_timezone(&args.timezone)
                    .format("%Y-%m-%d %H:%M:%S"),
                args.timezone
            );
            Ok((cache, key))
        })();
        match prepared {
            Ok((cache, key)) => candidates.push((
                job,
                cache,
                key,
                row["title"].as_str().unwrap_or_default().to_owned(),
            )),
            Err(error) => {
                counts.errors += 1;
                eprintln!("读取 FIT 失败 {activity_id}：{error}");
            }
        }
    }
    // A FIT can disagree with the list date. Expand the index before any upload.
    if let (Some(first), Some(last)) = (
        candidates.iter().map(|(_, _, key, _)| key.date).min(),
        candidates.iter().map(|(_, _, key, _)| key.date).max(),
    ) {
        let begin = queried_range.map_or(first, |(begin, _)| begin.min(first));
        let end = queried_range.map_or_else(
            || {
                Utc::now()
                    .with_timezone(&args.timezone)
                    .date_naive()
                    .max(last)
            },
            |(_, end)| end.max(last),
        );
        if queried_range != Some((begin, end)) {
            println!("FIT 日期超出列表查询范围，正在补全远端去重索引……");
            times = remote_keys(target, args, begin, end)?;
        }
    }
    let mut processed = 0;
    let candidate_total = candidates.len();
    for (index, (mut job, cache, key, source_title)) in candidates.into_iter().enumerate() {
        if args.limit.is_some_and(|n| processed >= n) {
            println!("已达到本轮处理数量上限，结束扫描。");
            break;
        }
        let activity_id = job.activity_id.clone();
        println!(
            "[去重及同步 {}/{}] {activity_id}",
            index + 1,
            candidate_total
        );
        let attempt = (|| -> Result<()> {
            if let Some(id) = duplicate(&times, &key) {
                println!("  远端四项数据匹配，跳过 {activity_id}（目标 {id}）。");
                counts.skipped += 1;
                if !args.dry_run {
                    db.complete_source_activity(&source_key, &activity_id)?;
                }
                return Ok(());
            }
            println!("  远端未发现四项数据匹配的重复记录。");
            processed += 1;
            if args.dry_run {
                if let Some(api_key) = &args.deepseek_key {
                    let raw = std::fs::read(&cache)?;
                    let (_, report) = fit::prepare(&raw, args.timezone, source.ftp)?;
                    let data = fit::analysis_data(&raw, &report, &source_title)?;
                    let analysis = crate::ai::analyze(api_key, &data, args.timeout)?;
                    println!("  运动说明预览：{analysis}");
                }
                println!("  预演通过：计划上传该活动，本次未执行上传。");
                counts.planned += 1;
                times.push((key, format!("本轮预演:{activity_id}")));
                return Ok(());
            }
            let raw = std::fs::read(&cache)?;
            let hash = storage::hash(&raw);
            if !args.resync && db.duplicate(&scope, &activity_id, &hash)? {
                println!("  跳过：原始 FIT 内容与已有同步记录重复。");
                job.status = "duplicate".into();
                job.sha256 = Some(hash);
                db.save(&job)?;
                db.complete_source_activity(&source_key, &activity_id)?;
                counts.skipped += 1;
                return Ok(());
            }
            println!(
                "  正在修正 FIT 时间、最大速度并计算功率指标（FTP={} W）……",
                source.ftp
            );
            let (prepared, report) = fit::prepare(&raw, args.timezone, source.ftp)?;
            println!(
                "  FIT 准备完成：NP={} W，IF={:.3}，TSS={:.1}，最大速度={:.2} km/h。",
                report.normalized_power,
                report.intensity_factor,
                report.training_stress_score,
                report.max_speed_mps * 3.6
            );
            let metrics = serde_json::to_string(&report)?;
            if let Some(api_key) = &args.deepseek_key {
                if job.analysis.is_none()
                    || job.analysis_version != crate::ai::VERSION
                    || job.sha256.as_ref() != Some(&hash)
                    || job.power_metrics.as_ref() != Some(&metrics)
                {
                    let data = fit::analysis_data(&raw, &report, &source_title)?;
                    job.analysis = Some(crate::ai::analyze(api_key, &data, args.timeout)?);
                    job.analysis_version = crate::ai::VERSION;
                } else {
                    println!("  复用已保存的 AI 运动分析。");
                }
            }
            let reuse = !args.resync
                && job.status == "uploaded"
                && job.compat_version == fit::VERSION
                && job.power_metrics.as_ref() == Some(&metrics);
            job.sha256 = Some(hash);
            job.compat_version = fit::VERSION;
            job.expected_start = Some(report.expected_start.clone());
            job.max_speed = Some(report.max_speed_mps);
            job.power_metrics = Some(metrics);
            if !reuse {
                job.oss_id = None;
                job.ride_id = None;
            }
            job.status = "downloaded".into();
            db.save(&job)?;
            storage::write_private(&cache.with_extension("igpsport.fit"), &prepared)?;
            if job.oss_id.is_none() {
                println!("  正在上传 FIT 文件（{} 字节）……", prepared.len());
                job.oss_id = Some(target.put_fit(&prepared)?);
                println!("  FIT 文件上传完成。");
            } else {
                println!("  复用已上传的 FIT 文件。");
            }
            job.status = "uploaded".into();
            db.save(&job)?;
            job.status = "submitting".into();
            db.save(&job)?;
            // Any uncertain non-idempotent result is journaled. Never automatically POST twice.
            println!("  正在提交 iGPSPORT 导入任务……");
            if let Err(error) = target.submit(job.oss_id.as_ref().unwrap(), &job.filename) {
                job.status = "uncertain".into();
                db.save(&job)?;
                counts.uncertain += 1;
                println!("  提交结果不确定，已保存待核对状态，下次将先核对导入结果。");
                times.push((key, format!("本轮待核对:{activity_id}")));
                return Err(error);
            }
            job.status = "submitted".into();
            db.save(&job)?;
            counts.submitted += 1;
            times.push((key, format!("本轮提交:{activity_id}")));
            println!("  导入任务已提交，正在查询处理结果……");
            if let Some((id, valid)) = reconcile(target, &job)? {
                if finish(target, db, &mut job, id, valid, args)? {
                    counts.confirmed += 1;
                } else {
                    counts.errors += 1;
                }
            } else {
                counts.pending += 1;
                println!("  导入结果尚未确认，已保存进度，下次同步将继续核对。");
            }
            if matches!(job.status.as_str(), "confirmed" | "rejected") {
                db.complete_source_activity(&source_key, &activity_id)?;
            }
            Ok(())
        })();
        if let Err(error) = attempt {
            counts.errors += 1;
            if !args.dry_run
                && !matches!(
                    job.status.as_str(),
                    "submitting" | "uncertain" | "submitted" | "uploaded"
                )
            {
                job.status = "failed".into();
                db.save(&job)?;
            }
            eprintln!("同步失败 {activity_id}：{error}");
        }
    }
    println!("本轮结束，用时 {:.1} 秒：下载 {} 条，提交 {} 条，确认完成 {} 条，跳过 {} 条，错误 {} 条，结果不确定 {} 条。", began.elapsed().as_secs_f64(), counts.downloaded, counts.submitted, counts.confirmed, counts.skipped, counts.errors, counts.uncertain);
    if counts.pending > 0 {
        println!("其中 {} 条已提交但尚待确认导入结果。", counts.pending);
    }
    if args.dry_run {
        println!("预演结果：计划上传 {} 条，实际上传 0 条。", counts.planned);
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn four_fields_tolerances_and_timezone() {
        let start = parse_time("2026-10-18 20:12:05", chrono_tz::Asia::Shanghai).unwrap();
        assert_eq!(
            start,
            parse_time("2026-10-18T12:12:05Z", chrono_tz::UTC).unwrap()
        );
        let key = ActivityKey::from_fit(
            &fit::Summary {
                start,
                seconds: 3658.0,
                distance: 26980.05,
                ascent: 33.0,
            },
            chrono_tz::Asia::Shanghai,
        );
        let entries = vec![(key.clone(), "id".into())];
        let mut boundary = key.clone();
        boundary.seconds += 2.0;
        boundary.distance += 10.0;
        boundary.ascent += 2.0;
        assert_eq!(duplicate(&entries, &boundary), Some("id"));
        for field in 0..4 {
            let mut changed = key.clone();
            match field {
                0 => changed.date = changed.date.succ_opt().unwrap(),
                1 => changed.seconds += 2.01,
                2 => changed.distance += 10.01,
                _ => changed.ascent += 2.01,
            }
            assert!(duplicate(&entries, &changed).is_none());
        }
        for date in ["2026.10.18", "2026-10-18", "2026-10-18T12:12:05Z"] {
            let row = serde_json::json!({"startTime":date,"recordTime":"3658","rideDistance":26980.05,"totalAscent":33});
            assert!(ActivityKey::from_row(&row, chrono_tz::Asia::Shanghai)
                .unwrap()
                .matches(&key));
        }
        let row =
            serde_json::json!({"startTime":"2026.10.18","recordTime":3658,"rideDistance":26980.05});
        assert!(ActivityKey::from_row(&row, chrono_tz::UTC).is_err());
        let midnight = fit::Summary {
            start: parse_time("2026-10-17T20:00:00Z", chrono_tz::UTC).unwrap(),
            seconds: 3658.0,
            distance: 26980.05,
            ascent: 33.0,
        };
        assert!(ActivityKey::from_fit(&midnight, chrono_tz::Asia::Shanghai).matches(&key));
        assert!(parse_time("2026-10-18", chrono_tz::UTC).is_err());
    }

    #[test]
    fn source_list_key_uses_four_available_fields() {
        let row = serde_json::json!({
            "startDatetime":"2026-10-18T12:12:05Z",
            "rideDuration":"01:00:58.000",
            "distance":26.98,
            "elevation":33,
            "title":"Sample ride"
        });
        let key = ActivityKey::from_source_row(&row, chrono_tz::Asia::Shanghai).unwrap();
        assert_eq!(key.date, NaiveDate::from_ymd_opt(2026, 10, 18).unwrap());
        assert_eq!(key.seconds, 3658.0);
        assert_eq!(key.distance, 26980.0);
        let overview = activity_overview(&row, chrono_tz::Asia::Shanghai);
        assert!(overview.contains("2026-10-18 20:12:05"));
        assert!(overview.contains("26.98 km"));
        assert!(overview.contains("Sample ride"));
        assert!(!overview.contains("33"));
        assert!(!overview.contains("activity-id"));
        let mut remote = key.clone();
        remote.distance = 26980.05;
        let mut entries = vec![(remote.clone(), "existing".into())];
        assert_eq!(list_duplicate(&entries, &key), Some("existing"));
        assert_eq!(duplicate(&entries, &key), Some("existing"));
        entries.push((remote, "ambiguous".into()));
        assert!(list_duplicate(&entries, &key).is_none());
        for invalid in [
            "",
            "1h garbage",
            "01:60:00",
            "01:00:NaN",
            "01:00:-1",
            "01:00:00:00",
        ] {
            assert!(parse_duration(invalid).is_none());
        }
        assert!(ActivityKey::from_source_row(
            &serde_json::json!({"startDatetime":"2026-10-18T12:12:05Z","totalTime":3658}),
            chrono_tz::Asia::Shanghai
        )
        .is_none());
    }
}
