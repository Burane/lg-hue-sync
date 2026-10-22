use anyhow::{anyhow, ensure, Context, Result};
use serde_json::Value;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};

const MAX_REPLY: u64 = 64 * 1024;

pub async fn query(uri: &str, payload: &str) -> Result<Value> {
    query_program(
        Path::new("/usr/bin/luna-send"),
        uri,
        payload,
        Duration::from_millis(1500),
    )
    .await
}

async fn query_program(
    program: &Path,
    uri: &str,
    payload: &str,
    deadline: Duration,
) -> Result<Value> {
    let mut child = Command::new(program)
        .kill_on_drop(true)
        .args(["-n", "1", "-f", "-w", "1000", uri, payload])
        // This firmware exits at stdin EOF before its asynchronous reply arrives.
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("could not start webOS query")?;
    let result = timeout(deadline, async {
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .context("webOS query output unavailable")?
            .take(MAX_REPLY + 1)
            .read_to_end(&mut bytes)
            .await
            .context("could not read webOS query")?;
        ensure!(
            bytes.len() as u64 <= MAX_REPLY,
            "webOS query response too large"
        );
        ensure!(
            child
                .wait()
                .await
                .context("could not wait for webOS query")?
                .success(),
            "webOS query process failed"
        );
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow!("webOS query returned invalid JSON"))?;
        ensure!(
            value.is_object() && value["returnValue"] == true,
            "webOS query refused or missing acknowledgement"
        );
        Ok(value)
    })
    .await
    .unwrap_or_else(|_| Err(anyhow!("webOS query timed out")));
    if result.is_err() {
        let _ = child.kill().await;
    }
    result
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[tokio::test]
    async fn query_holds_stdin_open_bounds_replies_and_reaps_timed_out_children() {
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let fixture = Fixture(std::env::temp_dir().join(format!(
                "lg-luna-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )));
        fs::create_dir(&fixture.0).unwrap();
        let program = fixture.0.join("luna-send");
        fs::write(
            &program,
            r#"#!/bin/sh
[ -p /dev/stdin ] || exit 98
[ "$6" = 'luna://com.webos.applicationManager/getForegroundAppInfo' ] && [ "$7" = '{}' ] || exit 99
case "$(cat "$0.mode")" in
good) printf '{"returnValue":true,"appId":"com.webos.app.hdmi2"}' ;;
failed) exit 2 ;;
malformed) printf 'private-invalid-payload' ;;
refused) printf '{"returnValue":false,"errorText":"private-error"}' ;;
unacknowledged) printf '{"appId":"com.webos.app.hdmi2"}' ;;
large) dd if=/dev/zero bs=65537 count=1 2>/dev/null ;;
wait) echo $$ > "$0.pid"; exec sleep 30 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        for mode in [
            "good",
            "failed",
            "malformed",
            "refused",
            "unacknowledged",
            "large",
            "wait",
        ] {
            fs::write(fixture.0.join("luna-send.mode"), mode).unwrap();
            let reply = query_program(
                &program,
                "luna://com.webos.applicationManager/getForegroundAppInfo",
                "{}",
                Duration::from_millis(500),
            )
            .await;
            if mode == "good" {
                assert_eq!(reply.unwrap()["appId"], "com.webos.app.hdmi2");
            } else {
                let error = reply.unwrap_err().to_string();
                assert!(!error.contains("private"));
            }
        }
        let pid = fs::read_to_string(fixture.0.join("luna-send.pid")).unwrap();
        assert!(!std::process::Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success());
        assert!(query_program(
            &fixture.0.join("missing"),
            "luna://unused",
            "{}",
            Duration::from_millis(500)
        )
        .await
        .is_err());
    }
}
