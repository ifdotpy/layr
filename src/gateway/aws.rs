//! Instance credentials, per-project role sessions, and the shared signed S3 transport.

use super::{auth, config::Project, server::{Entry, Object, Role, Storage}};
use crate::http;
use crate::store_remote::{aws::{self, Credentials}, s3::S3};
use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::{Client, Response};
use reqwest::Method;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone)]
struct Timed { credentials: Credentials, expires: u64 }
impl Timed { fn fresh(&self) -> bool { self.expires > auth::now().saturating_add(300_000) } }

pub struct Aws {
    http: Client,
    metadata: String,
    base: Mutex<Option<Timed>>,
    roles: Mutex<BTreeMap<(String, String), Timed>>,
    #[cfg(debug_assertions)]
    test: Option<(String, Credentials)>,
}

fn body(response: Response, maximum: usize) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|n| n > maximum as u64) { bail!("AWS response is too large"); }
    let mut bytes = Vec::new();
    response.take(maximum as u64 + 1).read_to_end(&mut bytes).map_err(|_| anyhow!("AWS response body read failed"))?;
    if bytes.len() > maximum { bail!("AWS response is too large"); }
    Ok(bytes)
}

fn checked(response: Response, operation: &str, maximum: usize) -> Result<Vec<u8>> {
    if !response.status().is_success() { bail!("{operation}: HTTP {}", response.status().as_u16()); }
    body(response, maximum)
}

fn expiry(text: &str) -> Result<u64> {
    chrono::DateTime::parse_from_rfc3339(text)?.timestamp_millis().try_into().context("invalid AWS credential expiry")
}

