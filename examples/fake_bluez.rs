//! Minimal stand-in for BlueZ on a private bus, used by tests/fake_bluez/run.sh.
//! It serves the object tree `watchctl identify` reads, with real D-Bus types.
use std::collections::HashMap;
use zbus::{blocking::connection, fdo::ObjectManager, interface, zvariant::OwnedValue};

const SETUP: &str = "0000fe25-0000-1000-8000-00805f9b34fb";
const ULTRA_2: [u8; 13] = [
    6, 0x20, 0x86, 0x93, 0xf4, 0xce, 0x64, 0x38, 0x50, 0, 0xd0, 0x10, 0,
];

struct Adapter;
#[interface(name = "org.bluez.Adapter1")]
impl Adapter {
    fn start_discovery(&self) {}
    fn stop_discovery(&self) {}
    #[zbus(property)]
    fn address(&self) -> String {
        "AA:BB:CC:DD:EE:01".into()
    }
}

struct Device {
    address: &'static str,
    rssi: i16,
    service_data: Option<(&'static str, Vec<u8>)>,
}
#[interface(name = "org.bluez.Device1")]
impl Device {
    #[zbus(property)]
    fn address(&self) -> String {
        self.address.into()
    }
    #[zbus(property, name = "RSSI")]
    fn rssi(&self) -> i16 {
        self.rssi
    }
    #[zbus(property)]
    fn service_data(&self) -> HashMap<String, OwnedValue> {
        self.service_data
            .iter()
            .map(|(uuid, bytes)| {
                let value = zbus::zvariant::Value::from(bytes.clone());
                (uuid.to_string(), value.try_into().expect("owned"))
            })
            .collect()
    }
}

fn main() -> zbus::Result<()> {
    let mut other_model = ULTRA_2;
    other_model[7] = 0x30;
    let devices = [
        (
            "dev_1",
            Device {
                address: "11:22:33:44:55:01",
                rssi: -51,
                service_data: Some((SETUP, ULTRA_2.to_vec())),
            },
        ),
        (
            "dev_2",
            Device {
                address: "11:22:33:44:55:02",
                rssi: -60,
                service_data: Some((SETUP, other_model.to_vec())),
            },
        ),
        (
            "dev_3",
            Device {
                address: "11:22:33:44:55:03",
                rssi: -40,
                service_data: Some(("0000180f-0000-1000-8000-00805f9b34fb", vec![1, 2, 3])),
            },
        ),
        (
            "dev_4",
            Device {
                address: "11:22:33:44:55:04",
                rssi: -45,
                service_data: None,
            },
        ),
    ];
    let mut builder = connection::Builder::session()?
        .name("org.bluez")?
        .serve_at("/", ObjectManager)?
        .serve_at("/org/bluez/hci0", Adapter)?;
    for (name, device) in devices {
        builder = builder.serve_at(format!("/org/bluez/hci0/{name}"), device)?;
    }
    let _connection = builder.build()?;
    println!("ready");
    loop {
        std::thread::park();
    }
}
