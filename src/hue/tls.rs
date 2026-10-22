use anyhow::{anyhow, bail, Context, Result};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::ssl::{SslConnector, SslMethod, SslStream, SslVerifyMode};
use openssl::x509::store::{X509Store, X509StoreBuilder};
use openssl::x509::X509;
use std::net::{IpAddr, TcpStream};
use std::time::{Duration, Instant};

/// A saved identity accepts CA-signed certificate renewal; a legacy pin bootstraps that identity.
#[derive(Clone, Copy, Default)]
pub struct BridgeTrust<'a> {
    pub bridge_id: Option<&'a str>,
    pub certificate_sha256: Option<&'a str>,
}

impl BridgeTrust<'_> {
    pub fn known(self) -> bool {
        self.bridge_id.is_some() || self.certificate_sha256.is_some()
    }
}

pub fn connect_bridge_tls(bridge_ip: &str, trust: BridgeTrust<'_>) -> Result<SslStream<TcpStream>> {
    let ip: IpAddr = bridge_ip
        .parse()
        .context("Hue Bridge address must be an IP literal")?;
    let tcp = match TcpStream::connect_timeout(&(ip, 443).into(), Duration::from_secs(5)) {
        Ok(tcp) => tcp,
        Err(error) => {
            let Some(id) = trust.bridge_id else {
                return Err(error).context("Failed to connect to Hue Bridge HTTPS endpoint");
            };
            // Rediscovery is only for an unreachable address, never a TLS or authorization failure.
            let discovered = super::discovery::discover_bridge_for(Some(id))?;
            let address: IpAddr = discovered
                .parse()
                .context("Invalid discovered Hue address")?;
            TcpStream::connect_timeout(&(address, 443).into(), Duration::from_secs(5))
                .context("Failed to connect to the rediscovered Hue Bridge")?
        }
    };
    tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(5)))?;
    let _deadline = super::SocketDeadline::new(&tcp, Instant::now() + Duration::from_secs(10))?;
    let mut builder = SslConnector::builder(SslMethod::tls())?;
    builder.set_cert_store(bridge_roots()?);
    builder.set_verify(SslVerifyMode::PEER);
    let mut configuration = builder.build().configure()?;
    // Hue certificates identify the bridge by CN, not its DHCP address; checked below.
    configuration.set_verify_hostname(false);
    let stream = configuration.connect(bridge_ip, tcp)
        .context("Hue Bridge TLS certificate validation failed; update bridge firmware or check its identity")?;
    let id = peer_bridge_id(&stream)?;
    let fingerprint = peer_certificate_sha256(&stream)?;
    check_identity(&id, &fingerprint, trust)?;
    Ok(stream)
}

fn bridge_roots() -> Result<X509Store> {
    let mut roots = X509StoreBuilder::new()?;
    for ca in X509::stack_from_pem(include_bytes!("bridge-ca.pem"))? {
        roots.add_cert(ca)?;
    }
    Ok(roots.build())
}

fn check_identity(id: &str, fingerprint: &str, trust: BridgeTrust<'_>) -> Result<()> {
    if id.len() != 16 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("Hue Bridge certificate has an invalid bridge ID");
    }
    if let Some(expected) = trust.bridge_id {
        if !expected.eq_ignore_ascii_case(id) {
            bail!("Hue Bridge identity changed; pair the replacement bridge explicitly");
        }
    } else if let Some(expected) = trust.certificate_sha256 {
        if !expected.eq_ignore_ascii_case(fingerprint) {
            bail!("Hue Bridge certificate fingerprint changed before identity migration; re-pair explicitly");
        }
    }
    Ok(())
}

pub fn peer_bridge_id(stream: &SslStream<TcpStream>) -> Result<String> {
    let certificate = stream
        .ssl()
        .peer_certificate()
        .ok_or_else(|| anyhow!("Hue Bridge did not present a TLS certificate"))?;
    let mut names = certificate.subject_name().entries_by_nid(Nid::COMMONNAME);
    let id = names
        .next()
        .ok_or_else(|| anyhow!("Hue Bridge certificate has no bridge ID"))?
        .data()
        .to_string()?
        .to_ascii_lowercase();
    if names.next().is_some() || id.len() != 16 || !id.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        bail!("Hue Bridge certificate has an invalid bridge ID");
    }
    Ok(id)
}

pub fn peer_certificate_sha256(stream: &SslStream<TcpStream>) -> Result<String> {
    let certificate = stream
        .ssl()
        .peer_certificate()
        .ok_or_else(|| anyhow!("Hue Bridge did not present a TLS certificate"))?;
    Ok(certificate
        .digest(MessageDigest::sha256())?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_requires_matching_identity_or_legacy_pin_and_embeds_both_roots() {
        let roots = X509::stack_from_pem(include_bytes!("bridge-ca.pem")).unwrap();
        assert_eq!(roots.len(), 2);
        for root in roots {
            assert!(root.verify(&root.public_key().unwrap()).unwrap());
        }
        let group = openssl::ec::EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        let key = openssl::pkey::PKey::from_ec_key(openssl::ec::EcKey::generate(&group).unwrap())
            .unwrap();
        let mut name = openssl::x509::X509Name::builder().unwrap();
        name.append_entry_by_nid(Nid::COMMONNAME, "001788fffe000001")
            .unwrap();
        let name = name.build();
        let mut untrusted = X509::builder().unwrap();
        untrusted.set_version(2).unwrap();
        untrusted.set_subject_name(&name).unwrap();
        untrusted.set_issuer_name(&name).unwrap();
        untrusted.set_pubkey(&key).unwrap();
        untrusted
            .set_not_before(&openssl::asn1::Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        untrusted
            .set_not_after(&openssl::asn1::Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        untrusted.sign(&key, MessageDigest::sha256()).unwrap();
        let mut context = openssl::x509::X509StoreContext::new().unwrap();
        assert!(!context
            .init(
                &bridge_roots().unwrap(),
                &untrusted.build(),
                &openssl::stack::Stack::new().unwrap(),
                |context| context.verify_cert()
            )
            .unwrap());
        let id = "001788fffe000001";
        let pin = "a".repeat(64);
        assert!(check_identity(
            id,
            &pin,
            BridgeTrust {
                bridge_id: None,
                certificate_sha256: Some(&pin)
            }
        )
        .is_ok());
        assert!(check_identity(
            id,
            "new",
            BridgeTrust {
                bridge_id: Some(id),
                certificate_sha256: Some(&pin)
            }
        )
        .is_ok());
        assert!(check_identity(
            id,
            "new",
            BridgeTrust {
                bridge_id: None,
                certificate_sha256: Some(&pin)
            }
        )
        .is_err());
        assert!(check_identity(
            id,
            &pin,
            BridgeTrust {
                bridge_id: Some("001788fffe000002"),
                certificate_sha256: Some(&pin)
            }
        )
        .is_err());
        assert!(check_identity("not-a-bridge-id!!", &pin, BridgeTrust::default()).is_err());
    }
}
