//! Model identification for any nearby Apple Watch in setup mode.
//!
//! This path deliberately avoids the pairing backend: it asks BlueZ for the
//! FE25 setup service data of nearby devices and decodes it with the upstream
//! wire codec, which is model-agnostic. Pairing itself is still restricted to
//! the one model the upstream protocol implements.

use crate::paths::Paths;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use watch_protocol::advertisement::WatchAdvertisement;
use zbus::{
    blocking::Connection,
    fdo::ManagedObjects,
    zvariant::{OwnedObjectPath, Value as ZValue},
};

/// 16-bit Continuity setup UUID FE25 in its full 128-bit BlueZ form.
pub const SETUP_SERVICE_UUID: &str = "0000fe25-0000-1000-8000-00805f9b34fb";

/// The one configuration upstream's pairing stack implements, from
/// `WatchSetupMetadataCodec` (product 7,5) and its pairing version window.
pub const BASELINE_PRODUCT: (u8, u8) = (7, 5);
pub const BASELINE_PAIRING_VERSIONS: std::ops::RangeInclusive<u8> = 25..=26;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sighting {
    pub address: String,
    pub rssi: Option<i16>,
    pub service_data: Vec<u8>,
    pub advertisement: WatchAdvertisement,
}

impl Sighting {
    pub fn watch_os(&self) -> String {
        let [major, minor, patch] = self.advertisement.system_version;
        format!("{major}.{minor}.{patch}")
    }

    /// Why upstream's pairing stack would or would not accept this Watch.
    /// An empty list means it matches the only configuration that is known to
    /// pair; it does not promise that pairing succeeds.
    pub fn blockers(&self) -> Vec<String> {
        let advertisement = &self.advertisement;
        let mut blockers = Vec::new();
        if (advertisement.product_major, advertisement.product_minor) != BASELINE_PRODUCT {
            blockers.push(format!(
                "model {} is not Watch{},{} (the only model upstream implements)",
                advertisement.product_type(),
                BASELINE_PRODUCT.0,
                BASELINE_PRODUCT.1
            ));
        }
        if !BASELINE_PAIRING_VERSIONS.contains(&advertisement.pairing_version) {
            blockers.push(format!(
                "pairing version {} is outside the supported {}-{}",
                advertisement.pairing_version,
                BASELINE_PAIRING_VERSIONS.start(),
                BASELINE_PAIRING_VERSIONS.end()
            ));
        }
        blockers
    }

