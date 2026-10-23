//! Opt-in integration test of the actual CLI against a local MinIO server.

use super::{auth, config};
use crate::store_remote::{aws::Credentials, s3::S3};
use base64::Engine;
use reqwest::blocking::Client;
use reqwest::Method;
use serde_json::json;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Server(Child);
impl Drop for Server { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
struct Files(PathBuf);
impl Drop for Files { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
struct Bucket(S3);
impl Drop for Bucket {
    fn drop(&mut self) {
        if let Ok(objects) = self.0.list("") { for object in objects { let _ = self.0.delete(&object.key); } }
        let _ = self.0.request(Method::DELETE, "", &[], &[], Vec::new(), Duration::from_secs(10));
    }
}
fn token(binary: &Path, config: &Path, gc: bool) -> String {
    let output = Command::new(binary).args(["gateway", "token", "example", if gc { "--gc" } else { "--write" },
        "--until", "1h", "--config"]).arg(config).output().unwrap();
    assert!(output.status.success(), "token command failed");
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn raw(address: std::net::SocketAddr, request: &str) -> u16 {
    let mut socket = TcpStream::connect(address).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    socket.write_all(request.as_bytes()).unwrap();
    let mut line = String::new();
    BufReader::new(socket).read_line(&mut line).unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
#[ignore = "requires layr-dev root, a debug layr binary, and a loopback MinIO endpoint"]
fn actual_cli_minio_round_trip() {
    assert!(rustix::process::geteuid().is_root(), "run this test as root in layr-dev");
    let endpoint = std::env::var("LAYR_MINIO_ENDPOINT").expect("set LAYR_MINIO_ENDPOINT to a local test MinIO server");
    let url = reqwest::Url::parse(&endpoint).unwrap();
    assert_eq!(url.scheme(), "http");
    assert!(url.host_str().unwrap().parse::<std::net::IpAddr>().unwrap().is_loopback(), "MinIO must use loopback");
    let credentials = Credentials { key_id: std::env::var("LAYR_MINIO_ACCESS_KEY").unwrap(),
        secret: std::env::var("LAYR_MINIO_SECRET_KEY").unwrap(), token: None };
    let home = Files(std::env::temp_dir().join(format!("layr-gateway-minio-{:016x}", rand::random::<u64>())));
    fs::create_dir(&home.0).unwrap();
    fs::set_permissions(&home.0, fs::Permissions::from_mode(0o755)).unwrap();
    let source = PathBuf::from(std::env::var_os("LAYR_GATEWAY_BINARY").expect("set LAYR_GATEWAY_BINARY to target/debug/layr"));
    let binary = home.0.join("layr");
    fs::copy(source, &binary).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    let identity = age::x25519::Identity::generate();
    let mut config = config::sample();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    config.listen = address;
    let project = config.projects.get_mut("example").unwrap();
    project.bucket = format!("layr-gateway-test-{:016x}", rand::random::<u64>());
    project.region = "us-east-1".into();
    project.recipient = identity.to_public().to_string();
    let s3 = Bucket(S3::new(&project.bucket, &project.region, credentials.clone(), Some(&endpoint)).unwrap());
    let response = s3.0.request(Method::PUT, "", &[], &[], Vec::new(), Duration::from_secs(10)).unwrap();
    assert!(response.status().is_success(), "create MinIO test bucket failed");
    let descriptor = json!({"format":2,"project":crate::ids::new_id(),"recipient":project.recipient});
    assert!(s3.0.put_new(&project.key("store.json"), descriptor.to_string().into_bytes()).unwrap());
    let path = home.0.join("config.json");
    fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let no_store = home.0.join("must-not-create-store");
    let root = Command::new(&binary).args(["gateway", "--tls", "none", "--config"]).arg(&path)
        .env("LAYR_ROOT", &no_store).output().unwrap();
    assert!(!root.status.success());
    assert!(String::from_utf8(root.stderr).unwrap().contains("refuses to run as root"));
    let log = fs::File::create(home.0.join("server.log")).unwrap();
    let child = Command::new("setpriv").args(["--reuid=nobody", "--regid=nogroup", "--clear-groups"]).arg(&binary)
        .args(["gateway", "--tls", "none", "--test-static-credentials", "--config"]).arg(&path)
        .env("LAYR_ROOT", &no_store).env("AWS_ENDPOINT_URL_S3", &endpoint)
        .env("AWS_ACCESS_KEY_ID", &credentials.key_id).env("AWS_SECRET_ACCESS_KEY", &credentials.secret)
        .stdout(Stdio::null()).stderr(log).spawn().unwrap();
    let _server = Server(child);
    let client = Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(15)).build().unwrap();
    let origin = format!("http://{address}");
    let request = |method, target: &str| client.request(method, format!("{origin}{target}")).header("Host", "localhost");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if request(Method::GET, "/v1/p/example/challenge").send().is_ok() { break; }
        assert!(Instant::now() < deadline, "gateway did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(request(Method::GET, "/v1/p/example/store.json").send().unwrap().json::<serde_json::Value>().unwrap(), descriptor);
    assert_eq!(request(Method::GET, "/v1/p/example/objects?prefix=states/").send().unwrap().status(), 401);
    let challenge: serde_json::Value = request(Method::GET, "/v1/p/example/challenge").send().unwrap().json().unwrap();
    let sealed = base64::engine::general_purpose::STANDARD.decode(challenge["challenge"].as_str().unwrap()).unwrap();
    assert!(age::decrypt(&age::x25519::Identity::generate(), &sealed).is_err());
    let proof = String::from_utf8(age::decrypt(&identity, &sealed).unwrap()).unwrap();
    let issued: serde_json::Value = request(Method::POST, "/v1/p/example/token").body(json!({"challenge":proof}).to_string())
        .send().unwrap().json().unwrap();
    let read = issued["token"].as_str().unwrap();
    let write = token(&binary, &path, false);
    let gc = token(&binary, &path, true);
    let bytes = age::encrypt(&identity.to_public(), b"the gateway cannot decrypt this").unwrap();
    let data_path = "/v1/p/example/objects/states/state/01234567.0001.data.age";
    assert_eq!(request(Method::PUT, data_path).bearer_auth(&write).body(bytes.clone()).send().unwrap().status(), 201);
    assert_eq!(request(Method::PUT, data_path).bearer_auth(&write).body(b"overwrite".to_vec()).send().unwrap().status(), 412);
    assert_eq!(request(Method::GET, data_path).bearer_auth(&write).send().unwrap().status(), 401);
    let found = request(Method::GET, data_path).bearer_auth(read).send().unwrap().bytes().unwrap();
    assert_eq!(found.as_ref(), bytes);
    let range = request(Method::GET, data_path).bearer_auth(read).header("Range", "bytes=1-5").send().unwrap();
    assert_eq!(range.status(), 206);
    assert_eq!(range.headers()["content-range"], format!("bytes 1-5/{}", bytes.len()));
    assert_eq!(range.bytes().unwrap().as_ref(), &bytes[1..=5]);
    assert_eq!(request(Method::GET, data_path).bearer_auth(read).header("Range", "bytes=9999999-").send().unwrap().status(), 416);
    let slot_path = "/v1/p/example/objects/slots/machine-new.age";
    assert_eq!(request(Method::PUT, slot_path).bearer_auth(&write).body(b"slot".to_vec()).send().unwrap().status(), 403);
    assert_eq!(request(Method::PUT, slot_path).bearer_auth(read).body(b"slot".to_vec()).send().unwrap().status(), 201);
    assert_eq!(request(Method::PUT, "/v1/p/example/objects/store.json").bearer_auth(&write).body(b"{}".to_vec()).send().unwrap().status(), 400);
    assert_eq!(request(Method::DELETE, slot_path).bearer_auth(&gc).send().unwrap().status(), 403);
    assert_eq!(request(Method::DELETE, data_path).bearer_auth(&write).send().unwrap().status(), 403);
    assert_eq!(request(Method::DELETE, data_path).bearer_auth(&gc).send().unwrap().status(), 204);
    assert_eq!(request(Method::GET, data_path).bearer_auth(read).send().unwrap().status(), 404);
    for malformed in [
        "POST /v1/p/example/token HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
        "GET /v1/p/example/objects/records/%2e%2e/store.json HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "GET /v1/p/example/slots HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
    ] { assert_eq!(raw(address, malformed), 400); }
    // Force ListObjectsV2 pagination through the actual HTTP endpoint.
    let bulk = Arc::new(S3::new(&config.projects["example"].bucket, "us-east-1", credentials, Some(&endpoint)).unwrap());
    std::thread::scope(|scope| {
        for worker in 0..8 {
            let s3 = &bulk;
            scope.spawn(move || {
                for n in (worker..1005).step_by(8) {
                    assert!(s3.put_new(&format!("example/records/bulk/{n:04}.age"), vec![1]).unwrap());
                }
            });
        }
    });
    let listed: serde_json::Value = request(Method::GET, "/v1/p/example/objects?prefix=records/").bearer_auth(read).send().unwrap().json().unwrap();
    assert_eq!(listed["objects"].as_array().unwrap().len(), 1005);
    let mut changed = config::read(&path).unwrap();
    changed.projects.get_mut("example").unwrap().write_token_hashes.clear();
    fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert_eq!(request(Method::PUT, data_path).bearer_auth(&write).body(bytes).send().unwrap().status(), 403);
    assert!(!no_store.exists(), "gateway opened a local project store");
    let persisted = fs::read_to_string(&path).unwrap();
    assert!(!persisted.contains(&write));
    assert!(!persisted.contains(&gc));
    assert!(persisted.contains(&auth::hash(&gc)));
}
