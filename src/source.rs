use crate::{
    cli::Credentials,
    fit,
    net::{self, Http},
    storage,
};
use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use md5::{Digest, Md5};
use rand::RngCore;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashSet, path::PathBuf};

const BASE: &str = "https://connectapp-api.mywhoosh.com/";
const LOGIN_URL: &str = "https://services.mywhoosh.com/http-service/api/login";
const FORCE_LOGOUT_URL: &str = "https://connectapp-api.mywhoosh.com/auth/force-logout";

fn login_type(value: &Value) -> Option<i64> {
    value["LoginType"]
        .as_i64()
        .or_else(|| value["LoginType"].as_str().and_then(|s| s.parse().ok()))
}

fn login_flow(body: &Value, mut post: impl FnMut(&str, &Value) -> Result<Value>) -> Result<String> {
    let mut value = post(LOGIN_URL, body)?;
    if login_type(&value) == Some(4) {
        let user_id = net::text(&value, "WhooshId")
            .context("MyWhoosh 登录凭证冲突响应缺少 WhooshId，无法强制登出")?;
        ensure!(
            !user_id.trim().is_empty(),
            "MyWhoosh WhooshId 为空，无法强制登出"
        );
        println!("MyWhoosh 检测到其他设备的登录凭证，正在强制登出其他设备……");
        post(FORCE_LOGOUT_URL, &json!({"userId": user_id})).context("MyWhoosh 强制登出请求失败")?;
        println!("强制登出请求已完成，正在重新登录 MyWhoosh……");
        value = post(LOGIN_URL, body)?;
        ensure!(
            login_type(&value) != Some(4),
            "MyWhoosh 强制登出后仍提示登录凭证冲突，请稍后重试"
        );
    }
    ensure!(login_type(&value) == Some(1), "MyWhoosh 登录未成功");
    net::text(&value, "AccessToken")
}
#[derive(Default, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub device_id: String,
}
pub struct Source {
    pub http: Http,
    pub session: Session,
    pub path: PathBuf,
    pub credentials: Credentials,
    pub id: String,
    pub ftp: f64,
}
impl Source {
    pub fn new(
        http: Http,
        path: PathBuf,
        credentials: Credentials,
        device: Option<String>,
    ) -> Result<Self> {
        let mut session: Session = storage::load(&path)?;
        if let Some(value) = device {
            ensure!(
                value.len() == 16 && value.chars().all(|c| c.is_ascii_hexdigit()),
                "device-id 必须是 16 位十六进制"
            );
            session.device_id = value;
        }
        if session.device_id.is_empty() {
            let mut bytes = [0u8; 8];
            rand::thread_rng().fill_bytes(&mut bytes);
            session.device_id = hex::encode(bytes);
        }
        Ok(Self {
            http,
            session,
            path,
            credentials,
            id: String::new(),
            ftp: 0.0,
        })
    }
    fn headers(&self) -> Vec<(&'static str, String)> {
        let mut headers = vec![
            ("Content-Type", "application/json".into()),
            ("deviceType", "ANDROID".into()),
            ("User-Agent", "okhttp/5.0.0-alpha.2".into()),
        ];
        if !self.session.access_token.is_empty() {
            headers.push((
                "Authorization",
                format!("Bearer {}", self.session.access_token),
            ));
        }
        headers
    }
    pub fn login(&mut self) -> Result<()> {
        println!("MyWhoosh 需要登录，正在获取登录凭据……");
        let (username, password) = self.credentials.get("MyWhoosh")?;
        println!("正在登录 MyWhoosh……");
        let body = json!({"UserName":username,"Password":password,"Source":"connect","DeviceId":self.session.device_id});
        let headers = vec![
            ("Content-Type", "application/json".into()),
            ("deviceType", "ANDROID".into()),
            ("User-Agent", "okhttp/5.0.0-alpha.2".into()),
        ];
        let token = login_flow(&body, |url, request| {
            let response = self.http.send(
                Method::POST,
                url,
                &headers,
                Some(serde_json::to_vec(request)?),
            )?;
            if url == FORCE_LOGOUT_URL {
                // The Android app treats a successful HTTP response as completion.
                if !response.status().is_success() {
                    return Err(net::ApiFailure {
                        status: response.status().as_u16(),
                        code: None,
                    }
                    .into());
                }
                Ok(Value::Null)
            } else {
                net::json_response(response, false)
            }
        })?;
        self.session.access_token = token;
        storage::save(&self.path, &self.session)?;
        println!("MyWhoosh 登录成功，登录凭证已保存。");
        Ok(())
    }
    pub fn json(&mut self, path: &str) -> Result<Value> {
        if self.session.access_token.is_empty() {
            self.login()?;
        }
        for attempt in 0..2 {
            let result = self
                .http
                .send(Method::GET, &format!("{BASE}{path}"), &self.headers(), None)
                .and_then(|r| net::json_response(r, false));
            match result {
                Err(error) if attempt == 0 && net::unauthorized(&error) => self.login()?,
                other => return other,
            }
        }
        unreachable!()
    }
    pub fn connect(&mut self, force: bool) -> Result<()> {
        if force {
            self.login()?;
        }
        let profile = self.json("users/get-my-profile")?;
        self.id = net::text(&profile["data"], "userId")?;
        self.ftp = profile["data"]["stats"]["ftp"]
            .as_f64()
            .or_else(|| {
                profile["data"]["stats"]["ftp"]
                    .as_str()
                    .and_then(|s| s.parse().ok())
            })
            .context("个人资料缺少 FTP")?;
        ensure!(self.ftp.is_finite() && self.ftp > 0.0, "个人资料 FTP 无效");
        Ok(())
    }
    pub fn activities(
        &mut self,
        max_pages: u32,
        since: Option<chrono::NaiveDate>,
    ) -> Result<Vec<Value>> {
        let mut result = Vec::new();
        let mut seen = HashSet::new();
        for page in 1..=max_pages {
            println!("正在读取 MyWhoosh 活动列表，第 {page} 页……");
            let response = self.web_activities(page)?;
            let data = &response["data"];
            let rows = data["results"]
                .as_array()
                .context("MyWhoosh 活动分页结构异常")?;
            for row in rows {
                let id = net::text(row, "id")?;
                if !seen.insert(id.clone()) {
                    continue;
                }
                if let Some(date) = since {
                    let start = row["startDatetime"]
                        .as_str()
                        .or_else(|| row["createdAt"].as_str())
                        .context("源活动缺少筛选日期")?;
                    let start = chrono::NaiveDate::parse_from_str(
                        start.get(..10).unwrap_or(""),
                        "%Y-%m-%d",
                    )?;
                    if start < date {
                        continue;
                    }
                }
                let mut row = row.clone();
                row["activityId"] = json!(id);
                result.push(row);
            }
            let total = data["totalPages"]
                .as_u64()
                .context("MyWhoosh 列表缺少总页数")?;
            if page as u64 >= total {
                return Ok(result);
            }
            ensure!(!rows.is_empty(), "MyWhoosh 活动分页提前结束");
        }
        bail!("MyWhoosh 达到 max-pages，活动分页不完整")
    }
    fn web_activities(&mut self, page: u32) -> Result<Value> {
        if self.session.access_token.is_empty() {
            self.login()?;
        }
        for attempt in 0..2 {
            let response = self
                .http
                .send_readonly_post(
                    "https://service14.mywhoosh.com/v2/rider/profile/activities",
                    &self.headers(),
                    Some(serde_json::to_vec(
                        &json!({"type":0,"page":page,"sortDate":"DESC"}),
                    )?),
                )
                .and_then(|r| net::json_response(r, false));
            match response {
                Err(e) if attempt == 0 && net::unauthorized(&e) => self.login()?,
                Ok(value) => {
                    ensure!(
                        value["error"] == false && value["code"] == 200,
                        "MyWhoosh 网页活动列表请求失败"
                    );
                    return Ok(value);
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }
    pub fn download(&mut self, activity_id: &str) -> Result<Vec<u8>> {
        ensure!(
            activity_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "活动 ID 无效"
        );
        let detail = self.json(&format!("activities/{activity_id}"))?;
        let file_id = net::text(&detail["data"], "userFileId")?;
        for attempt in 0..2 {
            let encrypted = encrypt_fit_request(&file_id)?;
            let response = self.http.send(
                Method::POST,
                "https://service14.mywhoosh.com/v2/rider/profile/download-activity-file",
                &self.headers(),
                Some(serde_json::to_vec(&encrypted)?),
            )?;
            match net::json_response(response, false) {
                Err(e) if attempt == 0 && net::unauthorized(&e) => {
                    self.login()?;
                    continue;
                }
                Err(e) => return Err(e),
                Ok(value) => {
                    ensure!(value["error"] != true, "官网 FIT 下载接口失败");
                    let url = net::text(&value, "data")?;
                    ensure!(url.starts_with("https://"), "下载地址必须为 HTTPS");
                    let response = self.http.send(Method::GET, &url, &[], None)?;
                    ensure!(response.status().is_success(), "原始 FIT 下载失败");
                    let raw = response
                        .bytes()
                        .map_err(|_| anyhow::anyhow!("FIT 下载中断"))?
                        .to_vec();
                    fit::validate(&raw)?;
                    return Ok(raw);
                }
            }
        }
        unreachable!()
    }
}

fn encrypt_fit_request(file_id: &str) -> Result<String> {
    let plain = serde_json::to_vec(&json!({"fileId":file_id}))?;
    let mut salt = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut salt);
    Ok(encrypt_with_salt(&plain, &salt))
}
fn encrypt_with_salt(plain: &[u8], salt: &[u8; 8]) -> String {
    let mut material = Vec::new();
    let mut previous = Vec::new();
    while material.len() < 48 {
        let mut digest = Md5::new();
        digest.update(&previous);
        digest.update(b"D9436E508087E863");
        digest.update(salt);
        previous = digest.finalize().to_vec();
        material.extend_from_slice(&previous);
    }
    let cipher =
        cbc::Encryptor::<aes::Aes256>::new_from_slices(&material[..32], &material[32..48]).unwrap();
    let ciphertext = cipher.encrypt_padded_vec_mut::<Pkcs7>(plain);
    let mut result = b"Salted__".to_vec();
    result.extend_from_slice(salt);
    result.extend(ciphertext);
    STANDARD.encode(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn login_conflict_logs_out_then_retries_same_credentials() {
        let body = json!({"UserName":"test@example.test", "Password":"secret", "Source":"connect", "DeviceId":"0123456789abcdef"});
        let mut calls = Vec::new();
        let token = login_flow(&body, |url, request| {
            calls.push(url.to_owned());
            match calls.len() {
                1 => {
                    assert_eq!(request, &body);
                    Ok(json!({"LoginType":"4", "WhooshId":"account-id"}))
                }
                2 => {
                    assert_eq!(url, FORCE_LOGOUT_URL);
                    assert_eq!(request, &json!({"userId":"account-id"}));
                    Ok(Value::Null)
                }
                3 => {
                    assert_eq!(request, &body);
                    Ok(json!({"LoginType":1,"AccessToken":"new-token"}))
                }
                _ => panic!("unexpected retry"),
            }
        })
        .unwrap();
        assert_eq!(token, "new-token");
        assert_eq!(calls, [LOGIN_URL, FORCE_LOGOUT_URL, LOGIN_URL]);
    }
    #[test]
    fn login_does_not_force_logout_without_identified_conflict() {
        for response in [
            json!({"LoginType":1,"AccessToken":"token"}),
            json!({"LoginType":2}),
            json!({"LoginType":4}),
            json!({"LoginType":4,"WhooshId":" "}),
        ] {
            let mut calls = 0;
            let result = login_flow(&json!({}), |url, _| {
                calls += 1;
                assert_eq!(url, LOGIN_URL);
                Ok(response.clone())
            });
            assert_eq!(result.is_ok(), login_type(&response) == Some(1));
            assert_eq!(calls, 1);
        }
    }
    #[test]
    fn force_logout_failure_stops_and_conflict_retry_is_bounded() {
        for fail_logout in [false, true] {
            let mut calls = 0;
            let result = login_flow(&json!({}), |url, _| {
                calls += 1;
                if url == FORCE_LOGOUT_URL {
                    if fail_logout {
                        bail!("simulated network failure");
                    }
                    return Ok(Value::Null);
                }
                Ok(json!({"LoginType":4,"WhooshId":"account-id"}))
            });
            assert!(result.is_err());
            assert_eq!(calls, if fail_logout { 2 } else { 3 });
        }
    }
    #[test]
    fn session_serialization_has_no_password() {
        let s = Session {
            access_token: "token".into(),
            device_id: "device".into(),
        };
        let value = serde_json::to_value(s).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert!(value.get("password").is_none());
    }
    #[test]
    fn encryption_has_openssl_envelope() {
        let encoded = encrypt_with_salt(b"{\"fileId\":\"test\"}", &[1; 8]);
        assert_eq!(
            encoded,
            "U2FsdGVkX18BAQEBAQEBAQ+Zgm6PSnGcZXFZaGO/nwdWMMitb+Y2Cb9Pe8sGKRud"
        );
        let value = STANDARD.decode(encoded).unwrap();
        assert_eq!(&value[..8], b"Salted__");
        assert_eq!(&value[8..16], &[1; 8]);
        assert_eq!((value.len() - 16) % 16, 0);
    }
}