impl Aws {
    pub fn new(test_static: bool) -> Result<Self> {
        let http = Client::builder().user_agent("layr-gateway").no_proxy()
            .redirect(reqwest::redirect::Policy::none()).connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30)).build()?;
        let metadata = if std::env::var("AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE").ok().is_some_and(|v| v.eq_ignore_ascii_case("ipv6")) {
            "http://[fd00:ec2::254]"
        } else { "http://169.254.169.254" };
        #[cfg(debug_assertions)]
        let test = if test_static {
            let endpoint = std::env::var("AWS_ENDPOINT_URL_S3").context("test mode needs AWS_ENDPOINT_URL_S3")?;
            let url = reqwest::Url::parse(&endpoint)?;
            if url.scheme() != "http" || url.host_str().and_then(|s| s.parse::<std::net::IpAddr>().ok()).is_none_or(|ip| !ip.is_loopback())
                || url.path() != "/" || url.query().is_some() || url.fragment().is_some() || !url.username().is_empty() || url.password().is_some()
            { bail!("test S3 endpoint must be a loopback HTTP origin"); }
            let credentials = Credentials { key_id: std::env::var("AWS_ACCESS_KEY_ID")?,
                secret: std::env::var("AWS_SECRET_ACCESS_KEY")?, token: std::env::var("AWS_SESSION_TOKEN").ok() };
            Some((endpoint.trim_end_matches('/').into(), credentials))
        } else { None };
        #[cfg(not(debug_assertions))]
        if test_static { bail!("test credentials are unavailable in release builds"); }
        Ok(Self { http, metadata: metadata.into(), base: Mutex::new(None), roles: Mutex::new(BTreeMap::new()),
            #[cfg(debug_assertions)] test })
    }

    fn instance(&self) -> Result<Credentials> {
        let mut cached = self.base.lock().map_err(|_| anyhow!("instance credential lock failed"))?;
        if let Some(timed) = cached.as_ref().filter(|t| t.fresh()) { return Ok(timed.credentials.clone()); }
        let response = self.http.put(format!("{}/latest/api/token", self.metadata))
            .header("x-aws-ec2-metadata-token-ttl-seconds", "300").timeout(Duration::from_secs(5))
            .send().map_err(reqwest::Error::without_url)?;
        let token = String::from_utf8(checked(response, "IMDSv2 token", 4096)?)?;
        if token.is_empty() { bail!("IMDSv2 returned an empty token"); }
        let get = |suffix: &str| -> Result<Response> {
            self.http.get(format!("{}/latest/meta-data/iam/security-credentials/{suffix}", self.metadata))
                .header("x-aws-ec2-metadata-token", &token).timeout(Duration::from_secs(5))
                .send().map_err(reqwest::Error::without_url).map_err(Into::into)
        };
        let names = String::from_utf8(checked(get("")?, "IMDSv2 role", 4096)?)?;
        let role = names.lines().next().filter(|s| !s.is_empty() && s.len() <= 64
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"+=,.@_-".contains(&b))).context("instance has no valid role")?;
        let value: serde_json::Value = serde_json::from_slice(&checked(get(role)?, "IMDSv2 credentials", 32 * 1024)?)?;
        if value["Code"] != "Success" { bail!("IMDSv2 credentials are not available"); }
        let field = |name: &str| value[name].as_str().filter(|s| !s.is_empty()).map(str::to_owned).context("incomplete instance credentials");
        let timed = Timed { credentials: Credentials { key_id: field("AccessKeyId")?, secret: field("SecretAccessKey")?,
            token: Some(field("Token")?) }, expires: expiry(&field("Expiration")?)? };
        if timed.expires <= auth::now() { bail!("instance credentials expired"); }
        let credentials = timed.credentials.clone();
        *cached = Some(timed);
        Ok(credentials)
    }

    fn credentials(&self, project: &Project, role: Role) -> Result<Credentials> {
        #[cfg(debug_assertions)]
        if let Some((_, credentials)) = &self.test { return Ok(credentials.clone()); }
        let arn = match role { Role::Reader => &project.reader_role_arn, Role::Writer => &project.writer_role_arn, Role::Gc => &project.gc_role_arn };
        let cache_key = (arn.clone(), project.region.clone());
        let mut cached = self.roles.lock().map_err(|_| anyhow!("project credential lock failed"))?;
        if let Some(timed) = cached.get(&cache_key).filter(|t| t.fresh()) { return Ok(timed.credentials.clone()); }
        let base = self.instance()?;
        let data = format!("Action=AssumeRole&RoleArn={}&RoleSessionName=layr-gateway&DurationSeconds=900&Version=2011-06-15",
                           aws::uri_encode(arn, false)).into_bytes();
        let endpoint = format!("https://sts.{}.api.aws", project.region);
        let host = format!("sts.{}.api.aws", project.region);
        let headers = [("content-type", "application/x-www-form-urlencoded; charset=utf-8")];
        let signed = aws::sign(&aws::Request { method: "POST", host: &host, path: "/", query: &[], headers: &headers,
            payload_sha256: &hex::encode(Sha256::digest(&data)) }, &base, &project.region, "sts", auth::now() / 1000);
        let mut request = self.http.post(endpoint).header(headers[0].0, headers[0].1).body(data);
        for (name, value) in signed { request = request.header(name, value); }
        let response = request.send().map_err(reqwest::Error::without_url)?;
        let xml = String::from_utf8(checked(response, "STS AssumeRole", 64 * 1024)?)?;
        let field = |name| aws::xml_text(&xml, name).filter(|s| !s.is_empty()).context("incomplete role credentials");
        let timed = Timed { credentials: Credentials { key_id: field("AccessKeyId")?, secret: field("SecretAccessKey")?,
            token: Some(field("SessionToken")?) }, expires: expiry(&field("Expiration")?)? };
        if timed.expires <= auth::now() { bail!("role credentials expired"); }
        let credentials = timed.credentials.clone();
        if cached.len() >= 4096 { cached.clear(); }
        cached.insert(cache_key, timed);
        Ok(credentials)
    }

    fn s3(&self, project: &Project, role: Role) -> Result<S3> {
        #[cfg(debug_assertions)]
        let endpoint = self.test.as_ref().map(|(endpoint, _)| endpoint.as_str());
        #[cfg(not(debug_assertions))]
        let endpoint = None;
        S3::new_with_client(&project.bucket, &project.region, self.credentials(project, role)?, endpoint, self.http.clone())
    }
}

