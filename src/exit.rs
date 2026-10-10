//! Process exit codes and the mapping from backend `status` strings.
pub const OK: u8 = 0;
pub const INVALID: u8 = 2;
pub const BUSY: u8 = 3;
pub const UNAVAILABLE: u8 = 4;
pub const FAILED: u8 = 5;
pub const TIMEOUT: u8 = 6;

pub fn for_status(status: &str) -> u8 {
    match status {
        "OK" | "QUEUED" | "APPLIED" | "IDLE" => OK,
        "INVALID_REQUEST"
        | "UNSUPPORTED_OPERATION"
        | "UNKNOWN_DEVICE"
        | "NO_PIN_CHALLENGE"
        | "NO_PENDING_ACTIVATION"
        | "NO_PENDING_SETUP"
        | "NO_PAIR"
        | "AMBIGUOUS_DEVICE"
        | "ADAPTER_REQUIRED" => INVALID,
        "BUSY" | "ADAPTER_LOADING" => BUSY,
        "UNAVAILABLE"
        | "CORE_UNAVAILABLE"
        | "ADAPTER_UNAVAILABLE"
        | "ADAPTER_DISCOVERY_FAILED"
        | "DAEMON_UNREACHABLE" => UNAVAILABLE,
        "TIMEOUT" => TIMEOUT,
        _ => FAILED,
    }
}

pub fn describe(status: &str) -> &'static str {
    match status {
        "BUSY" => {
            "a session is already running (see `watchctl status`, stop it with `watchctl disconnect`)"
        }
        "UNAVAILABLE" => {
            "backend unavailable: HCI broker, protocol runtime or adapter missing (run `watchctl doctor`)"
        }
        "CORE_UNAVAILABLE" => "the daemon's backend is unavailable",
        "ADAPTER_UNAVAILABLE" => {
            "that Bluetooth adapter is not present (see `watchctl adapters --refresh`)"
        }
        "ADAPTER_REQUIRED" => {
            "several adapters found; choose one with `watchctl adapters select <address>`"
        }
        "ADAPTER_LOADING" => "the adapter inventory is still loading; try again",
        "ADAPTER_DISCOVERY_FAILED" => {
            "could not list Bluetooth adapters (is BlueZ running? see `watchctl doctor`)"
        }
        "INVALID_REQUEST" => "invalid request",
        "UNSUPPORTED_OPERATION" => "the daemon does not support this operation",
        "UNKNOWN_DEVICE" => "that Watch is not currently discovered (run `watchctl scan`)",
        "NO_PIN_CHALLENGE" => "the Watch is not asking for a code",
        "NO_PENDING_ACTIVATION" => "there is no activation to retry",
        "NO_PENDING_SETUP" => "there is no setup waiting for confirmation",
        "NO_PAIR" => "no saved pair; run `watchctl pair` first",
        "REJECTED" => "the protocol host rejected the request",
        _ => "request failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_documented_exit_codes() {
        assert_eq!(for_status("QUEUED"), OK);
        assert_eq!(for_status("APPLIED"), OK);
        assert_eq!(for_status("INVALID_REQUEST"), INVALID);
        assert_eq!(for_status("UNKNOWN_DEVICE"), INVALID);
        assert_eq!(for_status("BUSY"), BUSY);
        assert_eq!(for_status("ADAPTER_REQUIRED"), INVALID);
        assert_eq!(for_status("ADAPTER_DISCOVERY_FAILED"), UNAVAILABLE);
        assert_eq!(for_status("UNAVAILABLE"), UNAVAILABLE);
        assert_eq!(for_status("DAEMON_UNREACHABLE"), UNAVAILABLE);
        assert_eq!(for_status("REJECTED"), FAILED);
        assert_eq!(for_status("TIMEOUT"), TIMEOUT);
        assert_eq!(for_status("SOMETHING_NEW"), FAILED);
    }
}
