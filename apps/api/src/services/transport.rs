//! How a DTN bundle leaves this node — the transport (bearer) seam.
//!
//! Delivery used to be hardcoded to an HTTP POST. That is the right default:
//! most of the fabric is reachable over IP. But the product's premise is that
//! IP may be gone for days — a ship with no VSAT, a rover over the horizon — so
//! the *bearer* has to be pluggable: a peer over HTTPS today, and later a LoRa
//! mesh radio, a serial link, or a USB drive carried by hand (sneakernet).
//!
//! This is that seam. It is a static enum, not a `dyn` trait, for the same
//! reason `carriers::Provider` and `settlement::SettlementRail` are: a small
//! closed set of bearers dispatched by `match`. One live variant today —
//! `Https` — with the others left as documented extension points that the
//! radio/serial/sneakernet work (and multi-hop next-hop delivery) plug into by
//! matching their own endpoint scheme.

use reqwest::Client;
use serde_json::Value;

use crate::services::fabric_auth;
use crate::services::identity::NodeIdentity;

/// The outcome of one delivery attempt: whether it was accepted, and a short
/// label for the log (the receiver distinguishes a first acceptance from a
/// repeat, and both are successes).
pub type Delivery = (bool, String);

/// A bearer for one bundle, resolved from its stored `endpoint`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// A peer's `/api/dtn/receive` reached over HTTP(S), behind the fabric
    /// guard. The endpoint is the full URL.
    Https(String),
    /// A directory on a removable drive. "Delivery" writes the bundle there as
    /// a file; an operator physically carries the drive to the destination,
    /// which ingests it. The slowest bearer there is, and the one that works
    /// when every link is gone. The endpoint is `file://<dir>`; the string here
    /// is the directory path.
    Sneakernet(String),
    // Remaining bearers match their own scheme and also need no IP link:
    //   #[cfg(feature = "lora")]   Lora(String),    // lora://<node>
    //   #[cfg(feature = "serial")] Serial(String),  // serial:/dev/ttyUSB0
}

impl Transport {
    /// Resolve an outbox `endpoint` string to a bearer by its scheme. An
    /// `http`/`https` URL is `Https`; anything else has no adapter in this
    /// build and returns `None` (a future adapter claims its own scheme rather
    /// than this one guessing).
    pub fn resolve(endpoint: &str) -> Option<Self> {
        let e = endpoint.trim();
        if e.starts_with("http://") || e.starts_with("https://") {
            Some(Transport::Https(e.to_string()))
        } else if let Some(dir) = e.strip_prefix("file://") {
            // `file:///mnt/usb/dtn` -> `/mnt/usb/dtn`; a bare `file://` with no
            // path is not a destination.
            if dir.is_empty() {
                None
            } else {
                Some(Transport::Sneakernet(dir.to_string()))
            }
        } else {
            None
        }
    }

    /// Short label for logs and metrics.
    pub fn kind(&self) -> &'static str {
        match self {
            Transport::Https(_) => "https",
            Transport::Sneakernet(_) => "sneakernet",
        }
    }

    /// Whether this bearer needs a live IP link. A radio or sneakernet bearer
    /// would return `false` — which is the whole reason the seam exists.
    pub fn needs_ip(&self) -> bool {
        matches!(self, Transport::Https(_))
    }

    /// Deliver one bundle over this bearer.
    ///
    /// The HTTPS adapter signs the request with this node's fabric identity so
    /// the receiving peer's guard accepts it, then reads the receiver's
    /// `status` back for the log. A non-IP adapter would frame and enqueue the
    /// bytes for its medium instead; the forwarder above it does not care which.
    pub async fn deliver(
        &self,
        http: &Client,
        identity: &NodeIdentity,
        payload: &Value,
        node_token: &str,
    ) -> Delivery {
        match self {
            Transport::Https(url) => {
                let request = fabric_auth::signed_post_json(http, identity, url, payload, node_token);
                match request.send().await {
                    Ok(resp) => {
                        let success = resp.status().is_success();
                        let label = resp
                            .json::<Value>()
                            .await
                            .ok()
                            .and_then(|v| {
                                v.get("status").and_then(|s| s.as_str()).map(String::from)
                            })
                            .unwrap_or_else(|| "accepted".into());
                        (success, label)
                    }
                    Err(e) => (false, e.to_string()),
                }
            }
            Transport::Sneakernet(dir) => match write_bundle(dir, payload).await {
                Ok(file) => (true, format!("written to {file}")),
                // A missing directory (drive not mounted) is a retryable
                // failure, not a lost bundle — the forwarder backs off and the
                // bundle stays in the outbox until the drive is there.
                Err(e) => (false, format!("sneakernet write failed: {e}")),
            },
        }
    }
}

/// Keep a bundle file name to a safe, flat token — no path separators, so a
/// hostile `msg_id` cannot escape the drive directory.
fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(128)
        .collect();
    if s.is_empty() {
        "bundle".to_string()
    } else {
        s
    }
}

