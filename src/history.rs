use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use std::{collections::BTreeSet, path::Path};

struct Ride {
    start: DateTime<Utc>,
    distance: f64,
    speed: f64,
    power: Option<Value>,
}

pub struct History {
    rides: Vec<Ride>,
    timezone: chrono_tz::Tz,
}

impl History {
    pub fn load(
        db: &crate::db::Db,
        source: &str,
        directory: &Path,
        ftp: f64,
        timezone: chrono_tz::Tz,
    ) -> Result<Self> {
        println!("  正在整理历史活动与本地 FIT 功率纪录……");
        let mut rides = Vec::new();
        for row in db.activity_history(source)? {
            let Some(start) = row["startDatetime"]
                .as_str()
                .and_then(|s| crate::sync::parse_time(s, timezone).ok())
            else {
                continue;
            };
            let Some(distance) = row["distance"]
                .as_f64()
                .filter(|d| d.is_finite() && *d > 0.0)
            else {
                continue;
            };
            let seconds = row["rideDuration"].as_str().and_then(duration);
            let Some(seconds) = seconds.filter(|s| s.is_finite() && *s > 0.0) else {
                continue;
            };
            let power = row["activityId"].as_str().and_then(|id| {
                let path = directory.join(format!("{}.fit", crate::storage::hash(id)));
                let raw = std::fs::read(path).ok()?;
                let (_, report) = crate::fit::prepare(&raw, timezone, ftp).ok()?;
                let data = crate::fit::analysis_data(&raw, &report, "").ok()?;
                Some(data["best_mean_power_w"].clone())
            });
            rides.push(Ride {
                start,
                distance,
                speed: distance / seconds * 3600.0,
                power,
            });
        }
        println!(
            "  历史参考：{} 条活动，{} 条具有可用的本地逐秒功率。",
            rides.len(),
            rides.iter().filter(|r| r.power.is_some()).count()
        );
        Ok(Self { rides, timezone })
    }

    pub fn context(&self, current: &Value, start: DateTime<Utc>) -> Value {
        // Strictly earlier activities only: historical uploads must not see future results.
        let previous: Vec<_> = self.rides.iter().filter(|r| r.start < start).collect();
        let recent: Vec<_> = previous
            .iter()
            .filter(|r| r.start >= start - Duration::days(30))
            .collect();
        let weeks: Vec<_> = previous
            .iter()
            .filter(|r| r.start >= start - Duration::days(21))
            .collect();
        let mean = |values: Vec<f64>| {
            (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
        };
        let seconds = current["duration_seconds"].as_f64().unwrap_or(0.0);
        let speed = current["distance_km"]
            .as_f64()
            .filter(|_| seconds > 0.0)
            .map(|d| d / seconds * 3600.0);
        let dates: BTreeSet<_> = previous
            .iter()
            .map(|r| r.start.with_timezone(&self.timezone).date_naive())
            .collect();
        let mut day = start.with_timezone(&self.timezone).date_naive();
        let mut streak = 1;
        while let Some(before) = day.pred_opt() {
            if !dates.contains(&before) {
                break;
            }
            streak += 1;
            day = before;
        }
        let mut ranks = serde_json::Map::new();
        if let Some(best) = current["best_mean_power_w"].as_object() {
            for (window, value) in best {
                if let Some(watts) = value.as_f64() {
                    let historical: Vec<_> = previous
                        .iter()
                        .filter_map(|r| r.power.as_ref()?.get(window)?.as_f64())
                        .collect();
                    ranks.insert(window.clone(), json!({
                        "current_w":watts, "historical_comparable_rides":historical.len(),
                        "rank_including_current":1 + historical.iter().filter(|p| **p > watts).count(),
                        "strict_new_best":!historical.is_empty() && historical.iter().all(|p| watts > *p),
                        "previous_best_w":historical.iter().copied().reduce(f64::max)
                    }));
                }
            }
        }
        json!({
            "scope":"仅本工具已读取的 MyWhoosh 活动；功率仅比较本地已有且通过连续逐秒采样校验的原始FIT，不代表其他平台或完整生涯。缺失、筛选或尚未读取的历史可能使连续天数和近期统计不完整。",
            "prior_rides":previous.len(),
            "prior_30_days":{"rides":recent.len(),"mean_distance_km":mean(recent.iter().map(|r| r.distance).collect()),"mean_speed_kmh":mean(recent.iter().map(|r| r.speed).collect())},
            "prior_21_days":{"rides":weeks.len(),"best_speed_kmh":weeks.iter().map(|r| r.speed).reduce(f64::max),
                "current_strict_fastest":!weeks.is_empty() && speed.is_some_and(|s| weeks.iter().all(|r| s > r.speed))},
            "current_speed_kmh":speed,
            "speed_note":"距离/记录总时长计算，含暂停；虚拟路线、坡度和跟骑影响速度，速度提高不能单独证明体能进步。",
            "observed_consecutive_training_days_including_current":streak,
            "power_rankings_in_cached_history":ranks,
            "relative_effort":null,
            "limitations":"没有Strava相对负荷度算法或阈值心率，不能编造相对负荷度、正常负荷范围或心率区间；TSS不是Strava相对负荷度。"
        })
    }
}

fn duration(value: &str) -> Option<f64> {
    let parts: Vec<_> = value
        .split(':')
        .map(str::parse::<f64>)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    if parts.len() != 3 || parts.iter().any(|p| *p < 0.0) || parts[1] >= 60.0 || parts[2] >= 60.0 {
        return None;
    }
    Some(parts[0] * 3600.0 + parts[1] * 60.0 + parts[2])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn historical_context_excludes_current_and_future_and_handles_ties() {
        let start: DateTime<Utc> = "2026-10-20T12:00:00Z".parse().unwrap();
        let rides = [-2, -1, 0, 1]
            .into_iter()
            .map(|offset| Ride {
                start: start + Duration::days(offset),
                distance: 20.0,
                speed: 25.0,
                power: Some(json!({"1800s":200.0})),
            })
            .collect();
        let history = History {
            rides,
            timezone: chrono_tz::Asia::Shanghai,
        };
        let data = json!({"duration_seconds":3600.0,"distance_km":30.0,"best_mean_power_w":{"1800s":200.0,"480s":250.0}});
        let context = history.context(&data, start);
        assert_eq!(context["prior_rides"], 2);
        assert_eq!(
            context["observed_consecutive_training_days_including_current"],
            3
        );
        assert_eq!(context["prior_21_days"]["current_strict_fastest"], true);
        assert_eq!(
            context["power_rankings_in_cached_history"]["1800s"]["strict_new_best"],
            false
        );
        assert_eq!(
            context["power_rankings_in_cached_history"]["480s"]["historical_comparable_rides"],
            0
        );
        assert!(duration("01:60:00").is_none());
        assert_eq!(duration("01:00:18.000"), Some(3618.0));
    }
}
