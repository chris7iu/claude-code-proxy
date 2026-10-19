use assert_cmd::Command;
use predicates::str::contains;
use std::env;
#[cfg(unix)]
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

#[test]
fn version_aliases_print_expected_version() -> Result<(), Box<dyn std::error::Error>> {
    let expected = format!("claude-code-proxy {}", env!("CARGO_PKG_VERSION"));

    for arg in ["--version", "-v", "version"] {
        let mut cmd = Command::cargo_bin("claude-code-proxy")?;
        cmd.arg(arg)
            .assert()
            .success()
            .stdout(contains(expected.clone()));
    }
    Ok(())
}

#[test]
fn models_prints_all_providers() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.arg("models");
    let out = String::from_utf8(cmd.output()?.stdout)?;
    assert!(out.contains("codex:"));
    assert!(out.contains("kimi:"));
    assert!(out.contains("opencode:"));
    assert!(out.contains("cursor:"));

    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.args(["models", "--full"]);
    cmd.output()?;
    Ok(())
}

#[test]
fn help_describes_visible_commands_and_hides_demo() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.arg("--help");
    let output = cmd.output()?;
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout)?;
    for description in [
        "Print version information",
        "Start the proxy server and monitor",
        "List supported provider models",
        "Manage Codex authentication",
        "Manage Kimi authentication",
        "Manage Cursor authentication",
        "Manage Grok authentication",
    ] {
        assert!(stdout.contains(description), "missing: {description}");
    }
    assert!(!stdout.contains("demo"));
    assert!(!stdout.contains("mock data and no proxy server"));
    Ok(())
}

#[test]
fn invalid_command_exits_two() -> Result<(), Box<dyn std::error::Error>> {
    Command::cargo_bin("claude-code-proxy")?
        .arg("definitely-not-a-command")
        .assert()
        .failure()
        .code(2);
    Ok(())
}

#[test]
fn unsupported_provider_auth_command_exits_two() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.args(["cursor", "auth", "device"]);
    let output = cmd.output()?;
    assert_eq!(output.status.code(), Some(2));
    let out = String::from_utf8(output.stderr)?;
    assert!(out.contains("not yet implemented") || out.contains("unsupported"));
    Ok(())
}

#[test]
fn provider_logout_without_auth_is_success() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.args(["kimi", "auth", "logout"]);
    cmd.env("CCP_CONFIG_DIR", temp.path());
    cmd.assert().success();
    Ok(())
}

#[test]
fn antigravity_user_agent_status_is_bounded_and_logout_ignores_invalid_transport() {
    for value in ["sentinel-native-UA", "sentinel-invalid\tUA", ""] {
        let directory = TempDir::new().unwrap();
        let output = Command::cargo_bin("claude-code-proxy")
            .unwrap()
            .args(["antigravity", "auth", "status"])
            .env("CCP_CONFIG_DIR", directory.path())
            .env("CCP_ANTIGRAVITY_USER_AGENT", value)
            .output()
            .unwrap();
        assert!(output.status.success());
        let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            status["status"]["configuration"],
            if value == "sentinel-native-UA" {
                "valid"
            } else {
                "invalid"
            }
        );
        assert_eq!(status["enabled"], false);
        assert_eq!(status["status"]["live_verified"], false);
        assert_eq!(status["backend_window_verified"], false);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("sentinel"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("sentinel"));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let models = Command::cargo_bin("claude-code-proxy")
            .unwrap()
            .args(["models", "--full"])
            .env("CCP_CONFIG_DIR", directory.path())
            .env("CCP_ANTIGRAVITY_USER_AGENT", value)
            .output()
            .unwrap();
        assert!(models.status.success());
        let models = String::from_utf8(models.stdout).unwrap();
        assert!(!models.contains("antigravity/gemini"));
        assert!(models.contains("codex:"));
        let auth_dir = directory.path().join("antigravity");
        std::fs::create_dir(&auth_dir).unwrap();
        std::fs::write(auth_dir.join("auth.json"), b"synthetic local credential").unwrap();
        std::fs::write(directory.path().join("other-provider-state"), b"untouched").unwrap();
        Command::cargo_bin("claude-code-proxy")
            .unwrap()
            .args(["antigravity", "auth", "logout"])
            .env("CCP_CONFIG_DIR", directory.path())
            .env("CCP_ANTIGRAVITY_USER_AGENT", value)
            .assert()
            .success();
        assert!(!directory.path().join("antigravity/auth.json").exists());
        assert_eq!(
            std::fs::read(directory.path().join("other-provider-state")).unwrap(),
            b"untouched"
        );
    }
}

