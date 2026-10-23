//! Tailnet object gateway. It runs independently of the local Layr service and store.

mod auth;
mod config;
mod tls;
mod server;

use anyhow::{anyhow, bail, Context, Result};
use std::path::PathBuf;

struct Options {
    config: PathBuf,
    tls: String,
    cert: Option<PathBuf>,
    key: Option<PathBuf>,
    tls_dir: PathBuf,
    test_static: bool,
}

pub fn command(args: &[String]) -> Result<()> {
    let mut options = Options { config: "/etc/layr-gateway/config.json".into(), tls: "files".into(),
        cert: None, key: None, tls_dir: "/etc/layr-gateway/tls".into(), test_static: false };
    let mut positional = Vec::new();
    let mut write = false;
    let mut gc = false;
    let mut until = "24h".to_string();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--write" => write = true,
            "--gc" => gc = true,
            "--test-static-credentials" if cfg!(debug_assertions) => options.test_static = true,
            "--config" | "--tls" | "--cert" | "--key" | "--tls-dir" | "--until" => {
                index += 1;
                let value = args.get(index).ok_or_else(|| anyhow!("{arg} needs a value"))?;
                match arg.as_str() {
                    "--config" => options.config = value.into(), "--tls" => options.tls = value.clone(),
                    "--cert" => options.cert = Some(value.into()), "--key" => options.key = Some(value.into()),
                    "--tls-dir" => options.tls_dir = value.into(), "--until" => until = value.clone(), _ => unreachable!(),
                }
            }
            "--help" | "-h" => {
                println!("usage: layr gateway --config <file> [--tls files|tailscale|none] [--cert <pem> --key <pem>]\n       layr gateway token <project> --write|--gc [--until <duration>] [--config <file>]");
                return Ok(());
            }
            value if value.starts_with('-') => bail!("unknown gateway option {value}"),
            value => positional.push(value.to_string()),
        }
        index += 1;
    }
    if positional.first().map(String::as_str) == Some("token") {
        if positional.len() != 2 || write == gc || options.test_static {
            bail!("usage: layr gateway token <project> --write|--gc [--until <duration>] [--config <file>]");
        }
        let project = &positional[1];
        if !config::project_name(project) { bail!("invalid gateway project name"); }
        let token = auth::issue(gc, auth::duration(&until)?, auth::now())?;
        config::add_hash(&options.config, project, auth::hash(&token), gc)?;
        println!("{token}");
        return Ok(());
    }
    if !positional.is_empty() || write || gc || until != "24h" { bail!("invalid gateway server arguments"); }
    if rustix::process::geteuid().is_root() { bail!("layr gateway refuses to run as root; use a dedicated service user"); }
    run(options)
}

fn run(_options: Options) -> Result<()> {
    bail!("gateway server transport is under construction")
}

fn tls_config(options: &Options, config: &config::Config) -> Result<Option<std::sync::Arc<rustls::ServerConfig>>> {
    let (cert, key) = match options.tls.as_str() {
        "none" => {
            if !config.listen.ip().is_loopback() { bail!("--tls none requires a loopback listen address"); }
            return Ok(None);
        }
        "tailscale" => {
            if !config.host.ends_with(".ts.net") { bail!("--tls tailscale requires a ts.net host"); }
            (options.tls_dir.join(format!("{}.crt", config.host)), options.tls_dir.join(format!("{}.key", config.host)))
        }
        "files" => {
            let directory = std::env::var_os("CREDENTIALS_DIRECTORY").map(PathBuf::from);
            let cert = options.cert.clone().or_else(|| directory.as_ref().map(|p| p.join("tls.crt")));
            let key = options.key.clone().or_else(|| directory.as_ref().map(|p| p.join("tls.key")));
            (cert.context("give --cert or systemd credential tls.crt")?, key.context("give --key or systemd credential tls.key")?)
        }
        _ => bail!("--tls must be files, tailscale or none"),
    };
    Ok(Some(tls::server(std::sync::Arc::new(tls::Reloading::new(cert, key, tls::provider())?))?))
}

