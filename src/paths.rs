//! Install locations, resolved exactly like `LinuxBackend::new` so the CLI and
//! the daemon agree with the GUI about where the runtime and state live.
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const DEFAULT_HELPER: &str = "/usr/local/libexec/watch-companion/watch-linux-hci";
pub const PROTOCOL_JAR: &str = "watch-linux-protocol.jar";

pub struct Paths {
    pub protocol: PathBuf,
    pub helper: PathBuf,
    pub state_dir: PathBuf,
}

impl Paths {
    pub fn resolve() -> Self {
        let executable = std::env::current_exe().unwrap_or_default();
        let bundle = executable.parent().unwrap_or(Path::new("."));
        let protocol = std::env::var_os("WATCH_LINUX_PROTOCOL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| bundle.join("lib/protocol"));
        let helper = std::env::var_os("WATCH_LINUX_HCI_HELPER")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_HELPER));
        let state_dir = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/state")
            })
            .join("watch-companion");
        Self {
            protocol,
            helper,
            state_dir,
        }
    }

    pub fn from_json(value: &Value) -> Option<Self> {
        let path = |key: &str| value[key].as_str().map(PathBuf::from);
        Some(Self {
            protocol: path("protocolDir")?,
            helper: path("hciHelper")?,
            state_dir: path("stateDir")?,
        })
    }

    pub fn to_json(&self) -> Value {
        json!({
            "protocolDir": self.protocol.to_string_lossy(),
            "hciHelper": self.helper.to_string_lossy(),
            "stateDir": self.state_dir.to_string_lossy(),
        })
    }

    pub fn log(&self) -> PathBuf {
        self.state_dir.join("protocol.log")
    }
}
