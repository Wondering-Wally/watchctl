//! Human-readable rendering of backend snapshots and the change events that
//! streaming commands emit.
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    time::{SystemTime, UNIX_EPOCH},
};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "-".into(),
        other => other.to_string(),
    }
}

fn yes(value: &Value) -> &'static str {
    if *value == true { "yes" } else { "no" }
}

pub fn device_line(device: &Value) -> String {
    format!(
        "{}  {}  watchOS {}  rssi {}",
        text(&device["id"]),
        text(&device["productType"]),
        text(&device["watchOs"]),
        text(&device["rssi"])
    )
}

pub fn adapter_line(adapter: &Value, selected: Option<&str>) -> String {
    let mark = if adapter["id"].as_str().is_some() && adapter["id"].as_str() == selected {
        "*"
    } else {
        " "
    };
    format!(
        "{mark} {}  {}  {}  {}",
        text(&adapter["controller"]),
        text(&adapter["address"]),
        text(&adapter["name"]),
        if adapter["powered"] == true {
            "powered"
        } else {
            "off"
        }
    )
}

pub fn adapters_human(snapshot: &Value) -> String {
    let selected = snapshot["selectedAdapterId"].as_str();
    let mut lines: Vec<String> = snapshot["bluetoothAdapters"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|a| adapter_line(a, selected))
        .collect();
    if lines.is_empty() {
        lines.push("no Bluetooth adapters found".into());
    }
    lines.push(format!(
        "selection: {}",
        text(&snapshot["adapterSelectionStatus"])
    ));
    if let Some(error) = snapshot["adapterSelectionError"].as_str() {
        lines.push(format!("error: {error}"));
    }
    lines.join("\n")
}

pub fn snapshot_human(snapshot: &Value) -> String {
    let mut lines = vec![
        format!("phase:      {}", text(&snapshot["phase"])),
        format!(
            "adapter:    {} (selection {})",
            text(&snapshot["adapterState"]),
            text(&snapshot["adapterSelectionStatus"])
        ),
        format!(
            "pair:       {} {}",
            snapshot["pairId"].as_str().unwrap_or("none"),
            snapshot["productType"].as_str().unwrap_or("")
        )
        .trim_end()
        .to_string(),
        format!(
            "activated:  {}   ready: {}   verified: {}",
            yes(&snapshot["activationConfirmed"]),
            yes(&snapshot["watchReady"]),
            yes(&snapshot["operationalEligible"])
        ),
    ];
    if let Some(error) = snapshot["error"].as_str() {
        lines.push(format!("error:      {error}"));
    }
    if let Some(reason) = snapshot["capabilities"]["pairingUnavailableReason"].as_str() {
        lines.push(format!("backend:    unavailable ({reason})"));
    }
    let devices = snapshot["devices"].as_array().cloned().unwrap_or_default();
    if !devices.is_empty() {
        lines.push("devices:".into());
        lines.extend(devices.iter().map(|d| format!("  {}", device_line(d))));
    }
    let selected = snapshot["selectedAdapterId"].as_str();
    let adapters = snapshot["bluetoothAdapters"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !adapters.is_empty() {
        lines.push("adapters:".into());
        lines.extend(
            adapters
                .iter()
                .map(|a| format!("  {}", adapter_line(a, selected))),
        );
    }
    lines.push(format!("log:        {}", text(&snapshot["logPath"])));
    lines.join("\n")
}

/// Turns successive snapshots into `phase` / `device` / `error` events.
#[derive(Default)]
pub struct Tracker {
    phase: Option<String>,
    error: Option<String>,
    devices: HashSet<String>,
}

impl Tracker {
    pub fn events(&mut self, snapshot: &Value) -> Vec<Value> {
        let mut events = Vec::new();
        let phase = snapshot["phase"].as_str().map(str::to_owned);
        if phase != self.phase {
            if let Some(phase) = &phase {
                events.push(json!({"type":"phase","phase":phase}));
            }
            self.phase = phase;
        }
        for device in snapshot["devices"].as_array().into_iter().flatten() {
            if let Some(id) = device["id"].as_str()
                && self.devices.insert(id.to_owned())
            {
                events.push(json!({"type":"device","device":device}));
            }
        }
        let error = snapshot["error"].as_str().map(str::to_owned);
        if error != self.error {
            if let Some(error) = &error {
                events.push(json!({"type":"error","error":error}));
            }
            self.error = error;
        }
        events
    }
}

pub fn event_human(event: &Value) -> String {
    match event["type"].as_str() {
        Some("phase") => format!("phase: {}", text(&event["phase"])),
        Some("device") => format!("device {}", device_line(&event["device"])),
        Some("error") => format!("error: {}", text(&event["error"])),
        Some("hint") => format!("hint: {}", text(&event["message"])),
        _ => event.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_reports_only_changes() {
        let mut tracker = Tracker::default();
        let device = json!({"id":"A1","productType":"Watch7,5","watchOs":"26.2","rssi":-51});
        let first = tracker.events(&json!({"phase":"DISCOVERING","devices":[device],"error":null}));
        assert_eq!(first.len(), 2);
        assert_eq!(first[0]["phase"], "DISCOVERING");
        assert_eq!(
            event_human(&first[1]),
            "device A1  Watch7,5  watchOS 26.2  rssi -51"
        );
        assert!(
            tracker
                .events(&json!({"phase":"DISCOVERING","devices":[device]}))
                .is_empty()
        );
        let failed = tracker.events(&json!({"phase":"FAILED","devices":[],"error":"boom"}));
        assert_eq!(
            failed,
            vec![
                json!({"type":"phase","phase":"FAILED"}),
                json!({"type":"error","error":"boom"})
            ]
        );
    }

    #[test]
    fn snapshot_human_marks_selected_adapter_and_errors() {
        let snapshot = json!({
            "phase":"OPERATIONAL","adapterState":"POWERED_ON","adapterSelectionStatus":"READY",
            "pairId":"P1","productType":"Watch7,5","activationConfirmed":true,"watchReady":true,
            "operationalEligible":true,"error":null,"devices":[],"logPath":"/x/protocol.log",
            "selectedAdapterId":"AA:BB:CC:DD:EE:FF",
            "bluetoothAdapters":[{"id":"AA:BB:CC:DD:EE:FF","address":"AA:BB:CC:DD:EE:FF","name":"usb","controller":"hci1","powered":true}],
            "capabilities":{"pairingUnavailableReason":null}
        });
        let text = snapshot_human(&snapshot);
        assert!(text.contains("phase:      OPERATIONAL"));
        assert!(text.contains("pair:       P1 Watch7,5"));
        assert!(text.contains("activated:  yes   ready: yes   verified: yes"));
        assert!(text.contains("  * hci1  AA:BB:CC:DD:EE:FF  usb  powered"));
        assert!(!text.contains("backend:"));
        assert!(!text.contains("error:"));
    }
}