    pub fn to_json(&self) -> Value {
        let advertisement = &self.advertisement;
        json!({
            "address": self.address,
            "rssi": self.rssi,
            "productType": advertisement.product_type(),
            "watchOs": self.watch_os(),
            "pairingVersion": advertisement.pairing_version,
            "pairingStrategy": advertisement.pairing_strategy,
            "serviceData": hex(&self.service_data),
            "pairable": self.blockers().is_empty(),
            "blockers": self.blockers(),
        })
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Pull the FE25 payload out of a BlueZ `ServiceData` property and decode it.
/// Devices that advertise no setup data, or data that is not a Watch in setup
/// mode, yield `None`.
pub fn decode_service_data(
    data: &HashMap<String, Vec<u8>>,
) -> Option<(Vec<u8>, WatchAdvertisement)> {
    let bytes = data
        .iter()
        .find(|(uuid, _)| uuid.eq_ignore_ascii_case(SETUP_SERVICE_UUID))
        .map(|(_, bytes)| bytes.clone())?;
    let advertisement = WatchAdvertisement::from_service_data(&bytes)?;
    Some((bytes, advertisement))
}

fn property<'a, T: TryFrom<ZValue<'a>>>(
    interface: &'a HashMap<String, zbus::zvariant::OwnedValue>,
    name: &str,
) -> Option<T> {
    let value: ZValue<'a> = interface.get(name)?.try_clone().ok()?.into();
    T::try_from(value).ok()
}

fn sightings(objects: &ManagedObjects) -> Vec<Sighting> {
    let mut found = Vec::new();
    for interfaces in objects.values() {
        let Some(device) = interfaces
            .iter()
            .find(|(name, _)| name.as_str() == "org.bluez.Device1")
            .map(|(_, properties)| properties)
        else {
            continue;
        };
        let Some(address) = property::<String>(device, "Address") else {
            continue;
        };
        let Some(data) = property::<HashMap<String, Vec<u8>>>(device, "ServiceData") else {
            continue;
        };
        let Some((service_data, advertisement)) = decode_service_data(&data) else {
            continue;
        };
        found.push(Sighting {
            address: address.to_ascii_lowercase(),
            rssi: property::<i16>(device, "RSSI"),
            service_data,
            advertisement,
        });
    }
    found.sort_by(|a, b| b.rssi.cmp(&a.rssi).then_with(|| a.address.cmp(&b.address)));
    found.dedup_by(|a, b| a.address == b.address);
    found
}

fn adapters(objects: &ManagedObjects) -> Vec<OwnedObjectPath> {
    let mut paths: Vec<_> = objects
        .iter()
        .filter(|(_, interfaces)| {
            interfaces
                .keys()
                .any(|name| name.as_str() == "org.bluez.Adapter1")
        })
        .map(|(path, _)| path.clone())
        .collect();
    paths.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    paths
}

fn managed_objects(connection: &Connection) -> Result<ManagedObjects, String> {
    connection
        .call_method(
            Some("org.bluez"),
            "/",
            Some("org.freedesktop.DBus.ObjectManager"),
            "GetManagedObjects",
            &(),
        )
        .map_err(|e| format!("BlueZ is not answering on D-Bus ({e})"))?
        .body()
        .deserialize()
        .map_err(|e| format!("could not read the BlueZ object tree ({e})"))
}

/// Scan passively for Watches in setup mode. Discovery is started on every
/// adapter, which leaves the adapter usable by the rest of the system; nothing
/// connects to, pairs with, or writes to any device.
pub fn scan(timeout: Duration, mut tick: impl FnMut(&[Sighting])) -> Result<Vec<Sighting>, String> {
    let connection =
        Connection::system().map_err(|e| format!("cannot reach the system D-Bus bus ({e})"))?;
    let objects = managed_objects(&connection)?;
    let adapters = adapters(&objects);
    if adapters.is_empty() {
        return Err("BlueZ reports no Bluetooth adapters".into());
    }
    let mut discovering = Vec::new();
    for adapter in &adapters {
        let started = connection.call_method(
            Some("org.bluez"),
            adapter,
            Some("org.bluez.Adapter1"),
            "StartDiscovery",
            &(),
        );
        if started.is_ok() {
            discovering.push(adapter.clone());
        }
    }
    if discovering.is_empty() {
        return Err("no adapter accepted a discovery request (is one in use by watchctld?)".into());
    }
    let deadline = std::time::Instant::now() + timeout;
    let mut seen: Vec<Sighting> = Vec::new();
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        if let Ok(objects) = managed_objects(&connection) {
            let fresh = sightings(&objects);
            if fresh != seen {
                seen = fresh;
                tick(&seen);
            }
        }
    }
    for adapter in discovering {
        let _ = connection.call_method(
            Some("org.bluez"),
            &adapter,
            Some("org.bluez.Adapter1"),
            "StopDiscovery",
            &(),
        );
    }
    Ok(seen)
}

/// Write the capture to its own run directory and return its path. Only the
/// public setup advertisement is recorded; it carries no pairing secret.
pub fn record(paths: &Paths, started_ms: u64, found: &[Sighting]) -> std::io::Result<PathBuf> {
    let directory = paths.state_dir.join("runs").join(started_ms.to_string());
    private_dir(&directory)?;
    let report = json!({
        "schema": 1,
        "command": "identify",
        "startedMs": started_ms,
        "baseline": {
            "productType": format!("Watch{},{}", BASELINE_PRODUCT.0, BASELINE_PRODUCT.1),
            "pairingVersions": [*BASELINE_PAIRING_VERSIONS.start(), *BASELINE_PAIRING_VERSIONS.end()],
        },
        "watches": found.iter().map(Sighting::to_json).collect::<Vec<_>>(),
    });
    let path = directory.join("identify.json");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(format!("{report:#}\n").as_bytes())?;
    Ok(path)
}

/// Create `path` and any missing parents as 0700, independent of the umask.
fn private_dir(path: &Path) -> std::io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        private_dir(parent)?;
    }
    match fs::create_dir(path) {
        Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => return Err(error),
        _ => {}
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The author's physical Watch7,5 / watchOS 26.2 capture.
    const ULTRA_2: [u8; 13] = [
        6, 0x20, 0x86, 0x93, 0xf4, 0xce, 0x64, 0x38, 0x50, 0, 0xd0, 0x10, 0,
    ];

    fn sighting(bytes: &[u8]) -> Sighting {
        let advertisement = WatchAdvertisement::from_service_data(bytes).expect("decodes");
        Sighting {
            address: "aa:bb:cc:dd:ee:01".into(),
            rssi: Some(-51),
            service_data: bytes.to_vec(),
            advertisement,
        }
    }

    #[test]
    fn baseline_watch_is_reported_as_pairable() {
        let found = sighting(&ULTRA_2);
        assert_eq!(found.advertisement.product_type(), "Watch7,5");
        assert_eq!(found.watch_os(), "26.2.0");
        assert_eq!(found.blockers(), Vec::<String>::new());
        assert_eq!(found.to_json()["pairable"], true);
    }

    #[test]
    fn other_models_are_reported_with_the_reason() {
        let mut bytes = ULTRA_2;
        // product major/minor share m[0..3]; shift to a different model.
        bytes[7] = 0x30;
        let found = sighting(&bytes);
        assert_eq!(found.advertisement.product_type(), "Watch6,5");
        let blockers = found.blockers();
        assert_eq!(blockers.len(), 1, "{blockers:?}");
        assert!(blockers[0].contains("is not Watch7,5"), "{blockers:?}");
        assert_eq!(found.to_json()["pairable"], false);
    }

    #[test]
    fn pairing_version_outside_the_window_is_a_blocker() {
        let found = sighting(&ULTRA_2);
        let mut older = found.clone();
        older.advertisement.pairing_version = 24;
        let blockers = older.blockers();
        assert_eq!(blockers.len(), 1, "{blockers:?}");
        assert!(
            blockers[0].contains("outside the supported 25-26"),
            "{blockers:?}"
        );
    }

    #[test]
    fn service_data_is_selected_by_the_setup_uuid_only() {
        let mut data = HashMap::new();
        data.insert("0000180f-0000-1000-8000-00805f9b34fb".into(), vec![1, 2, 3]);
        assert!(decode_service_data(&data).is_none());
        data.insert(SETUP_SERVICE_UUID.to_ascii_uppercase(), ULTRA_2.to_vec());
        let (bytes, advertisement) = decode_service_data(&data).expect("decodes");
        assert_eq!(bytes, ULTRA_2);
        assert_eq!(advertisement.product_type(), "Watch7,5");
    }

    #[test]
    fn non_watch_service_data_is_ignored() {
        let mut data = HashMap::new();
        data.insert(SETUP_SERVICE_UUID.into(), vec![6, 0, 0, 0]);
        assert!(decode_service_data(&data).is_none());
    }

    #[test]
    fn captures_are_written_to_their_own_run_directory() {
        let root = std::env::temp_dir().join(format!("watchctl-identify-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let paths = Paths {
            protocol: root.join("protocol"),
            helper: root.join("hci"),
            state_dir: root.join("state"),
        };
        let path = record(&paths, 1760000000000, &[sighting(&ULTRA_2)]).expect("writes");
        assert!(
            path.ends_with("runs/1760000000000/identify.json"),
            "{path:?}"
        );
        let written = fs::read_to_string(&path).expect("readable");
        assert!(
            written.contains("\"productType\": \"Watch7,5\""),
            "{written}"
        );
        assert!(written.contains("\"pairable\": true"), "{written}");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let _ = fs::remove_dir_all(&root);
    }
}