/// Write one bundle into a sneakernet directory as `<msg_id>.bundle.json`,
/// atomically (temp file then rename within the same directory).
///
/// The directory must already exist. It is a mount point for a removable
/// drive, and creating it would mask an unmounted drive with a local folder
/// that silently swallows every bundle — the opposite of what an operator
/// carrying a USB stick between outposts needs.
pub(crate) async fn write_bundle(dir: &str, payload: &Value) -> std::io::Result<String> {
    let id = payload
        .get("msg_id")
        .and_then(|v| v.as_str())
        .map(sanitize)
        .unwrap_or_else(|| "bundle".to_string());
    let file = format!("{id}.bundle.json");
    let final_path = format!("{dir}/{file}");
    let tmp_path = format!("{dir}/.{file}.tmp");

    let bytes = serde_json::to_vec(payload)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    tokio::fs::write(&tmp_path, &bytes).await?;
    tokio::fs::rename(&tmp_path, &final_path).await?;
    Ok(file)
}

/// List and parse the bundles sitting in a sneakernet directory, oldest-named
/// first. This is the ingest *primitive*: a bundle read here still has to be
/// fed into the receive pipeline (where its envelope signature and the replay
/// dedupe are checked) exactly as an HTTP-delivered one is — carrying it off
/// the drive does not make it trusted. Temp files and non-bundle files are
/// skipped.
pub async fn read_inbound(dir: &str) -> std::io::Result<Vec<(String, Value)>> {
    let mut out = Vec::new();
    let mut entries = tokio::fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".bundle.json") {
            continue;
        }
        let bytes = tokio::fs::read(entry.path()).await?;
        if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
            out.push((name, v));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_and_https_endpoints_resolve_to_the_https_bearer() {
        assert_eq!(
            Transport::resolve("https://peer.example/api/dtn/receive"),
            Some(Transport::Https("https://peer.example/api/dtn/receive".into()))
        );
        assert_eq!(
            Transport::resolve("  http://10.0.0.2:3000/api/dtn/receive  "),
            Some(Transport::Https("http://10.0.0.2:3000/api/dtn/receive".into()))
        );
    }

    #[test]
    fn an_endpoint_with_no_adapter_resolves_to_nothing() {
        // No LoRa/serial adapter is compiled in this build, so these are
        // unreachable rather than silently treated as HTTP.
        assert_eq!(Transport::resolve("lora://outpost-b"), None);
        assert_eq!(Transport::resolve("serial:/dev/ttyUSB0"), None);
        assert_eq!(Transport::resolve("file://"), None); // a scheme with no path
        assert_eq!(Transport::resolve(""), None);
    }

    #[test]
    fn https_needs_an_ip_link() {
        assert!(Transport::Https("https://x/".into()).needs_ip());
        assert_eq!(Transport::Https("https://x/".into()).kind(), "https");
    }

    #[test]
    fn a_file_endpoint_resolves_to_the_sneakernet_bearer() {
        assert_eq!(
            Transport::resolve("file:///mnt/usb/dtn"),
            Some(Transport::Sneakernet("/mnt/usb/dtn".into()))
        );
        let s = Transport::Sneakernet("/mnt/usb/dtn".into());
        assert_eq!(s.kind(), "sneakernet");
        // A drive needs no network — the entire reason the bearer exists.
        assert!(!s.needs_ip());
    }

    #[tokio::test]
    async fn a_bundle_round_trips_through_a_sneakernet_directory() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tidw-sneakernet-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let dir_s = dir.to_string_lossy().into_owned();

        let a = json!({ "msg_id": "11111111-2222-3333-4444-555555555555", "payload": {"n": 1} });
        let b = json!({ "msg_id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "payload": {"n": 2} });
        let fa = write_bundle(&dir_s, &a).await.unwrap();
        write_bundle(&dir_s, &b).await.unwrap();
        assert!(fa.ends_with(".bundle.json"));

        let read = read_inbound(&dir_s).await.unwrap();
        assert_eq!(read.len(), 2, "both bundles are listed");
        let ids: Vec<&str> = read
            .iter()
            .filter_map(|(_, v)| v.get("msg_id").and_then(|m| m.as_str()))
            .collect();
        assert!(ids.contains(&"11111111-2222-3333-4444-555555555555"), "content survives intact");

        // A half-written temp file is not a deliverable bundle.
        tokio::fs::write(dir.join(".x.bundle.json.tmp"), b"{}").await.unwrap();
        assert_eq!(read_inbound(&dir_s).await.unwrap().len(), 2, "temp files are skipped");

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn a_hostile_msg_id_cannot_escape_the_directory() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tidw-sneakernet-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let dir_s = dir.to_string_lossy().into_owned();

        let evil = json!({ "msg_id": "../../etc/passwd", "payload": {} });
        let file = write_bundle(&dir_s, &evil).await.unwrap();
        assert!(!file.contains('/') && !file.contains(".."), "path is sanitised: {file}");
        assert_eq!(read_inbound(&dir_s).await.unwrap().len(), 1);

        tokio::fs::remove_dir_all(&dir).await.ok();
    }
}
