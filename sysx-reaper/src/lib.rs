//! Kernel cgroup.events observer — `04` reaper band, `12` §4.1.1 sweep trigger.
//!
//! Uses **`epoll(7)`** on each **`…/sysx/<svc>/cgroup.events`** (`POLLPRI` / `POLLIN`). When
//! **`populated`** transitions **1 → 0** and the service directory still exists, invokes the
//! callback so **`sysxd`** can run DFS **`rmdir`** and update epistemic state.

use std::collections::HashMap;
use std::fs::{read_dir, File};
use std::io;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use log::{info, warn};
use nix::sys::epoll::{Epoll, EpollCreateFlags, EpollEvent, EpollFlags, EpollTimeout};

use sysx_runtime::read_sysfs_cgroup_populated;

static NEXT_COOKIE: AtomicU64 = AtomicU64::new(1);

/// Opaque join handle (best-effort shutdown not implemented in v1).
pub struct ObserverHandle;

/// Spawn background **`epoll`** observer on **`slice_root`** (e.g. `/sys/fs/cgroup/sysx`).
pub fn spawn_slice_observer<F>(slice_root: PathBuf, on_populated_zero: F) -> io::Result<ObserverHandle>
where
    F: Fn(String) + Send + Sync + 'static,
{
    let cb = std::sync::Arc::new(on_populated_zero);
    thread::Builder::new()
        .name("sysx-reaper".into())
        .spawn(move || run_epoll_loop(slice_root, cb))
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    Ok(ObserverHandle)
}

struct Tracked {
    #[allow(dead_code)]
    file: File,
}

fn run_epoll_loop(slice_root: PathBuf, on_populated_zero: std::sync::Arc<dyn Fn(String) + Send + Sync>) {
    let epoll = match Epoll::new(EpollCreateFlags::empty()) {
        Ok(e) => e,
        Err(e) => {
            warn!("reaper: epoll_create failed: {}", e);
            return;
        }
    };

    let mut cookie_to_name: HashMap<u64, String> = HashMap::new();
    let mut last_pop: HashMap<String, bool> = HashMap::new();
    let mut tracked: HashMap<String, Tracked> = HashMap::new();

    info!("reaper: epoll observer slice={}", slice_root.display());

    loop {
        add_new_services(
            &slice_root,
            &epoll,
            &mut tracked,
            &mut cookie_to_name,
            &mut last_pop,
        );

        let mut events = [EpollEvent::empty(); 32];
        let timeout = match EpollTimeout::try_from(Duration::from_secs(2)) {
            Ok(t) => t,
            Err(_) => EpollTimeout::NONE,
        };
        let n = match epoll.wait(&mut events, timeout) {
            Ok(n) => n,
            Err(e) => {
                warn!("reaper: epoll_wait: {}", e);
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };

        for ev in &events[..n] {
            let cookie = ev.data();
            let Some(name) = cookie_to_name.get(&cookie).cloned() else {
                continue;
            };
            let events_path = slice_root.join(&name).join("cgroup.events");
            let pop = read_sysfs_cgroup_populated(&events_path).ok().flatten();
            let prev = last_pop.get(&name).copied();
            let now_true = pop == Some(true);
            let now_false = pop == Some(false);
            if now_true {
                last_pop.insert(name.clone(), true);
            } else if now_false {
                let was_true = prev == Some(true);
                last_pop.insert(name.clone(), false);
                let dir = slice_root.join(&name);
                if was_true && dir.is_dir() {
                    info!(
                        "reaper: populated 1→0 service={} — DFS sweep request (12 §4.1.1)",
                        name
                    );
                    on_populated_zero(name);
                }
            }
        }

        if n == 0 {
            // Timeout: re-read all known services (edge might be missed)
            poll_all_state(
                &slice_root,
                &mut last_pop,
                &on_populated_zero,
            );
        }
    }
}

fn poll_all_state(
    slice_root: &PathBuf,
    last_pop: &mut HashMap<String, bool>,
    on_populated_zero: &std::sync::Arc<dyn Fn(String) + Send + Sync>,
) {
    let Ok(rd) = read_dir(slice_root) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || sysx_runtime::validate_service_id(&name).is_err() {
            continue;
        }
        let events_path = e.path().join("cgroup.events");
        let pop = read_sysfs_cgroup_populated(&events_path).ok().flatten();
        let prev = last_pop.get(&name).copied();
        let now_true = pop == Some(true);
        let now_false = pop == Some(false);
        if now_true {
            last_pop.insert(name.clone(), true);
        } else if now_false {
            let was_true = prev == Some(true);
            last_pop.insert(name.clone(), false);
            if was_true && e.path().is_dir() {
                info!(
                    "reaper (poll): populated 1→0 service={} — DFS sweep request",
                    name
                );
                on_populated_zero(name);
            }
        }
    }
}

fn add_new_services(
    slice_root: &PathBuf,
    epoll: &Epoll,
    tracked: &mut HashMap<String, Tracked>,
    cookie_to_name: &mut HashMap<u64, String>,
    last_pop: &mut HashMap<String, bool>,
) {
    let Ok(rd) = read_dir(slice_root) else {
        return;
    };

    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || sysx_runtime::validate_service_id(&name).is_err() {
            continue;
        }
        if tracked.contains_key(&name) {
            continue;
        }

        let events_path = e.path().join("cgroup.events");
        let file = match File::open(&events_path) {
            Ok(f) => f,
            Err(err) => {
                warn!("reaper: open {}: {}", events_path.display(), err);
                continue;
            }
        };

        let cookie = NEXT_COOKIE.fetch_add(1, Ordering::Relaxed);
        cookie_to_name.insert(cookie, name.clone());

        let ev = EpollEvent::new(EpollFlags::EPOLLIN | EpollFlags::EPOLLPRI, cookie);
        if let Err(err) = epoll.add(file.as_fd(), ev) {
            warn!("reaper: epoll add {}: {}", events_path.display(), err);
            cookie_to_name.remove(&cookie);
            continue;
        }

        if let Ok(p) = read_sysfs_cgroup_populated(&events_path) {
            last_pop.insert(name.clone(), p == Some(true));
        }

        info!("reaper: epoll watch service={}", name);
        tracked.insert(name, Tracked { file });
    }
}

pub fn reaper_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
