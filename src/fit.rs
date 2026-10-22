use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

pub const VERSION: i64 = 2;
const EPOCH: i64 = 631065600;

pub fn crc(raw: &[u8]) -> u16 {
    let mut value = 0u16;
    for byte in raw {
        value ^= *byte as u16;
        for _ in 0..8 {
            value = (value >> 1) ^ if value & 1 != 0 { 0xa001 } else { 0 };
        }
    }
    value
}

pub fn validate(raw: &[u8]) -> Result<()> {
    ensure!(raw.len() >= 14 && &raw[8..12] == b".FIT", "FIT 签名无效");
    let header = raw[0] as usize;
    ensure!(matches!(header, 12 | 14), "FIT 头长度无效");
    let size = u32::from_le_bytes(raw[4..8].try_into()?) as usize;
    ensure!(header + size + 2 == raw.len(), "FIT 文件长度无效");
    if header == 14 && raw[12..14] != [0, 0] {
        ensure!(crc(&raw[..14]) == 0, "FIT 头 CRC 无效");
    }
    ensure!(crc(raw) == 0, "FIT 文件 CRC 无效");
    Ok(())
}

#[derive(Clone)]
struct Field {
    id: u8,
    size: u8,
    base: u8,
}
#[derive(Clone)]
struct Definition {
    local: u8,
    global: u16,
    big: bool,
    fields: Vec<Field>,
    dev: Vec<[u8; 3]>,
}
#[derive(Clone)]
struct Message {
    def: Definition,
    data: Vec<u8>,
}
enum Frame {
    Definition(Vec<u8>),
    Data(Message),
}

impl Message {
    fn number(&self, id: u8) -> Option<u64> {
        let mut offset = 1;
        for f in &self.def.fields {
            let bytes = &self.data[offset..offset + f.size as usize];
            if f.id == id {
                let value = match f.size {
                    1 => bytes[0] as u64,
                    2 => {
                        (if self.def.big {
                            u16::from_be_bytes(bytes.try_into().ok()?)
                        } else {
                            u16::from_le_bytes(bytes.try_into().ok()?)
                        }) as u64
                    }
                    4 => {
                        (if self.def.big {
                            u32::from_be_bytes(bytes.try_into().ok()?)
                        } else {
                            u32::from_le_bytes(bytes.try_into().ok()?)
                        }) as u64
                    }
                    _ => return None,
                };
                if value == (1u64 << (f.size * 8)) - 1 {
                    return None;
                }
                return Some(value);
            }
            offset += f.size as usize;
        }
        None
    }
    fn rewrite(&self, replacements: &BTreeMap<u8, (u8, u8, u64)>) -> Result<Vec<u8>> {
        let mut remaining = replacements.clone();
        let mut fields = Vec::new();
        let mut payload = vec![self.def.local];
        let mut offset = 1;
        for f in &self.def.fields {
            fields.extend_from_slice(&[f.id, f.size, f.base]);
            let data = &self.data[offset..offset + f.size as usize];
            if let Some((size, base, value)) = remaining.remove(&f.id) {
                ensure!(f.size == size && f.base == base, "FIT 汇总字段类型异常");
                append_number(&mut payload, value, size, self.def.big);
            } else {
                payload.extend_from_slice(data);
            }
            offset += f.size as usize;
        }
        for (id, (size, base, value)) in remaining {
            fields.extend_from_slice(&[id, size, base]);
            append_number(&mut payload, value, size, self.def.big);
        }
        payload.extend_from_slice(&self.data[offset..]); // Preserve developer bytes.
        let dev = !self.def.dev.is_empty();
        let mut output = vec![
            if dev { 0x60 } else { 0x40 } | self.def.local,
            0,
            self.def.big as u8,
        ];
        output.extend_from_slice(&if self.def.big {
            self.def.global.to_be_bytes()
        } else {
            self.def.global.to_le_bytes()
        });
        output.push((fields.len() / 3).try_into()?);
        output.extend(fields);
        if dev {
            output.push(self.def.dev.len().try_into()?);
            for d in &self.def.dev {
                output.extend_from_slice(d);
            }
        }
        output.extend(payload);
        Ok(output)
    }
}

fn append_number(out: &mut Vec<u8>, value: u64, size: u8, big: bool) {
    if big {
        out.extend_from_slice(&value.to_be_bytes()[8 - size as usize..]);
    } else {
        out.extend_from_slice(&value.to_le_bytes()[..size as usize]);
    }
}