fn allowed_address(ip: std::net::IpAddr) -> bool {
    if ip.is_loopback() { return true; }
    match ip {
        std::net::IpAddr::V4(ip) => { let b = ip.octets(); b[0] == 100 && (64..128).contains(&b[1]) }
        std::net::IpAddr::V6(ip) => ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

trait Wire: std::io::Read + std::io::Write { fn begin_response(&mut self, timeout: std::time::Duration); }
impl Wire for crate::http::Socket { fn begin_response(&mut self, timeout: std::time::Duration) { self.begin_write(timeout); } }
impl Wire for rustls::StreamOwned<rustls::ServerConnection, crate::http::Socket> {
    fn begin_response(&mut self, timeout: std::time::Duration) { self.sock.begin_write(timeout); }
}

fn answer<W: Wire>(stream: &mut W, gateway: &server::Gateway, limits: &crate::http::Limits) -> Result<()> {
    let reply = match crate::http::request(stream, limits) {
        Ok(request) => gateway.handle(request),
        Err(error) => server::Reply::error(error.status, error.message),
    };
    stream.begin_response(limits.write_timeout);
    crate::http::response(stream, reply.status, &reply.headers, &reply.body)?;
    Ok(())
}

fn serve(options: Options, storage: std::sync::Arc<dyn server::Storage>) -> Result<()> {
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
    use std::time::Duration;
    let file = Arc::new(config::ConfigFile::new(options.config.clone()));
    let config = file.get()?;
    if !allowed_address(config.listen.ip()) { bail!("gateway listen must use a tailnet or loopback address"); }
    let tls = tls_config(&options, &config)?;
    if options.test_static && (tls.is_some() || !config.listen.ip().is_loopback()) {
        bail!("test credentials require --tls none on loopback");
    }
    let listener = std::net::TcpListener::bind(config.listen).context("bind gateway listen address")?;
    let limits = crate::http::Limits { request_line: 4096, header_count: 64, header_bytes: 16 * 1024,
        body_bytes: server::OBJECT_LIMIT, header_timeout: Duration::from_secs(15),
        body_timeout: Duration::from_secs(120), write_timeout: Duration::from_secs(60) };
    let gateway = Arc::new(server::Gateway { config: file, listen: config.listen, proof: auth::Proof::new(), storage });
    let active = Arc::new(AtomicUsize::new(0));
    eprintln!("layr gateway: listening on {}", config.listen);
    for incoming in listener.incoming() {
        let stream = match incoming { Ok(stream) => stream, Err(error) => { eprintln!("layr gateway: accept failed: {error}"); continue; } };
        if active.load(Ordering::Acquire) >= 4 { continue; }
        limits.configure(&stream)?;
        active.fetch_add(1, Ordering::AcqRel);
        let (active, gateway, tls) = (active.clone(), gateway.clone(), tls.clone());
        std::thread::spawn(move || {
            struct Slot(Arc<AtomicUsize>);
            impl Drop for Slot { fn drop(&mut self) { self.0.fetch_sub(1, Ordering::AcqRel); } }
            let _slot = Slot(active);
            let _ = (|| -> Result<()> {
                let mut socket = crate::http::Socket::new(stream, Duration::from_secs(10));
                match tls {
                    Some(tls) => {
                        let mut connection = rustls::ServerConnection::new(tls)?;
                        while connection.is_handshaking() { connection.complete_io(&mut socket)?; }
                        socket.begin_read(limits.header_timeout + limits.body_timeout, limits.header_timeout);
                        let mut stream = rustls::StreamOwned::new(connection, socket);
                        answer(&mut stream, &gateway, &limits)
                    }
                    None => {
                        socket.begin_read(limits.header_timeout + limits.body_timeout, limits.header_timeout);
                        answer(&mut socket, &gateway, &limits)
                    }
                }
            })();
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wildcard_public_private_lan_and_plaintext_tailnet_binds_are_refused() {
        for address in ["0.0.0.0", "::", "192.168.1.1", "10.0.0.1", "8.8.8.8", "fd00::1"] {
            assert!(!allowed_address(address.parse().unwrap()));
        }
        for address in ["127.0.0.1", "::1", "100.64.0.1", "100.127.255.254", "fd7a:115c:a1e0::123"] {
            assert!(allowed_address(address.parse().unwrap()));
        }
        let mut config = config::sample();
        config.listen = "100.64.0.1:443".parse().unwrap();
        let options = Options { config: "unused".into(), tls: "none".into(), cert: None, key: None, tls_dir: "unused".into(), test_static: false };
        assert!(tls_config(&options, &config).is_err());
    }
}
