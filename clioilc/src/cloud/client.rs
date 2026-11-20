//! Minimal blocking client for the backend API (v1).

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;
use ureq::Agent;

pub struct Client {
    base: String,
    token: Option<String>,
    agent: Agent,
}

/// A non-2xx answer. `error` is the API's machine-readable code, if any.
#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub error: Option<String>,
    pub body: Value,
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (&self.error, self.body.get("detail")) {
            (Some(code), _) => write!(f, "server answered {} ({code})", self.status),
            (None, Some(detail)) => write!(f, "server answered {}: {detail}", self.status),
            (None, None) => write!(f, "server answered {}: {}", self.status, self.body),
        }
    }
}

impl std::error::Error for ApiError {}

impl Client {
    pub fn new(server: &str, token: Option<&str>) -> Result<Self> {
        let base = server.trim_end_matches('/').to_string();
        if !(base.starts_with("https://") || is_local(&base)) {
            bail!("server must use https:// (http:// is only allowed for localhost)");
        }
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)))
            .user_agent(concat!("clioilc/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();
        Ok(Self {
            base,
            token: token.map(str::to_string),
            agent,
        })
    }

    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let mut req = self.agent.get(format!("{}{path}", self.base));
        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let resp = req.call().with_context(|| format!("GET {path}"))?;
        decode(resp, path)
    }

    pub fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        let mut req = self.agent.post(format!("{}{path}", self.base));
        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let resp = req
            .send_json(body)
            .with_context(|| format!("POST {path}"))?;
        decode(resp, path)
    }

    pub fn delete(&self, path: &str) -> Result<()> {
        let mut req = self.agent.delete(format!("{}{path}", self.base));
        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        let resp = req.call().with_context(|| format!("DELETE {path}"))?;
        if resp.status().is_success() {
            return Ok(());
        }
        Err(api_error(resp).into())
    }
}

fn is_local(base: &str) -> bool {
    ["http://localhost", "http://127.0.0.1", "http://[::1]"]
        .iter()
        .any(|p| {
            base == *p || base.starts_with(&format!("{p}:")) || base.starts_with(&format!("{p}/"))
        })
}

fn decode<T: DeserializeOwned>(
    mut resp: ureq::http::Response<ureq::Body>,
    path: &str,
) -> Result<T> {
    if !resp.status().is_success() {
        return Err(api_error(resp).into());
    }
    resp.body_mut()
        .read_json()
        .with_context(|| format!("unexpected response from {path}"))
}

fn api_error(mut resp: ureq::http::Response<ureq::Body>) -> ApiError {
    let status = resp.status().as_u16();
    let body = resp
        .body_mut()
        .read_to_string()
        .ok()
        .and_then(|s| {
            serde_json::from_str::<Value>(&s)
                .ok()
                .or(Some(Value::String(s)))
        })
        .unwrap_or(Value::Null);
    let error = body
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_string);
    ApiError {
        status,
        error,
        body,
    }
}

/// The API error behind `err`, if it is one.
pub fn api_error_of(err: &anyhow::Error) -> Option<&ApiError> {
    err.downcast_ref::<ApiError>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_or_localhost() {
        assert!(Client::new("https://api.example.com", None).is_ok());
        assert!(Client::new("http://localhost:8000/", None).is_ok());
        assert!(Client::new("http://127.0.0.1:8000", None).is_ok());
        assert!(Client::new("http://api.example.com", None).is_err());
        assert!(Client::new("http://localhost.evil.com", None).is_err());
    }
}