impl Storage for Aws {
    fn get(&self, project: &Project, key: &str, range: Option<http::Range>, limit: usize) -> Result<Option<Object>> {
        let s3 = self.s3(project, Role::Reader)?;
        let range_header = range.map(http::Range::header);
        let headers: Vec<(&str, &str)> = range_header.as_deref().map(|s| vec![("range", s)]).unwrap_or_default();
        let response = s3.request(Method::GET, &project.key(key), &[], &headers, Vec::new(), Duration::from_secs(30))?;
        let status = response.status().as_u16();
        if status == 416 { return Err(http::Error { status: 416, message: "range is outside the object" }.into()); }
        if status == 404 {
            let xml = String::from_utf8(body(response, 16 * 1024)?)?;
            if aws::xml_text(&xml, "Code").as_deref() == Some("NoSuchKey") { return Ok(None); }
            bail!("S3 get: HTTP 404");
        }
        if !matches!(status, 200 | 206) { bail!("S3 get: HTTP {status}"); }
        let content_range = if status == 206 {
            let header = response.headers().get("content-range").context("S3 omitted Content-Range")?.to_str()?;
            let parsed = http::ContentRange::parse(header).context("invalid S3 Content-Range")?;
            if range.and_then(|r| r.resolve(parsed.total)) != Some((parsed.start, parsed.end)) { bail!("S3 returned a different byte range"); }
            Some(parsed)
        } else { None };
        let bytes = body(response, limit)?;
        if content_range.as_ref().is_some_and(|r| r.end - r.start + 1 != bytes.len() as u64) { bail!("S3 returned an incorrect range length"); }
        Ok(Some(Object { status, body: bytes, content_range: content_range.map(|r| r.header()) }))
    }

    fn list(&self, project: &Project, prefix: &str) -> Result<Vec<Entry>> {
        let s3 = self.s3(project, Role::Reader)?;
        let root = project.key("");
        let prefix = project.key(prefix);
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut seen = BTreeSet::new();
        let mut token = None;
        let mut entries = Vec::new();
        loop {
            if Instant::now() >= deadline || seen.len() > 100 { bail!("S3 list deadline reached"); }
            let mut query = vec![("list-type", "2"), ("prefix", prefix.as_str()), ("max-keys", "1000")];
            if let Some(token) = &token { query.push(("continuation-token", token.as_str())); }
            let response = s3.request(Method::GET, "", &query, &[], Vec::new(), Duration::from_secs(30))?;
            let xml = String::from_utf8(checked(response, "S3 list", 4 * 1024 * 1024)?)?;
            for item in aws::xml_texts(&xml, "Contents") {
                let key = aws::xml_text(&item, "Key").context("S3 list omitted key")?;
                if !key.starts_with(&prefix) { bail!("S3 list returned another prefix"); }
                let key = key.strip_prefix(&root).context("S3 list returned another project")?.to_owned();
                let modified = aws::xml_text(&item, "LastModified").map(|s| expiry(&s)).transpose()?.unwrap_or(0) as i64;
                entries.push(Entry { key, modified });
                if entries.len() > 10_000 { bail!("S3 list has too many objects"); }
            }
            token = aws::xml_text(&xml, "NextContinuationToken").filter(|s| !s.is_empty());
            match &token { None => break, Some(t) if !seen.insert(t.clone()) => bail!("S3 list repeated its continuation token"), _ => {} }
        }
        Ok(entries)
    }

    fn put(&self, project: &Project, key: &str, bytes: Vec<u8>) -> Result<bool> {
        let s3 = self.s3(project, Role::Writer)?;
        let response = s3.request(Method::PUT, &project.key(key), &[], &[("if-none-match", "*")], bytes, Duration::from_secs(120))?;
        match response.status().as_u16() { 200 | 201 => Ok(true), 412 => Ok(false), status => bail!("S3 conditional put: HTTP {status}") }
    }
    fn delete(&self, project: &Project, key: &str) -> Result<()> {
        let s3 = self.s3(project, Role::Gc)?;
        let response = s3.request(Method::DELETE, &project.key(key), &[], &[], Vec::new(), Duration::from_secs(30))?;
        match response.status().as_u16() { 200 | 204 | 404 => Ok(()), status => bail!("S3 delete: HTTP {status}") }
    }
}
