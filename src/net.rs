use anyhow::{bail, Context, Result};
use reqwest::{
    blocking::{Client, Response},
    Method, StatusCode,
};
use serde_json::Value;
use std::{fmt, thread, time::Duration};

#[derive(Debug)]
pub struct ApiFailure {
    pub status: u16,
    pub code: Option<i64>,
}
impl fmt::Display for ApiFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "接口请求失败（HTTP={}，code={:?}）",
            self.status, self.code
        )
    }
}
impl std::error::Error for ApiFailure {}
pub fn unauthorized(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ApiFailure>()
        .is_some_and(|e| e.status == 401 || e.code == Some(401))
}

#[derive(Clone)]
pub struct Http {
    pub client: Client,
    pub retries: u8,
}
impl Http {
    pub fn new(timeout: u64, retries: u8) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(timeout))
                .http1_only()
                .user_agent("mywhoosh-to-igpsport/0.1.0")
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            retries,
        })
    }
    pub fn send(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, String)],
        body: Option<Vec<u8>>,
    ) -> Result<Response> {
        self.send_with_retry(method, url, headers, body, false)
    }
    pub fn send_readonly_post(
        &self,
        url: &str,
        headers: &[(&str, String)],
        body: Option<Vec<u8>>,
    ) -> Result<Response> {
        self.send_with_retry(Method::POST, url, headers, body, true)
    }
    fn send_with_retry(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, String)],
        body: Option<Vec<u8>>,
        readonly: bool,
    ) -> Result<Response> {
        let safe = readonly || method == Method::GET || method == Method::PUT;
        for attempt in 0..=self.retries {
            let mut request = self.client.request(method.clone(), url);
            for (key, value) in headers {
                request = request.header(*key, value);
            }
            if let Some(bytes) = &body {
                request = request.body(bytes.clone());
            }
            match request.send() {
                Ok(response) => {
                    if safe
                        && (response.status() == StatusCode::TOO_MANY_REQUESTS
                            || response.status().is_server_error())
                        && attempt < self.retries
                    {
                        println!(
                            "  服务暂时不可用（HTTP {}），准备重试（{}/{}）……",
                            response.status().as_u16(),
                            attempt + 1,
                            self.retries
                        );
                        thread::sleep(Duration::from_secs((1u64 << attempt).min(8)));
                        continue;
                    }
                    return Ok(response);
                }
                Err(_) if safe && attempt < self.retries => {
                    println!(
                        "  网络请求失败或超时，准备重试（{}/{}）……",
                        attempt + 1,
                        self.retries
                    );
                    thread::sleep(Duration::from_secs((1u64 << attempt).min(8)))
                }
                Err(_) => bail!("网络请求失败或超时；请求地址和凭据未输出"),
            }
        }
        unreachable!()
    }
}

pub fn json_response(response: Response, envelope: bool) -> Result<Value> {
    let status = response.status().as_u16();
    let value: Value = response
        .json()
        .map_err(|_| ApiFailure { status, code: None })?;
    let code = value.get("code").and_then(Value::as_i64);
    if !(200..300).contains(&status) || (envelope && code != Some(0)) {
        return Err(ApiFailure { status, code }.into());
    }
    if envelope {
        Ok(value.get("data").context("接口缺少 data")?.clone())
    } else {
        Ok(value)
    }
}

pub fn text(value: &Value, key: &str) -> Result<String> {
    match value.get(key) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        _ => bail!("接口缺少有效 {key}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    #[test]
    fn readonly_post_retries_503_but_mutating_post_does_not() {
        for readonly in [false, true] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let server = thread::spawn(move || {
                for index in 0..if readonly { 2 } else { 1 } {
                    let (mut socket, _) = listener.accept().unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut buf = [0; 4096];
                    assert!(socket.read(&mut buf).unwrap() > 0);
                    let status = if index == 0 {
                        "503 Service Unavailable"
                    } else {
                        "200 OK"
                    };
                    write!(
                        socket,
                        "HTTP/1.1 {status}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
                    )
                    .unwrap();
                }
            });
            let http = Http {
                client: Client::builder()
                    .no_proxy()
                    .timeout(Duration::from_secs(5))
                    .build()
                    .unwrap(),
                retries: 1,
            };
            let response = if readonly {
                http.send_readonly_post(&url, &[], None)
            } else {
                http.send(Method::POST, &url, &[], None)
            }
            .unwrap();
            assert_eq!(response.status().as_u16(), if readonly { 200 } else { 503 });
            server.join().unwrap();
        }
    }
}
