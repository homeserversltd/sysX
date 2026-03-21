//! cgroup.events epoll + sweep coordination (stub until wired to sysxd).

/// Placeholder reaper for build/tests; real logic per 04 / 12 §4.1.1.
pub struct Reaper;

impl Reaper {
    pub fn new() -> Self {
        Reaper
    }

    pub fn init(&mut self) -> Result<(), String> {
        Ok(())
    }

    pub fn register_cgroup(&mut self, _path: std::path::PathBuf, _name: String) -> Result<(), String> {
        Ok(())
    }

    pub fn wait_for_sweep_ready(&mut self, _timeout_ms: u64) -> Result<Vec<String>, String> {
        Ok(vec![])
    }

    pub fn perform_sweep(&mut self, _ready: &[String]) -> Result<(), String> {
        Ok(())
    }
}

pub fn reaper_version() -> &'static str {
    "0.1.0"
}
