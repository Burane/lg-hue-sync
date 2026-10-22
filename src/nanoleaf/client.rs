use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::Value;
use std::time::{Duration, Instant};
use tracing::info;

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PanelPosition {
    #[serde(rename = "panelId")]
    pub panel_id: u16,
    pub x: i32,
    pub y: i32,
    pub o: Option<i32>,
    #[serde(rename = "shapeType")]
    pub shape_type: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PanelLayout {
    #[serde(rename = "numPanels")]
    pub num_panels: u16,
    #[serde(rename = "sideLength")]
    pub side_length: Option<u32>,
    #[serde(rename = "positionData", default)]
    pub position_data: Vec<PanelPosition>,
}

/// Pairs with a Nanoleaf controller on the local network.
/// The user must hold the power button for 5-7 seconds until the LED starts flashing.
pub fn pair_nanoleaf(ip: &str, timeout_secs: u64) -> Result<(String, u16, Vec<u16>)> {
    println!("\n========================================================");
    println!(">>> ACTION REQUIRED FOR NANOLEAF 4D:");
    println!(">>> Press and hold the power button on your Nanoleaf controller");
    println!(">>> for 5-7 seconds until the status LED starts blinking!");
    println!("========================================================\n");

    pair_nanoleaf_at(
        &format!("http://{ip}:16021"),
        Duration::from_secs(timeout_secs),
    )
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

fn pair_nanoleaf_at(base_url: &str, timeout: Duration) -> Result<(String, u16, Vec<u16>)> {
    let url = format!("{base_url}/api/v1/new");
    let start = Instant::now();
    let mut auth_token = String::new();
    let agent = crate::local_http::agent(Some(REQUEST_TIMEOUT));

    while let Some(remaining) = timeout
        .checked_sub(start.elapsed())
        .filter(|d| !d.is_zero())
    {
        let resp = agent
            .post(&url)
            .header("Content-Type", "application/json")
            .config()
            .timeout_global(Some(remaining.min(REQUEST_TIMEOUT)))
            .build()
            .send_empty();

        match resp {
            Ok(r) => {
                if let Ok(json) = r.into_body().read_json::<Value>()
                    && let Some(token) = json.get("auth_token").and_then(|v| v.as_str())
                {
                    auth_token = token.to_string();
                    println!("\n[+] Successfully paired with Nanoleaf controller!");
                    break;
                }
            }
            Err(ureq::Error::StatusCode(403)) => {
                // Not in pairing mode yet
                print!(".");
                use std::io::Write;
                std::io::stdout().flush().ok();
            }
            Err(e) => {
                // Network glitch or connection refused
                tracing::debug!("Nanoleaf pairing poll error: {}", e);
            }
        }
        std::thread::sleep(
            timeout
                .saturating_sub(start.elapsed())
                .min(Duration::from_secs(1)),
        );
    }

    if auth_token.is_empty() {
        return Err(anyhow!(
            "Timed out waiting for Nanoleaf pairing button press"
        ));
    }

    // Retrieve panel layout to discover segments
    let remaining = timeout.saturating_sub(start.elapsed());
    if remaining.is_zero() {
        return Err(anyhow!("Timed out retrieving paired Nanoleaf panel layout"));
    }
    let layout = get_panel_layout(base_url, &auth_token, remaining.min(REQUEST_TIMEOUT))?;
    let mut panel_ids = Vec::new();
    for pos in &layout.position_data {
        panel_ids.push(pos.panel_id);
    }

    info!(
        "Discovered Nanoleaf layout: {} panels/segments, {} position data points",
        layout.num_panels,
        panel_ids.len()
    );

    Ok((auth_token, layout.num_panels, panel_ids))
}

/// Retrieves the physical panel/segment layout within the caller's request budget.
pub fn get_panel_layout(
    base_url: &str,
    auth_token: &str,
    timeout: Duration,
) -> Result<PanelLayout> {
    let url = format!("{base_url}/api/v1/{auth_token}/panelLayout/layout");
    let resp = crate::local_http::agent(Some(timeout))
        .get(&url)
        .header("Content-Type", "application/json")
        .call()
        .map_err(|error| crate::local_http::request_error("Nanoleaf panel layout", error))?;

    let layout: PanelLayout = resp
        .into_body()
        .read_json()
        .with_context(|| "Failed to parse Nanoleaf panel layout JSON")?;

    Ok(layout)
}

/// Activates external control mode (UDP v2 streaming) on the Nanoleaf device.
/// Returns the UDP port to stream to (typically 60222).
pub fn enable_external_control(ip: &str, auth_token: &str) -> Result<u16> {
    let url = format!("http://{}:16021/api/v1/{}/effects", ip, auth_token);
    let payload = serde_json::json!({
        "write": {
            "command": "display",
            "animType": "extControl",
            "extControlVersion": "v2"
        }
    });

    info!(
        "Enabling Nanoleaf external control (UDP v2) mode on {}...",
        ip
    );
    let agent = crate::local_http::agent(Some(Duration::from_secs(3)));
    let resp = agent
        .put(&url)
        .header("Content-Type", "application/json")
        .send_json(payload)
        .map_err(|error| crate::local_http::request_error("Nanoleaf external control", error))?;

    let default_port = 60222u16;
    // Some firmwares return UDP details in the response body or headers
    if let Ok(val) = resp.into_body().read_json::<Value>()
        && let Some(port) = val.get("streamControlPort").and_then(|v| v.as_u64())
    {
        info!("Nanoleaf specified streamControlPort: {}", port);
        return Ok(port as u16);
    }

    Ok(default_port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    fn stalled_server(
        responses: Vec<&'static [u8]>,
    ) -> (String, mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (release, wait) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let count = responses.len();
            for (index, response) in responses.into_iter().enumerate() {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 2048];
                assert!(socket.read(&mut request).unwrap() > 0);
                socket.write_all(response).unwrap();
                if index + 1 == count {
                    let _ = wait.recv_timeout(Duration::from_secs(2));
                }
            }
        });
        (url, release, server)
    }

    #[test]
    fn pairing_deadline_covers_stalled_headers_body_and_poll_sleep() {
        for response in [
            &b""[..],
            &b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{"[..],
            &b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n"[..],
        ] {
            let (url, release, server) = stalled_server(vec![response]);
            let start = Instant::now();
            let result = pair_nanoleaf_at(&url, Duration::from_millis(150));
            let elapsed = start.elapsed();
            release.send(()).unwrap();
            server.join().unwrap();
            assert!(result.is_err());
            assert!(elapsed < Duration::from_millis(700), "elapsed: {elapsed:?}");
        }
    }

    #[test]
    fn layout_body_read_obeys_request_and_remaining_pairing_deadlines() {
        const LAYOUT: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{";
        const PAIRED: &[u8] = b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 21\r\n\r\n{\"auth_token\":\"test\"}";
        for pairing in [false, true] {
            let responses = if pairing {
                vec![PAIRED, LAYOUT]
            } else {
                vec![LAYOUT]
            };
            let (url, release, server) = stalled_server(responses);
            let start = Instant::now();
            let timeout = Duration::from_millis(150);
            let result = if pairing {
                pair_nanoleaf_at(&url, timeout).map(|_| ())
            } else {
                get_panel_layout(&url, "test", timeout).map(|_| ())
            };
            let elapsed = start.elapsed();
            release.send(()).unwrap();
            server.join().unwrap();
            assert!(result.is_err());
            assert!(elapsed < Duration::from_millis(700), "elapsed: {elapsed:?}");
        }
    }
}
