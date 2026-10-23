use crate::{
    cli::Credentials,
    net::{self, ApiFailure, Http},
    sign::Signer,
    storage,
};
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{NaiveDate, Utc};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashSet, path::PathBuf};

const BASE: &str = "https://prod.zh.igpsport.com";
const ACT: &str = "/service/web-gateway/web-analyze/activity/";

fn collect_pages(
    max_pages: u32,
    mut fetch: impl FnMut(u32) -> Result<Value>,
) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    let mut expected = None;
    for page in 1..=max_pages {
        let data = fetch(page)?;
        let total = data["totalRows"]
            .as_u64()
            .or_else(|| data["totalRows"].as_str().and_then(|v| v.parse().ok()))
            .context("目标列表缺少有效总条数，无法确认去重范围完整")?;
        ensure!(
            expected.is_none_or(|n| n == total),
            "目标活动总数在翻页期间变化，请重试"
        );
        expected = Some(total);
        let batch = data["rows"].as_array().context("目标活动列表结构异常")?;
        for row in batch {
            let id = net::text(row, "rideId")?;
            ensure!(
                seen.insert(id),
                "目标分页重复返回活动，无法确认去重范围完整"
            );
            rows.push(row.clone());
        }
        ensure!(rows.len() as u64 <= total, "目标列表总条数不一致");
        if rows.len() as u64 == total {
            return Ok(rows);
        }
        ensure!(
            !batch.is_empty(),
            "目标列表分页提前结束，停止上传以避免重复"
        );
    }
    anyhow::bail!("目标日期范围尚未读完，已达到 --max-pages 上限；本轮停止上传")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagination_requires_all_rows_even_with_short_pages() {
        let rows = collect_pages(3, |page| {
            let ids: Vec<_> = match page {
                1 => (1..=20).collect(),
                2 => (21..=40).collect(),
                _ => (41..=43).collect(),
            };
            Ok(json!({"totalRows":43,"rows":ids.into_iter().map(|id| json!({"rideId":id})).collect::<Vec<_>>()}))
        }).unwrap();
        assert_eq!(rows.len(), 43);
        assert!(collect_pages(1, |_| Ok(json!({"totalRows":2,"rows":[{"rideId":1}]}))).is_err());
        assert!(collect_pages(3, |_| Ok(json!({"totalRows":2,"rows":[{"rideId":1}]}))).is_err());
        assert!(collect_pages(3, |_| Ok(json!({"totalRows":2,"rows":[]}))).is_err());
        assert!(collect_pages(3, |_| Ok(json!({"totalRows":0,"rows":[]})))
            .unwrap()
            .is_empty());
    }
}
#[derive(Default, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub token: String,
}
pub struct Target {
    pub http: Http,
    pub session: Session,
    pub path: PathBuf,
    pub credentials: Credentials,
    pub identity: String,
    timezone: String,
    signer: Signer,
    keys: Option<(String, String)>,
    offset: i64,
}

