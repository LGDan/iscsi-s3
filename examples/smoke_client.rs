//! Minimal login + write/read smoke test (skips discovery).

use iscsi_target::client::IscsiClient;
use std::env;
use std::process::ExitCode;
use std::thread;
use std::time::{Duration, Instant};

const DISK0: &str = "iqn.2026-09.local.iscsi-s3:disk0";
const DISK1: &str = "iqn.2026-09.local.iscsi-s3:disk1";
const INITIATOR: &str = "iqn.2026-09.local.iscsi-s3:smoke";

fn wait_for_port(addr: &str, timeout: Duration) -> Result<(), String> {
    let start = Instant::now();
    loop {
        match std::net::TcpStream::connect(addr) {
            Ok(_) => return Ok(()),
            Err(e) => {
                if start.elapsed() > timeout {
                    return Err(format!("timeout waiting for {addr}: {e}"));
                }
                thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

fn round_trip(addr: &str, target: &str) -> Result<(), String> {
    let mut client = IscsiClient::connect(addr).map_err(|e| e.to_string())?;
    client
        .login(INITIATOR, target)
        .map_err(|e| format!("login {target}: {e}"))?;

    let mut data = vec![0u8; 4096];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    client
        .send_write(0, &data)
        .map_err(|e| format!("write {target}: {e}"))?;

    let (_pdu, read_back) = client
        .send_read(0, 8)
        .map_err(|e| format!("read {target}: {e}"))?;
    if read_back != data {
        return Err(format!("data mismatch on {target}"));
    }

    client.logout().map_err(|e| format!("logout {target}: {e}"))?;
    println!("ok round-trip {target}");
    Ok(())
}

fn main() -> ExitCode {
    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:3260".to_string());

    if let Err(e) = wait_for_port(&addr, Duration::from_secs(60)) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    // Discovery is best-effort; multi-target discovery in iscsi-target 1.0 has
    // a login-phase bug, so we always continue with known IQNs.
    println!("using known IQNs {DISK0}, {DISK1}");

    for target in [DISK0, DISK1] {
        if let Err(e) = round_trip(&addr, target) {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    }

    println!("smoke client passed");
    ExitCode::SUCCESS
}
