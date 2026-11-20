use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use rmp_bridge_relay::{RelayConfig, RelayHandle};

const DEFAULT_API_URL: &str = "http://127.0.0.1:21064";
const DEFAULT_IPC_DIR: &str = r"E:\FF14\Repos\PrivateTest\rmp-bridge\runtime\bridge_ipc";

struct CliArgs {
    api_url: String,
    ipc_dir: PathBuf,
    land_ident: Option<i32>,
    parent_pid: Option<u32>,
}

fn parse_args() -> CliArgs {
    let mut api_url = DEFAULT_API_URL.to_string();
    let mut ipc_dir = PathBuf::from(DEFAULT_IPC_DIR);
    let mut land_ident = None;
    let mut parent_pid = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--api-url" => {
                if let Some(val) = args.next() {
                    api_url = val;
                }
            }
            "--ipc-dir" => {
                if let Some(val) = args.next() {
                    ipc_dir = PathBuf::from(val);
                }
            }
            "--land-ident" => {
                if let Some(val) = args.next()
                    && let Ok(parsed) = val.parse::<i32>()
                    && parsed > 0
                {
                    land_ident = Some(parsed);
                }
            }
            "--parent-pid" => {
                if let Some(val) = args.next()
                    && let Ok(parsed) = val.parse::<u32>()
                    && parsed > 0
                {
                    parent_pid = Some(parsed);
                }
            }
            _ => {}
        }
    }

    CliArgs {
        api_url,
        ipc_dir,
        land_ident,
        parent_pid,
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_max_level(tracing::Level::INFO)
        .init();

    let args = parse_args();
    tracing::info!(
        api_url = %args.api_url,
        ipc_dir = %args.ipc_dir.display(),
        land_ident = ?args.land_ident,
        parent_pid = ?args.parent_pid,
        "Starting rmp-bridge-relay"
    );

    let handle = RelayHandle::start(
        RelayConfig {
            api_url: args.api_url,
            ipc_dir: args.ipc_dir,
            land_ident: args.land_ident,
            parent_pid: args.parent_pid,
            clear_rmp_state_files: false,
        },
        None,
        |_| {},
    )
    .expect("failed to start rmp-bridge-relay");

    while handle.is_running() {
        thread::sleep(Duration::from_millis(100));
    }
    handle.stop();
}