impl Target {
    pub fn new(
        http: Http,
        path: PathBuf,
        credentials: Credentials,
        timezone: String,
    ) -> Result<Self> {
        let session = storage::load(&path)?;
        Ok(Self {
            http,
            session,
            path,
            credentials,
            identity: String::new(),
            timezone,
            signer: Signer::new()?,
            keys: None,
            offset: 0,
        })
    }
    fn bootstrap(&mut self) -> Result<()> {
        let mut headers = vec![("Origin", "https://app.igpsport.cn".into())];
        if !self.session.token.is_empty() {
            headers.push(("Authorization", format!("Bearer {}", self.session.token)));
        }
        let data = net::json_response(
            self.http.send(
                Method::GET,
                &format!("{BASE}/service/edge-core/api/public/key"),
                &headers,
                None,
            )?,
            true,
        )?;
        self.keys = Some((
            net::text(&data, "access_key")?,
            net::text(&data, "secret_key")?,
        ));
        self.offset = 0;
        if let Ok(response) = self.http.send(
            Method::GET,
            &format!("{BASE}/service/edge-core/api/public/timestamp"),
            &[],
            None,
        ) {
            if let Ok(v) = net::json_response(response, true) {
                if let Some(server) = v
                    .as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                {
                    self.offset = server - Utc::now().timestamp();
                }
            }
        }
        Ok(())
    }
    fn signed(
        &mut self,
        method: Method,
        path: &str,
        body: Option<Value>,
        authenticated: bool,
    ) -> Result<Value> {
        let serialized = body
            .map(|v| serde_json::to_string(&v))
            .transpose()?
            .unwrap_or_default();
        for attempt in 0..2 {
            if self.keys.is_none() {
                self.bootstrap()?;
            }
            let (access, secret) = self.keys.as_ref().unwrap();
            let timestamp = (Utc::now().timestamp() + self.offset).to_string();
            let nonce = uuid::Uuid::new_v4().to_string();
            let signature = self.signer.sign(
                secret,
                method.as_str(),
                path,
                &timestamp,
                &nonce,
                &serialized,
            )?;
            let mut headers = vec![
                ("Origin", "https://app.igpsport.cn".into()),
                ("Referer", "https://app.igpsport.cn/".into()),
                ("Accept-Language", "zh-Hans".into()),
                ("TimeZone", self.timezone.clone()),
                ("Qiwu-App-Version", "8.07.08".into()),
                ("X-Platform", "web".into()),
                ("Content-Type", "application/json".into()),
                ("X-Access-Key", access.clone()),
                ("X-Timestamp", timestamp),
                ("X-Nonce", nonce),
                ("X-Signature", signature),
            ];
            if authenticated {
                headers.push(("Authorization", format!("Bearer {}", self.session.token)));
            }
            let response = self.http.send(
                method.clone(),
                &format!("{BASE}{path}"),
                &headers,
                if serialized.is_empty() {
                    None
                } else {
                    Some(serialized.as_bytes().to_vec())
                },
            )?;
            let result = net::json_response(response, true);
            if attempt == 0
                && result
                    .as_ref()
                    .err()
                    .and_then(|e| e.downcast_ref::<ApiFailure>())
                    .is_some_and(|e| e.status == 403 && matches!(e.code, Some(1020..=1022)))
            {
                self.keys = None;
                continue;
            }
            return result;
        }
        unreachable!()
    }
    pub fn login(&mut self) -> Result<()> {
        println!("iGPSPORT 需要登录，正在获取登录凭据……");
        let (username, password) = self.credentials.get("iGPSPORT")?;
        println!("正在登录 iGPSPORT……");
        let body = json!({"appId":"igpsport-web","username":username,"password":password});
        let data = self.signed(
            Method::POST,
            "/service/auth/account/login",
            Some(body),
            false,
        )?;
        self.session.token = net::text(&data, "access_token")?;
        self.keys = None;
        self.identify()?;
        storage::save(&self.path, &self.session)?;
        println!("iGPSPORT 登录成功，登录凭证已保存。");
        Ok(())
    }
    fn identify(&mut self) -> Result<()> {
        let payload = self
            .session
            .token
            .split('.')
            .nth(1)
            .context("目标登录凭证格式错误")?;
        let claims: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload.trim_end_matches('='))?)?;
        self.identity = net::text(&claims, "sub").or_else(|_| net::text(&claims, "memberId"))?;
        Ok(())
    }
    pub fn api(&mut self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        if self.session.token.is_empty() {
            self.login()?;
        }
        for attempt in 0..2 {
            match self.signed(method.clone(), path, body.clone(), true) {
                Err(e) if attempt == 0 && net::unauthorized(&e) => self.login()?,
                other => return other,
            }
        }
        unreachable!()
    }
    pub fn connect(&mut self, force: bool) -> Result<()> {
        if force {
            self.login()?;
        }
        self.activities(1, 20, None)?;
        self.identify()
    }
    pub fn activities(&mut self, page: u32, size: u32, title: Option<&str>) -> Result<Value> {
        self.filtered_activities(page, size, title, None)
    }
    fn filtered_activities(
        &mut self,
        page: u32,
        size: u32,
        title: Option<&str>,
        dates: Option<(NaiveDate, NaiveDate)>,
    ) -> Result<Value> {
        let mut url = reqwest::Url::parse(&format!("{BASE}{ACT}queryMyActivity"))?;
        url.query_pairs_mut()
            .append_pair("reqType", "1")
            .append_pair("sportType", "0")
            .append_pair("pageNo", &page.to_string())
            .append_pair("pageSize", &size.to_string())
            .append_pair("sort", "1");
        if let Some(title) = title {
            url.query_pairs_mut().append_pair("title", title);
        }
        if let Some((begin, end)) = dates {
            url.query_pairs_mut()
                .append_pair("beginTime", &begin.to_string())
                .append_pair("endTime", &end.to_string());
        }
        let path = format!("{}?{}", url.path(), url.query().unwrap()).replace('+', "%20");
        let mut data = self.api(Method::GET, &path, None)?;
        if data.is_null() {
            data = json!({"rows":[],"totalRows":0});
        }
        if data["rows"].is_null() && data["totalRows"] == 0 {
            data["rows"] = json!([]);
        }
        ensure!(data["rows"].is_array(), "目标活动列表结构异常");
        Ok(data)
    }
    pub fn detail(&mut self, id: &str) -> Result<Value> {
        ensure!(id.chars().all(|c| c.is_ascii_digit()), "目标 ride_id 无效");
        self.api(Method::GET, &format!("{ACT}queryActivityDetail/{id}"), None)
    }
    pub fn in_date_range(
        &mut self,
        begin: NaiveDate,
        end: NaiveDate,
        max_pages: u32,
    ) -> Result<Vec<Value>> {
        println!("正在读取 iGPSPORT 活动：{begin} 至 {end}（含首尾日期），每页 20 条……");
        collect_pages(max_pages, |page| {
            println!("  正在读取 iGPSPORT 第 {page} 页……");
            self.filtered_activities(page, 20, None, Some((begin, end)))
        })
    }
    pub fn put_fit(&mut self, raw: &[u8]) -> Result<String> {
        ensure!(raw.len() <= 20 * 1024 * 1024, "FIT 超过 20MB");
        let grant = self.api(
            Method::GET,
            "/service/sportg/third-party-server/oss/getSignedUrl?fileExtension=.fit",
            None,
        )?;
        let url = net::text(&grant, "signedUrl")?;
        ensure!(url.starts_with("https://"), "OSS 地址必须为 HTTPS");
        let response = self.http.send(
            Method::PUT,
            &url,
            &[("Content-Type", String::new())],
            Some(raw.to_vec()),
        )?;
        ensure!(response.status().is_success(), "FIT 对象上传失败");
        net::text(&grant, "ossId")
    }
    pub fn submit(&mut self, oss: &str, filename: &str) -> Result<()> {
        self.api(
            Method::POST,
            &format!("{ACT}uploadByOss"),
            Some(json!({"fileName":filename,"ossName":oss})),
        )?;
        Ok(())
    }
    pub fn rename(&mut self, id: &str, title: &str) -> Result<()> {
        self.api(
            Method::PUT,
            &format!("{ACT}update"),
            Some(json!({"rideId":id,"title":title})),
        )?;
        Ok(())
    }
    pub fn describe(&mut self, id: &str, description: &str) -> Result<()> {
        self.api(
            Method::PUT,
            &format!("{ACT}update"),
            Some(json!({"rideId":id,"desc":description})),
        )?;
        Ok(())
    }
}
