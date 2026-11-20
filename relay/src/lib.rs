pub mod client;
pub mod coords;
pub mod ipc;
pub mod matcher;
pub mod sync;

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use client::{FoundationApiClient, HousingSseEventMsg, spawn_sse_listener};
use ipc::IpcMailbox;
use sync::{BridgeCoordinator, LiveFoundationBackend};

pub use sync::RelayStatusSnapshot;

#[derive(Clone, Debug)]
pub struct RelayConfig {
    pub api_url: String,
    pub ipc_dir: PathBuf,
    pub land_ident: Option<i32>,
    pub parent_pid: Option<u32>,
    pub clear_rmp_state_files: bool,
}

#[cfg(windows)]
pub fn is_process_alive(pid: u32) -> bool {
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;

    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn GetExitCodeProcess(handle: isize, exit_code: *mut u32) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle == 0 {
            return false;
        }
        let mut exit_code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        ok != 0 && exit_code == STILL_ACTIVE
    }
}

#[cfg(not(windows))]
pub fn is_process_alive(_pid: u32) -> bool {
    true
}

pub struct RelayHandle {
    stop_flag: Arc<AtomicBool>,
    running_flag: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl RelayHandle {
    pub fn start<F>(
        config: RelayConfig,
        external_sse_rx: Option<Receiver<HousingSseEventMsg>>,
        mut on_status: F,
    ) -> io::Result<Self>
    where
        F: FnMut(RelayStatusSnapshot) + Send + 'static,
    {
        let mailbox = IpcMailbox::new(&config.ipc_dir)?;
        mailbox.clear_stale_outbound_commands();
        if config.clear_rmp_state_files {
            mailbox.clear_stale_rmp_state();
        }

        let stop_flag = Arc::new(AtomicBool::new(false));
        let running_flag = Arc::new(AtomicBool::new(true));

        let (sse_rx, internal_sse_handle) = match external_sse_rx {
            Some(rx) => (rx, None),
            None => {
                let (tx, rx) = mpsc::channel();
                let handle = spawn_sse_listener(&config.api_url, tx, Arc::clone(&stop_flag));
                (rx, Some(handle))
            }
        };

        let thread_stop = Arc::clone(&stop_flag);
        let thread_running = Arc::clone(&running_flag);

        let worker = thread::Builder::new()
            .name("rmp-bridge-relay".to_string())
            .spawn(move || {
                let backend = LiveFoundationBackend::new(FoundationApiClient::new(&config.api_url));
                let mut coordinator = BridgeCoordinator::new(backend, mailbox, config.land_ident);

                let mut last_parent_check = Instant::now();
                let initial = coordinator.status_snapshot();
                on_status(initial.clone());
                let mut prev_status = Some(initial);

                while !thread_stop.load(Ordering::Relaxed) {
                    if let Some(pid) = config.parent_pid
                        && last_parent_check.elapsed() >= Duration::from_millis(250)
                    {
                        last_parent_check = Instant::now();
                        if !is_process_alive(pid) {
                            tracing::info!(
                                parent_pid = pid,
                                "Parent process exited; shutting down relay"
                            );
                            break;
                        }
                    }

                    let mut drained = Vec::new();
                    loop {
                        match sse_rx.try_recv() {
                            Ok(evt) => drained.push(evt),
                            Err(mpsc::TryRecvError::Empty) => break,
                            Err(mpsc::TryRecvError::Disconnected) => {
                                if external_sse_rx_disconnected(&internal_sse_handle) {
                                    // External SSE channel was intentionally closed by caller on shutdown.
                                    break;
                                }
                                break;
                            }
                        }
                    }

                    coordinator.poll_tick(&drained, Instant::now());
                    let current = coordinator.status_snapshot();
                    if prev_status.as_ref() != Some(&current) {
                        on_status(current.clone());
                        prev_status = Some(current);
                    }

                    thread::sleep(Duration::from_millis(20));
                }

                thread_stop.store(true, Ordering::Relaxed);
                thread_running.store(false, Ordering::Relaxed);

                let mut stopped = coordinator.status_snapshot();
                stopped.running = false;
                stopped.attached = false;
                on_status(stopped);

                // Keep internal SSE handle ownership clean on CLI shutdown.
                drop(internal_sse_handle);
            })?;

        Ok(Self {
            stop_flag,
            running_flag,
            worker: Some(worker),
        })
    }

    pub fn is_running(&self) -> bool {
        self.running_flag.load(Ordering::Relaxed)
    }

    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for RelayHandle {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        // Join only if a handle wasn't already stopped explicitly; if external SSE closed, thread exits within <=20ms.
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

fn external_sse_rx_disconnected(internal_sse_handle: &Option<JoinHandle<()>>) -> bool {
    internal_sse_handle.is_none()
}