#[cfg(unix)]
#[test]
fn antigravity_user_agent_non_utf8_environment_fails_closed() {
    use std::os::unix::ffi::OsStringExt;
    for enabled in ["0", "1"] {
        let directory = TempDir::new().unwrap();
        let output = Command::cargo_bin("claude-code-proxy")
            .unwrap()
            .args(["antigravity", "auth", "status"])
            .env("CCP_CONFIG_DIR", directory.path())
            .env("CCP_ANTIGRAVITY_ENABLED", enabled)
            .env(
                "CCP_ANTIGRAVITY_USER_AGENT",
                std::ffi::OsString::from_vec(b"sentinel-non-utf8-\xff".to_vec()),
            )
            .output()
            .unwrap();
        assert!(output.status.success());
        let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(status["status"]["configuration"], "invalid");
        assert_eq!(status["status"]["configuration_ready"], false);
        assert_eq!(status["enabled"], false);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("sentinel"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("sentinel"));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}

#[cfg(unix)]
#[test]
fn antigravity_user_agent_non_utf8_logout_still_deletes_only_local_credentials() {
    use std::os::unix::ffi::OsStringExt;
    let directory = TempDir::new().unwrap();
    let auth_dir = directory.path().join("antigravity");
    std::fs::create_dir(&auth_dir).unwrap();
    std::fs::write(auth_dir.join("auth.json"), b"synthetic-local-credential").unwrap();
    std::fs::write(directory.path().join("unrelated-state"), b"untouched").unwrap();
    let output = Command::cargo_bin("claude-code-proxy")
        .unwrap()
        .args(["antigravity", "auth", "logout"])
        .env("CCP_CONFIG_DIR", directory.path())
        .env(
            "CCP_ANTIGRAVITY_USER_AGENT",
            std::ffi::OsString::from_vec(b"sentinel-non-utf8-\xff".to_vec()),
        )
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(!auth_dir.join("auth.json").exists());
    assert_eq!(
        std::fs::read(directory.path().join("unrelated-state")).unwrap(),
        b"untouched"
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("sentinel"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sentinel"));
}

#[test]
fn models_output_is_stable_order() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.args(["models", "--full"]);
    let output = cmd.output()?;
    let out = String::from_utf8(output.stdout)?;
    let codex_pos = out.find("codex:").unwrap_or(0);
    let kimi_pos = out.find("kimi:").unwrap_or(0);
    let cursor_pos = out.find("cursor:").unwrap_or(0);
    assert!(codex_pos < kimi_pos);
    assert!(kimi_pos < cursor_pos);
    Ok(())
}

#[cfg(unix)]
struct ChildGuard(Child);

#[cfg(unix)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(unix)]
fn wait_for_service(
    child: &mut ChildGuard,
    port: u16,
) -> Result<TcpStream, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() < deadline => {
                if let Some(status) = child.0.try_wait()? {
                    let mut stderr = String::new();
                    if let Some(mut pipe) = child.0.stderr.take() {
                        pipe.read_to_string(&mut stderr)?;
                    }
                    return Err(format!("service exited with {status}: {stderr}").into());
                }
                let _ = error;
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(unix)]
fn send_signal(child: &ChildGuard, signal: &str) -> Result<(), Box<dyn std::error::Error>> {
    let status = std::process::Command::new("kill")
        .args([signal, &child.0.id().to_string()])
        .status()?;
    if !status.success() {
        return Err(format!("kill {signal} failed with {status}").into());
    }
    Ok(())
}

#[cfg(unix)]
fn wait_for_exit(
    child: &mut ChildGuard,
    timeout: Duration,
) -> Result<ExitStatus, Box<dyn std::error::Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.0.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err("plain service did not exit after the second signal".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn antigravity_user_agent_invalid_environment_is_scoped_to_native_provider()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::{ffi::OsStringExt, fs::PermissionsExt};
    for enabled in ["0", "1"] {
        for invalid in [
            b"sentinel-invalid\tUA".to_vec(),
            b"sentinel-invalid-\xff".to_vec(),
        ] {
            let directory = TempDir::new()?;
            let upstream = TcpListener::bind("127.0.0.1:0")?;
            upstream.set_nonblocking(true)?;
            let config = serde_json::json!({"antigravity": {
                "userAgent": "file-native-fallback",
                "baseUrl": format!("https://{}", upstream.local_addr()?)
            }});
            std::fs::write(directory.path().join("config.json"), config.to_string())?;
            let auth_dir = directory.path().join("antigravity");
            std::fs::create_dir(&auth_dir)?;
            std::fs::set_permissions(&auth_dir, std::fs::Permissions::from_mode(0o700))?;
            let auth_path = auth_dir.join("auth.json");
            std::fs::write(&auth_path, serde_json::json!({
                "profile_id":"synthetic-profile", "project_id":"synthetic-project",
                "access_token":"synthetic-access", "refresh_token":null, "expires_at":4102444800u64
            }).to_string())?;
            std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600))?;
            let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
            let child = std::process::Command::new(env!("CARGO_BIN_EXE_claude-code-proxy"))
                .args(["serve", "--no-monitor", "--port", &port.to_string()])
                .env("CCP_CONFIG_DIR", directory.path())
                .env("XDG_STATE_HOME", directory.path().join("state"))
                .env("CCP_ANTIGRAVITY_ENABLED", enabled)
                .env(
                    "CCP_ANTIGRAVITY_USER_AGENT",
                    std::ffi::OsString::from_vec(invalid),
                )
                .env("CCP_CURSOR_CLIENT_VERSION", "synthetic-version")
                .env("CCP_LOG_STDERR", "1")
                .env("CCP_LOG_VERBOSE", "1")
                .env("CCP_TRAFFIC_LOG", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()?;
            let mut child = ChildGuard(child);
            drop(wait_for_service(&mut child, port)?);
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()?;
            let response = client
                .get(format!(
                    "http://127.0.0.1:{port}/v1/models?include_capabilities=true"
                ))
                .send()
                .await?;
            assert_eq!(response.status(), 200);
            let models: serde_json::Value = response.json().await?;
            assert!(!models["data"].as_array().unwrap().iter().any(|entry| {
                entry["id"]
                    .as_str()
                    .is_some_and(|name| name.starts_with("antigravity/"))
            }));
            let capability = &models["capabilities"]["providers"]["antigravity"];
            assert_eq!(capability["status"]["configuration"], "invalid");
            assert_eq!(capability["status"]["configuration_ready"], false);
            assert_eq!(capability["enabled"], false);
            // A network-free other-provider operation must reach traffic setup,
            // not merely the early rejection of an unavailable native model.
            let response = client
                .post(format!("http://127.0.0.1:{port}/v1/messages/count_tokens"))
                .json(&serde_json::json!({
                    "model": "kimi-for-coding",
                    "messages": [{"role": "user", "content": "synthetic"}]
                }))
                .send()
                .await?;
            assert_eq!(response.status(), 200);
            let count: serde_json::Value = response.json().await?;
            assert!(
                count["input_tokens"]
                    .as_u64()
                    .is_some_and(|count| count > 0)
            );
            assert!(
                directory
                    .path()
                    .join("state/claude-code-proxy/traffic")
                    .exists()
            );
            let response = client.post(format!("http://127.0.0.1:{port}/v1/messages"))
                .json(&serde_json::json!({"model":"antigravity/gemini-4-argon-low", "max_tokens":32, "messages":[{"role":"user","content":"synthetic"}]}))
                .send().await?;
            assert!(response.status().is_client_error());
            assert!(!response.text().await?.contains("sentinel"));
            assert!(
                matches!(upstream.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
            );
            send_signal(&child, "-TERM")?;
            assert!(wait_for_exit(&mut child, Duration::from_secs(5))?.success());
            let mut stderr = String::new();
            child.0.stderr.take().unwrap().read_to_string(&mut stderr)?;
            assert!(!stderr.contains("sentinel"));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn plain_service_exits_on_second_signal(signal: &str) -> Result<(), Box<dyn std::error::Error>> {
    let upstream = TcpListener::bind("127.0.0.1:0")?;
    let upstream_url = format!("http://{}", upstream.local_addr()?);
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let fixture = thread::spawn(move || {
        let (stream, _) = upstream.accept().unwrap();
        accepted_tx.send(()).unwrap();
        let _ = release_rx.recv();
        drop(stream);
    });

    let config = TempDir::new()?;
    let auth_dir = config.path().join("kimi");
    std::fs::create_dir_all(&auth_dir)?;
    std::fs::write(
        auth_dir.join("auth.json"),
        r#"{"access":"test","refresh":"test","expires":4102444800000,"scope":"openid","userId":"test"}"#,
    )?;
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_claude-code-proxy"))
        .args(["serve", "--no-monitor", "--port", &port.to_string()])
        .env("CCP_CONFIG_DIR", config.path())
        .env("CCP_KIMI_BASE_URL", upstream_url)
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut child = ChildGuard(child);
    let mut downstream = wait_for_service(&mut child, port)?;
    let body = br#"{"model":"kimi-for-coding","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}"#;
    write!(
        downstream,
        "POST /v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )?;
    downstream.write_all(body)?;
    accepted_rx.recv_timeout(Duration::from_secs(20))?;

    send_signal(&child, signal)?;
    thread::sleep(Duration::from_millis(200));
    assert!(child.0.try_wait()?.is_none());
    send_signal(&child, signal)?;
    let status = wait_for_exit(&mut child, Duration::from_secs(2));
    let _ = release_tx.send(());
    fixture.join().unwrap();

    assert_eq!(status?.code(), Some(130));
    Ok(())
}

#[cfg(unix)]
#[test]
fn plain_service_exits_on_second_ctrl_c() -> Result<(), Box<dyn std::error::Error>> {
    plain_service_exits_on_second_signal("-INT")
}

#[cfg(unix)]
#[test]
fn plain_service_exits_on_second_sigterm() -> Result<(), Box<dyn std::error::Error>> {
    plain_service_exits_on_second_signal("-TERM")
}

#[test]
fn kimi_auth_status_reads_stored_auth() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let auth_dir = temp.path().join("kimi");
    std::fs::create_dir_all(&auth_dir)?;
    std::fs::write(
        auth_dir.join("auth.json"),
        r#"{"access":"a","refresh":"r","expires":4102444800000,"scope":"openid","userId":"u"}"#,
    )?;
    let mut cmd = Command::cargo_bin("claude-code-proxy")?;
    cmd.args(["kimi", "auth", "status"]);
    cmd.env("CCP_CONFIG_DIR", temp.path());
    cmd.assert().success().stdout(contains("User: u"));
    Ok(())
}
