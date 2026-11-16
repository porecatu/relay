// SPDX-License-Identifier: GPL-3.0-or-later

//! Smoke test of the binary: it starts with the reference configuration file
//! and answers `/healthz`; and the refusals at startup have the exit code and
//! the message the operator needs.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BINARY: &str = env!("CARGO_BIN_EXE_porecatu-relay");
const EXAMPLE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/docs/reference/relay.example.toml"
);

/// A relay process that is killed when the guard is dropped, whatever the
/// test does. Only the process this test spawned is ever killed.
struct Relay(Child);

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A loopback port nobody listens on right now.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn http_get(addr: SocketAddr, path: &str) -> std::io::Result<String> {
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: relay\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

#[test]
fn the_binary_starts_with_the_example_file_and_answers_healthz() {
    let port = free_port();
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let child = Command::new(BINARY)
        .arg("--config")
        .arg(EXAMPLE)
        // The reference file binds to 8080; the environment moves it to a free
        // port (RF-1.3), which is exactly what a container does.
        .env("PORECATU_RELAY_SERVER_BIND", addr.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the binary starts");
    let mut relay = Relay(child);

    let deadline = Instant::now() + Duration::from_secs(15);
    let response = loop {
        if let Some(status) = relay.0.try_wait().unwrap() {
            panic!("the relay exited on its own: {status}");
        }
        match http_get(addr, "/healthz") {
            Ok(response) if response.starts_with("HTTP/1.1 200") => break response,
            _ => {
                assert!(Instant::now() < deadline, "no answer on {addr}");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    };
    assert!(response.ends_with("ok"), "{response:?}");
}

/// Runs the binary to the end and returns its exit code and `stderr`.
fn run_to_end(args: &[&str], envs: &[(&str, &str)]) -> (Option<i32>, String) {
    let output = Command::new(BINARY)
        .args(args)
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .output()
        .expect("the binary runs");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn a_bad_configuration_exits_with_2_and_names_the_key() {
    let (code, stderr) = run_to_end(
        &["--config", EXAMPLE],
        &[("PORECATU_RELAY_LIMITS_MAX_FRAME_BYTES", "1")],
    );
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("limits.max_frame_bytes"), "{stderr}");
}

#[test]
fn a_missing_configuration_file_exits_with_2() {
    let missing: PathBuf = std::env::temp_dir().join("porecatu-relay-no-such-file.toml");
    let (code, stderr) = run_to_end(&["--config", missing.to_str().unwrap()], &[]);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("no-such-file.toml"), "{stderr}");
}

#[test]
fn a_tls_section_with_unreadable_files_exits_with_2_and_names_the_keys() {
    let dir = std::env::temp_dir().join(format!("porecatu-relay-smoke-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("relay.toml");
    std::fs::write(
        &path,
        "[server]\nbind = \"127.0.0.1:0\"\n[tls]\ncert_file = \"c.pem\"\nkey_file = \"k.pem\"\n\
         [[tokens]]\nname = \"a\"\nvalue = \"token-de-desenvolvimento-nao-use-em-producao\"\n",
    )
    .unwrap();
    let (code, stderr) = run_to_end(&["--config", path.to_str().unwrap()], &[]);
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("tls.cert_file"), "{stderr}");
    assert!(stderr.contains("tls.key_file"), "{stderr}");
    assert!(stderr.contains("c.pem"), "{stderr}");
}

#[test]
fn a_port_in_use_exits_with_1_and_names_the_key() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap();
    let (code, stderr) = run_to_end(
        &["--config", EXAMPLE],
        &[("PORECATU_RELAY_SERVER_BIND", &addr.to_string())],
    );
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("server.bind"), "{stderr}");
}