fn parse(raw: &[u8]) -> Result<Vec<Frame>> {
    validate(raw)?;
    // Semantic validation uses an established FIT parser; raw frames are retained for lossless editing.
    fitparser::from_bytes(raw).map_err(|_| anyhow::anyhow!("FIT 结构解析失败"))?;
    let mut definitions: Vec<Option<Definition>> = vec![None; 16];
    let mut pos = raw[0] as usize;
    let end = raw.len() - 2;
    let mut frames = Vec::new();
    while pos < end {
        let start = pos;
        let header = raw[pos];
        pos += 1;
        ensure!(header & 0x80 == 0, "暂不支持压缩时间戳 FIT，未生成上传副本");
        let local = header & 0xf;
        if header & 0x40 != 0 {
            ensure!(pos + 5 <= end, "FIT 定义截断");
            let big = raw[pos + 1] != 0;
            let global = if big {
                u16::from_be_bytes(raw[pos + 2..pos + 4].try_into()?)
            } else {
                u16::from_le_bytes(raw[pos + 2..pos + 4].try_into()?)
            };
            let count = raw[pos + 4] as usize;
            pos += 5;
            ensure!(pos + count * 3 <= end, "FIT 字段截断");
            let mut fields = Vec::new();
            for _ in 0..count {
                fields.push(Field {
                    id: raw[pos],
                    size: raw[pos + 1],
                    base: raw[pos + 2],
                });
                pos += 3;
            }
            let mut dev = Vec::new();
            if header & 0x20 != 0 {
                ensure!(pos < end, "FIT developer 定义截断");
                let count = raw[pos] as usize;
                pos += 1;
                ensure!(pos + count * 3 <= end, "FIT developer 字段截断");
                for _ in 0..count {
                    dev.push(raw[pos..pos + 3].try_into()?);
                    pos += 3;
                }
            }
            definitions[local as usize] = Some(Definition {
                local,
                global,
                big,
                fields,
                dev,
            });
            frames.push(Frame::Definition(raw[start..pos].to_vec()));
        } else {
            let def = definitions[local as usize]
                .as_ref()
                .context("FIT 缺少本地定义")?
                .clone();
            let len: usize = def.fields.iter().map(|f| f.size as usize).sum::<usize>()
                + def.dev.iter().map(|d| d[1] as usize).sum::<usize>();
            ensure!(pos + len <= end, "FIT 数据截断");
            pos += len;
            frames.push(Frame::Data(Message {
                def,
                data: raw[start..pos].to_vec(),
            }));
        }
    }
    Ok(frames)
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Report {
    pub start_utc: String,
    pub expected_start: String,
    pub max_speed_mps: f64,
    pub normalized_power: u64,
    pub intensity_factor: f64,
    pub training_stress_score: f64,
    pub threshold_power: u64,
    pub record_count: usize,
}

pub fn start(raw: &[u8]) -> Result<DateTime<Utc>> {
    let frames = parse(raw)?;
    let sessions: Vec<_> = frames
        .iter()
        .filter_map(|f| match f {
            Frame::Data(m) if m.def.global == 18 => Some(m),
            _ => None,
        })
        .collect();
    ensure!(sessions.len() == 1, "FIT 必须有单个 session");
    let seconds = sessions[0].number(2).context("FIT 缺少开始时间")?;
    Utc.timestamp_opt(seconds as i64 + EPOCH, 0)
        .single()
        .context("FIT 开始时间无效")
}

pub struct Summary {
    pub start: DateTime<Utc>,
    pub seconds: f64,
    pub distance: f64,
    pub ascent: f64,
}

pub fn summary(raw: &[u8]) -> Result<Summary> {
    let start = start(raw)?;
    let frames = parse(raw)?;
    let session = frames
        .iter()
        .find_map(|frame| match frame {
            Frame::Data(m) if m.def.global == 18 => Some(m),
            _ => None,
        })
        .context("FIT 缺少 session")?;
    Ok(Summary {
        start,
        seconds: session.number(7).context("FIT 缺少记录时长，无法去重")? as f64 / 1000.0,
        distance: session.number(9).context("FIT 缺少距离，无法去重")? as f64 / 100.0,
        ascent: session.number(22).context("FIT 缺少总爬升，无法去重")? as f64,
    })
}

pub fn prepare(raw: &[u8], tz: chrono_tz::Tz, ftp: f64) -> Result<(Vec<u8>, Report)> {
    ensure!(
        ftp.is_finite() && ftp > 0.0 && ftp < 65535.0,
        "个人资料 FTP 无效"
    );
    let frames = parse(raw)?;
    let messages: Vec<_> = frames
        .iter()
        .filter_map(|f| {
            if let Frame::Data(m) = f {
                Some(m)
            } else {
                None
            }
        })
        .collect();
    let sessions: Vec<_> = messages.iter().filter(|m| m.def.global == 18).collect();
    let activities: Vec<_> = messages.iter().filter(|m| m.def.global == 34).collect();
    let records: Vec<_> = messages.iter().filter(|m| m.def.global == 20).collect();
    ensure!(
        sessions.len() == 1 && activities.len() == 1 && !records.is_empty(),
        "兼容处理需要单个 session、activity 和采样"
    );
    let session = sessions[0];
    let begin = session.number(2).context("缺少 FIT 开始时间")?;
    let timer = session.number(8).context("缺少计时时长")? as f64 / 1000.0;
    let elapsed = session.number(7).unwrap_or((timer * 1000.0) as u64) as f64 / 1000.0;
    ensure!(
        (timer - records.len() as f64).abs() <= 0.01 && (elapsed - timer).abs() <= 0.01,
        "采样包含暂停或时长不匹配，未估算功率指标"
    );
    let mut max_speed = None;
    let (mut rolling, mut fourth, mut count) = (0.0, 0.0, 0u64);
    let mut window = VecDeque::new();
    let mut previous = None;
    for record in &records {
        let timestamp = record.number(253).context("采样缺少时间戳")?;
        if let Some(last) = previous {
            ensure!(
                timestamp == last + 1,
                "采样不是连续每秒记录，未估算功率指标"
            );
        }
        previous = Some(timestamp);
        let power = record.number(7).context("功率采样缺失")? as f64;
        window.push_back(power);
        rolling += power;
        if window.len() > 30 {
            rolling -= window.pop_front().unwrap();
        }
        if window.len() == 30 {
            fourth += (rolling / 30.0).powi(4);
            count += 1;
        }
        if let Some(speed) = record.number(73).or_else(|| record.number(6)) {
            max_speed = Some(max_speed.unwrap_or(0).max(speed));
        }
    }
    ensure!(count > 0, "功率记录不足 30 秒");
    let np = (fourth / count as f64).powf(0.25);
    let intensity = np / ftp;
    let tss = timer / 3600.0 * intensity.powi(2) * 100.0;
    ensure!(
        np < 65534.5 && intensity < 65.5345 && tss < 6553.45,
        "功率指标超出 FIT 范围"
    );
    let maximum = max_speed.context("没有有效速度采样")?;
    let end = begin + (elapsed.round() as u64);
    let start = Utc
        .timestamp_opt(begin as i64 + EPOCH, 0)
        .single()
        .context("开始时间异常")?;
    let end_time = Utc
        .timestamp_opt(end as i64 + EPOCH, 0)
        .single()
        .context("结束时间异常")?;
    use chrono::Offset;
    let local =
        (end as i64 + end_time.with_timezone(&tz).offset().fix().local_minus_utc() as i64) as u64;
    let report = Report {
        start_utc: start.to_rfc3339(),
        expected_start: start
            .with_timezone(&tz)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        max_speed_mps: maximum as f64 / 1000.0,
        normalized_power: np.round() as u64,
        intensity_factor: (intensity * 1000.0).round() / 1000.0,
        training_stress_score: (tss * 10.0).round() / 10.0,
        threshold_power: ftp.round() as u64,
        record_count: records.len(),
    };
    let replacements = BTreeMap::from([
        (253, (4, 0x86, end)),
        (15, (2, 0x84, maximum.min(65535))),
        (125, (4, 0x86, maximum)),
        (34, (2, 0x84, report.normalized_power)),
        (
            35,
            (
                2,
                0x84,
                (report.training_stress_score * 10.0).round() as u64,
            ),
        ),
        (
            36,
            (2, 0x84, (report.intensity_factor * 1000.0).round() as u64),
        ),
        (45, (2, 0x84, report.threshold_power)),
    ]);
    let mut payload = Vec::new();
    for frame in &frames {
        match frame {
            Frame::Definition(bytes) => payload.extend_from_slice(bytes),
            Frame::Data(m) if m.def.global == 18 => payload.extend(m.rewrite(&replacements)?),
            Frame::Data(m) if m.def.global == 34 => {
                payload.extend(m.rewrite(&BTreeMap::from([
                    (253, (4, 0x86, end)),
                    (5, (4, 0x86, local)),
                ]))?)
            }
            Frame::Data(m) => payload.extend_from_slice(&m.data),
        }
    }
    let header = raw[0] as usize;
    let mut output = raw[..header].to_vec();
    output[4..8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    if header == 14 {
        let value = crc(&output[..12]);
        output[12..14].copy_from_slice(&value.to_le_bytes());
    }
    output.extend(payload);
    output.extend_from_slice(&crc(&output).to_le_bytes());
    let checked = parse(&output)?;
    let after: Vec<_> = checked
        .iter()
        .filter_map(|f| match f {
            Frame::Data(m) if m.def.global == 20 => Some(&m.data),
            _ => None,
        })
        .collect();
    if records.iter().map(|m| &m.data).collect::<Vec<_>>() != after {
        bail!("采样字节发生变化，拒绝上传");
    }
    Ok((output, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crc_reference() {
        assert_eq!(crc(b"123456789"), 0xbb3d);
    }
    #[test]
    fn damaged_fit_rejected() {
        assert!(validate(b"not a fit").is_err());
    }
    fn message(global: u16, fields: &[(u8, u8, u8, u64)]) -> Vec<u8> {
        let mut result = vec![0x40, 0, 0];
        result.extend_from_slice(&global.to_le_bytes());
        result.push(fields.len() as u8);
        for (id, size, base, _) in fields {
            result.extend_from_slice(&[*id, *size, *base]);
        }
        result.push(0);
        for (_, size, _, value) in fields {
            append_number(&mut result, *value, *size, false);
        }
        result
    }
    fn fixture(powers: &[u64], duration: u64) -> Vec<u8> {
        let begin = 1161259925u64;
        let mut payload = message(0, &[(0, 1, 0, 4), (1, 2, 0x84, 1)]);
        for (i, power) in powers.iter().enumerate() {
            payload.extend(message(
                20,
                &[
                    (253, 4, 0x86, begin + i as u64),
                    (6, 2, 0x84, 8394),
                    (7, 2, 0x84, *power),
                    (0, 4, 0x85, 471254495),
                    (1, 4, 0x85, 1619872000),
                ],
            ));
        }
        payload.extend(message(
            18,
            &[
                (253, 4, 0x86, begin),
                (2, 4, 0x86, begin),
                (7, 4, 0x86, duration * 1000),
                (8, 4, 0x86, duration * 1000),
                (9, 4, 0x86, 2698005),
                (22, 2, 0x84, 33),
            ],
        ));
        payload.extend(message(
            34,
            &[(253, 4, 0x86, begin), (5, 4, 0x86, 1792329152)],
        ));
        let mut raw = vec![14, 0x20, 0, 0];
        raw.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        raw.extend_from_slice(b".FIT");
        raw.extend_from_slice(&crc(&raw).to_le_bytes());
        raw.extend(payload);
        raw.extend_from_slice(&crc(&raw).to_le_bytes());
        raw
    }
    #[test]
    fn full_power_time_speed_and_record_preservation() {
        let original = fixture(&vec![200; 3600], 3600);
        let key = summary(&original).unwrap();
        assert_eq!(key.seconds, 3600.0);
        assert_eq!(key.distance, 26980.05);
        assert_eq!(key.ascent, 33.0);
        assert_eq!(key.start, start(&original).unwrap());
        let (fixed, report) = prepare(&original, chrono_tz::Asia::Shanghai, 250.0).unwrap();
        assert_eq!(report.normalized_power, 200);
        assert_eq!(report.intensity_factor, 0.8);
        assert_eq!(report.training_stress_score, 64.0);
        assert_eq!(report.max_speed_mps, 8.394);
        assert_eq!(report.expected_start, "2026-10-18 20:12:05");
        validate(&fixed).unwrap();
        let parsed = parse(&fixed).unwrap();
        let session = parsed
            .iter()
            .find_map(|f| match f {
                Frame::Data(m) if m.def.global == 18 => Some(m),
                _ => None,
            })
            .unwrap();
        assert_eq!(session.number(34), Some(200));
        assert_eq!(session.number(35), Some(640));
        assert_eq!(session.number(36), Some(800));
        assert_eq!(session.number(45), Some(250));
        assert_eq!(session.number(253), Some(1161259925 + 3600));
    }
    #[test]
    fn zero_power_and_complete_windows() {
        let mut powers = vec![300; 31];
        powers[0] = 0;
        let (_, r) = prepare(&fixture(&powers, 31), chrono_tz::UTC, 250.0).unwrap();
        let expected = ((290f64.powi(4) + 300f64.powi(4)) / 2.0).powf(0.25);
        assert_eq!(r.normalized_power, expected.round() as u64);
    }
    #[test]
    fn gaps_short_missing_power_and_invalid_ftp_rejected() {
        assert!(prepare(&fixture(&[100; 29], 29), chrono_tz::UTC, 250.0).is_err());
        assert!(prepare(&fixture(&[100; 30], 31), chrono_tz::UTC, 250.0).is_err());
        let mut powers = vec![100; 30];
        powers[3] = 65535;
        assert!(prepare(&fixture(&powers, 30), chrono_tz::UTC, 250.0).is_err());
        assert!(prepare(&fixture(&[100; 30], 30), chrono_tz::UTC, 0.0).is_err());
    }
}
