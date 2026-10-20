use anyhow::{anyhow, Context, Result};
use openssl::ssl::{SslConnector, SslMethod, SslStream, SslVerifyMode};
use std::io::{Read, Write};
use std::net::{IpAddr, UdpSocket};
use std::time::{Duration, Instant};
use tracing::info;

#[derive(Debug)]
pub struct ConnectedUdpSocket {
    socket: UdpSocket,
    deadline: Option<Instant>,
}

impl ConnectedUdpSocket {
    fn timeout(&self) -> std::io::Result<Duration> {
        match self.deadline {
            Some(deadline) => deadline
                .checked_duration_since(Instant::now())
                .filter(|remaining| !remaining.is_zero())
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "DTLS handshake deadline expired",
                    )
                }),
            None => Ok(Duration::from_secs(5)),
        }
    }
}

impl Read for ConnectedUdpSocket {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.deadline.is_some() {
            self.socket.set_read_timeout(Some(self.timeout()?))?;
        }
        self.socket.recv(buf)
    }
}

impl Write for ConnectedUdpSocket {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.deadline.is_some() {
            self.socket.set_write_timeout(Some(self.timeout()?))?;
        }
        self.socket.send(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct HueDtlsClient {
    stream: SslStream<ConnectedUdpSocket>,
}

impl HueDtlsClient {
    /// Connect to Hue Bridge DTLS port 2100 with PSK identity and client key
    pub fn connect(bridge_ip: &str, username: &str, clientkey_hex: &str) -> Result<Self> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let psk_bytes = hex_to_bytes(clientkey_hex).with_context(|| {
            "Failed to parse clientkey as hex (must be 32 hex chars / 16 bytes)"
        })?;

        info!(
            "Configuring DTLS 1.2 PSK connector for {}:2100...",
            bridge_ip
        );

        let mut builder = SslConnector::builder(SslMethod::dtls())
            .with_context(|| "Failed to create DTLS SSL connector builder")?;

        // Allow PSK ciphers supported by Hue Bridge
        builder
            .set_cipher_list("PSK-AES128-GCM-SHA256:PSK-AES256-GCM-SHA384")
            .with_context(|| "Failed to set PSK cipher list")?;

        // Disable server certificate verification since Hue uses pre-shared keys
        builder.set_verify(SslVerifyMode::NONE);

        let identity = username.to_string();
        builder.set_psk_client_callback(move |_ssl, _hint, identity_buf, psk_buf| {
            let id_bytes = identity.as_bytes();
            if id_bytes.len() > identity_buf.len() {
                return Err(openssl::error::ErrorStack::get());
            }
            identity_buf[..id_bytes.len()].copy_from_slice(id_bytes);

            if psk_bytes.len() > psk_buf.len() {
                return Err(openssl::error::ErrorStack::get());
            }
            psk_buf[..psk_bytes.len()].copy_from_slice(&psk_bytes);

            Ok(psk_bytes.len())
        });

        let connector = builder.build();

        // Bind local UDP socket and connect to Hue Bridge:2100
        let socket =
            UdpSocket::bind("0.0.0.0:0").with_context(|| "Failed to bind local UDP socket")?;
        let target_addr = format!("{}:2100", bridge_ip);
        let target_ip: IpAddr = bridge_ip
            .parse()
            .context("Hue Bridge address must be an IP literal")?;
        socket
            .connect((target_ip, 2100))
            .with_context(|| format!("Failed to connect UDP socket to {}", target_addr))?;
        socket
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .with_context(|| "Failed to set UDP socket read timeout")?;
        socket
            .set_write_timeout(Some(std::time::Duration::from_secs(5)))
            .with_context(|| "Failed to set UDP socket write timeout")?;

        let mut last_err = None;
        for attempt in 1..=4 {
            if Instant::now() >= deadline {
                break;
            }
            let socket_clone = socket
                .try_clone()
                .with_context(|| "Failed to clone UDP socket for DTLS retry")?;
            info!(
                "Performing DTLS handshake with {} (attempt {}/4)...",
                target_addr, attempt
            );
            match connector.connect(
                "HueBridge",
                ConnectedUdpSocket {
                    socket: socket_clone,
                    deadline: Some(deadline),
                },
            ) {
                Ok(mut stream) => {
                    if Instant::now() >= deadline {
                        return Err(anyhow!("DTLS handshake deadline expired"));
                    }
                    stream.get_mut().deadline = None;
                    stream
                        .get_ref()
                        .socket
                        .set_read_timeout(Some(Duration::from_secs(5)))?;
                    stream
                        .get_ref()
                        .socket
                        .set_write_timeout(Some(Duration::from_secs(5)))?;
                    info!("[+] DTLS 1.2 handshake successful with Hue Bridge!");
                    return Ok(Self { stream });
                }
                Err(e) => {
                    let err_str = format!("{:?}", e);
                    tracing::warn!("DTLS handshake attempt {} failed: {}", attempt, err_str);
                    last_err = Some(e);
                    std::thread::sleep(
                        deadline
                            .saturating_duration_since(Instant::now())
                            .min(Duration::from_millis(250)),
                    );
                }
            }
        }

        Err(anyhow!(
            "DTLS handshake failed within retry/deadline limits: {:?}",
            last_err
        ))
    }

    /// Send a binary HueStream packet over the encrypted DTLS channel
    pub fn send(&mut self, packet: &[u8]) -> Result<()> {
        self.stream.write_all(packet)?;
        self.stream.flush()?;
        Ok(())
    }
}

fn hex_to_bytes(hex: &str) -> Result<[u8; 16]> {
    let hex = hex.trim();
    if hex.len() != 32 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(anyhow!(
            "Client key must contain exactly 32 ASCII hex characters"
        ));
    }
    let mut key = [0; 16];
    // ASCII validation above makes every pair a valid UTF-8 boundary.
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)?;
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_keys_require_exact_ascii_hex_without_panicking() {
        for invalid in [
            "",
            "aéa",
            "éééééééééééééééé",
            "00",
            "0000000000000000000000000000000g",
        ] {
            assert!(hex_to_bytes(invalid).is_err());
        }
        assert_eq!(
            hex_to_bytes("00112233445566778899aAbBcCdDeEfF").unwrap(),
            [0, 17, 34, 51, 68, 85, 102, 119, 136, 153, 170, 187, 204, 221, 238, 255]
        );
        assert!(HueDtlsClient::connect("127.0.0.1", "user", "aéa").is_err());
    }

    #[test]
    fn handshake_deadline_bounds_reads_and_expired_writes() {
        let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.connect(peer.local_addr().unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_millis(30);
        let mut transport = ConnectedUdpSocket {
            socket,
            deadline: Some(deadline),
        };
        let error = transport.read(&mut [0; 1]).unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        ));
        assert!(Instant::now().duration_since(deadline) < Duration::from_secs(1));
        transport.deadline = Some(Instant::now() - Duration::from_millis(1));
        assert_eq!(
            transport.write(&[0]).unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
    }
}
