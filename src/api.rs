use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::Duration,
};

#[derive(Clone)]
pub struct Api {
    pub port: u16,
    pub secret: String,
}
#[derive(Clone, Debug, PartialEq)]
pub struct Group {
    pub name: String,
    pub kind: String,
    pub current: String,
    pub choices: Vec<String>,
}

impl Api {
    pub fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value> {
        ensure!(
            path.starts_with('/')
                && !path.contains(['\r', '\n'])
                && !self.secret.contains(['\r', '\n']),
            "控制请求参数无效"
        );
        let address = SocketAddr::from(([127, 0, 0, 1], self.port));
        let mut connection = TcpStream::connect_timeout(&address, Duration::from_millis(800))
            .context("无法连接内核")?;
        connection.set_read_timeout(Some(Duration::from_secs(2)))?;
        connection.set_write_timeout(Some(Duration::from_secs(2)))?;
        let bytes = body
            .map(|b| serde_json::to_vec(&b))
            .transpose()?
            .unwrap_or_default();
        write!(
            connection,
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            self.port,
            self.secret,
            bytes.len()
        )?;
        connection.write_all(&bytes)?;
        let mut response = Vec::new();
        connection
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut response)?;
        ensure!(response.len() <= 8 * 1024 * 1024, "控制接口响应过大");
        let (status, bytes) = decode_response(&response)?;
        ensure!(
            (200..300).contains(&status),
            "内核控制接口返回 HTTP {status}"
        );
        if bytes.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(serde_json::from_slice(&bytes)?)
        }
    }
    pub fn select(&self, group: &str, choice: &str) -> Result<()> {
        self.request(
            "PUT",
            &format!("/proxies/{}", encode_segment(group)),
            Some(json!({"name":choice})),
        )?;
        Ok(())
    }
    pub fn groups(&self, order: &[String]) -> Result<Vec<Group>> {
        let root = self.request("GET", "/proxies", None)?;
        let proxies = root["proxies"].as_object().context("策略组响应无效")?;
        let mut names = order.to_vec();
        if !names.iter().any(|n| n == "GLOBAL") && proxies.contains_key("GLOBAL") {
            names.push("GLOBAL".into());
        }
        Ok(names
            .into_iter()
            .filter_map(|name| {
                let item = proxies.get(&name)?;
                let choices = item["all"]
                    .as_array()?
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect();
                Some(Group {
                    name,
                    kind: item["type"].as_str().unwrap_or_default().into(),
                    current: item["now"].as_str().unwrap_or_default().into(),
                    choices,
                })
            })
            .collect())
    }
}
pub fn encode_segment(input: &str) -> String {
    let mut result = String::new();
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            result.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(result, "%{byte:02X}").unwrap();
        }
    }
    result
}
fn decode_response(response: &[u8]) -> Result<(u16, Vec<u8>)> {
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("HTTP 响应头无效")?;
    let header = std::str::from_utf8(&response[..split])?;
    let status = header
        .lines()
        .next()
        .and_then(|s| s.split_whitespace().nth(1))
        .context("HTTP 状态缺失")?
        .parse()?;
    let body = &response[split + 4..];
    if !header.lines().any(|line| {
        line.split_once(':').is_some_and(|(k, v)| {
            k.eq_ignore_ascii_case("transfer-encoding") && v.trim().eq_ignore_ascii_case("chunked")
        })
    }) {
        return Ok((status, body.to_vec()));
    }
    let mut remaining = body;
    let mut decoded = Vec::new();
    loop {
        let end = remaining
            .windows(2)
            .position(|w| w == b"\r\n")
            .context("HTTP 分块响应无效")?;
        let length = usize::from_str_radix(
            std::str::from_utf8(&remaining[..end])?
                .split(';')
                .next()
                .unwrap()
                .trim(),
            16,
        )?;
        remaining = &remaining[end + 2..];
        if length == 0 {
            break;
        }
        if length > remaining.len().saturating_sub(2) {
            bail!("HTTP 分块响应被截断");
        }
        decoded.extend_from_slice(&remaining[..length]);
        ensure!(
            &remaining[length..length + 2] == b"\r\n",
            "HTTP 分块结束符无效"
        );
        remaining = &remaining[length + 2..];
    }
    Ok((status, decoded))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunks_and_unicode_paths() {
        let (status, bytes) = decode_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\n{\"x\r\n4\r\n\":1}\r\n0\r\n\r\n").unwrap();
        assert_eq!(status, 200);
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            json!({"x":1})
        );
        assert_eq!(encode_segment("a/b 空格"), "a%2Fb%20%E7%A9%BA%E6%A0%BC");
        assert!(
            decode_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nff\r\nx")
                .is_err()
        );
    }
}
