use anyhow::{anyhow, Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use serde::Deserialize;
use std::net::Ipv4Addr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const SERVICE: &str = "_hue._tcp.local.";
const CLOUD_INTERVAL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeCandidate {
    pub id: String,
    pub ip: Ipv4Addr,
}

#[derive(Clone)]
struct CloudCache {
    checked: Instant,
    result: Result<Vec<BridgeCandidate>, String>,
}

static CLOUD_CACHE: OnceLock<Mutex<Option<CloudCache>>> = OnceLock::new();

pub fn discover_bridge() -> Result<String> {
    discover_bridge_for(None)
}

/// A known identity must never silently select a different bridge after an IP change.
pub fn discover_bridge_for(expected_id: Option<&str>) -> Result<String> {
    let mut candidates = discover_local().unwrap_or_default();
    if candidates.is_empty()
        || expected_id.is_some_and(|id| !candidates.iter().any(|c| c.id.eq_ignore_ascii_case(id)))
    {
        candidates.extend(discover_cloud()?);
    }
    candidates.sort_by(|a, b| a.id.cmp(&b.id).then(a.ip.cmp(&b.ip)));
    candidates.dedup_by(|a, b| a.id == b.id);
    select_bridge(&candidates, expected_id).map(|candidate| candidate.ip.to_string())
}

fn select_bridge<'a>(
    candidates: &'a [BridgeCandidate],
    expected_id: Option<&str>,
) -> Result<&'a BridgeCandidate> {
    if let Some(id) = expected_id {
        return candidates
            .iter()
            .find(|candidate| candidate.id.eq_ignore_ascii_case(id))
            .ok_or_else(|| anyhow!("Paired Hue Bridge was not discovered; check its network connection or pair the replacement bridge"));
    }
    match candidates {
        [candidate] => Ok(candidate),
        [] => Err(anyhow!(
            "No Hue Bridge discovered; enter its IP address manually"
        )),
        _ => Err(anyhow!(
            "Multiple Hue Bridges discovered ({}); enter the intended bridge IP explicitly",
            candidates
                .iter()
                .map(|candidate| candidate.ip.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn discover_local() -> Result<Vec<BridgeCandidate>> {
    let daemon = ServiceDaemon::new().context("Could not start Hue mDNS discovery")?;
    let result: Result<Vec<BridgeCandidate>> = (|| {
        let receiver = daemon.browse(SERVICE)?;
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut candidates = Vec::new();
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            let Ok(event) = receiver.recv_timeout(remaining) else {
                break;
            };
            if let ServiceEvent::ServiceResolved(service) = event {
                let Some(id) = service.get_property_val_str("bridgeid").and_then(bridge_id) else {
                    continue;
                };
                for ip in service.get_addresses_v4() {
                    if !ip.is_unspecified() && !ip.is_multicast() && !ip.is_loopback() {
                        candidates.push(BridgeCandidate { id: id.clone(), ip });
                    }
                }
            }
        }
        Ok(candidates)
    })();
    let _ = daemon.stop_browse(SERVICE);
    if let Ok(stopped) = daemon.shutdown() {
        let _ = stopped.recv_timeout(Duration::from_secs(1));
    }
    result
}

fn discover_cloud() -> Result<Vec<BridgeCandidate>> {
    let mut cache = CLOUD_CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| anyhow!("Hue discovery cache unavailable"))?;
    if let Some(previous) = cache
        .as_ref()
        .filter(|entry| entry.checked.elapsed() < CLOUD_INTERVAL)
    {
        return previous.result.clone().map_err(anyhow::Error::msg);
    }
    let result: Result<Vec<BridgeCandidate>> = (|| {
        let agent = crate::local_http::agent(Some(Duration::from_secs(5)));
        let mut response = agent
            .get("https://discovery.meethue.com/")
            .header(
                "User-Agent",
                concat!("lg-hue-sync/", env!("CARGO_PKG_VERSION")),
            )
            .call()
            .context("Hue cloud discovery failed; enter the bridge IP manually")?;
        #[derive(Deserialize)]
        struct Entry {
            id: String,
            internalipaddress: String,
        }
        let entries: Vec<Entry> = response.body_mut().read_json()?;
        Ok(entries
            .into_iter()
            .filter_map(|entry| {
                let id = bridge_id(&entry.id)?;
                let ip: Ipv4Addr = entry.internalipaddress.parse().ok()?;
                (!ip.is_unspecified() && !ip.is_multicast() && !ip.is_loopback())
                    .then_some(BridgeCandidate { id, ip })
            })
            .collect())
    })();
    // Cache failures too: repeated setup clicks must not exceed the cloud rate limit.
    *cache = Some(CloudCache {
        checked: Instant::now(),
        result: result
            .as_ref()
            .map(Clone::clone)
            .map_err(ToString::to_string),
    });
    result
}

fn bridge_id(id: &str) -> Option<String> {
    (id.len() == 16 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| id.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_never_chooses_an_arbitrary_or_replacement_bridge() {
        let first = BridgeCandidate {
            id: "001788fffe000001".into(),
            ip: Ipv4Addr::new(192, 0, 2, 1),
        };
        let second = BridgeCandidate {
            id: "001788fffe000002".into(),
            ip: Ipv4Addr::new(192, 0, 2, 2),
        };
        assert!(select_bridge(&[], None).is_err());
        assert_eq!(
            select_bridge(std::slice::from_ref(&first), None).unwrap(),
            &first
        );
        let candidates = [first.clone(), second.clone()];
        assert!(select_bridge(&candidates, None).is_err());
        assert_eq!(
            select_bridge(&candidates, Some("001788FFFE000002")).unwrap(),
            &second
        );
        assert!(select_bridge(&candidates, Some("001788fffe000003")).is_err());
        assert_eq!(bridge_id("001788FFFE000001"), Some(first.id));
        assert!(bridge_id("not-a-bridge-id!!").is_none());
    }
}
