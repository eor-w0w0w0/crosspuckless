/// Host sessions belong to Steam's main process. Child processes keep the
/// existing cached/debug behavior and must never compete for a host session.
pub(crate) fn connection_worker_allowed(process: &str, host_bridge_enabled: bool) -> bool {
    host_bridge_enabled && process.eq_ignore_ascii_case("steam.exe")
}

pub(crate) fn device_change_count(original: u32, generation: u32) -> u32 {
    original.wrapping_add(generation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_requires_steam_and_enabled_bridge() {
        assert!(connection_worker_allowed("STEAM.EXE", true));
        assert!(!connection_worker_allowed("steam.exe", false));
        assert!(!connection_worker_allowed("steamwebhelper.exe", true));
        assert!(!connection_worker_allowed("game.exe", true));
    }

    #[test]
    fn counter_keeps_original_and_wraps_generation() {
        assert_eq!(device_change_count(10, 2), 12);
        assert_eq!(device_change_count(u32::MAX, 1), 0);
    }
}
