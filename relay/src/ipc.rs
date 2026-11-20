use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::ser::Serialize;
use serde_derive::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct RmpActorPose {
    pub actor_key: String,
    #[serde(default)]
    pub item_name: String,
    pub loc: [f32; 3],
    pub rot: [f32; 3],
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
pub struct RmpHeartbeat {
    pub wall_ts: u64,
    pub uptime_ms: u64,
    pub is_outside: bool,
    pub house_size: String,
    pub light_level: f32,
    pub actor_count: usize,
    pub hooks_registered: usize,
    pub hooks_total: usize,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
pub struct RmpDragMessage {
    pub seq: u64,
    pub wall_ts: u64,
    pub is_outside: bool,
    pub house_size: String,
    pub light_level: f32,
    pub actors: Vec<RmpActorPose>,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
pub struct RmpCommitMessage {
    pub seq: u64,
    pub wall_ts: u64,
    pub reason: String,
    pub is_outside: bool,
    pub house_size: String,
    pub light_level: f32,
    pub snapshot_ok: bool,
    pub snapshot_path: String,
    pub actors: Vec<RmpActorPose>,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
pub struct RmpActorsSync {
    pub seq: u64,
    pub wall_ts: u64,
    pub reason: String,
    pub is_outside: bool,
    pub house_size: String,
    pub light_level: f32,
    pub actors: Vec<RmpActorPose>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct InboundMoveActor {
    pub actor_key: String,
    pub loc: [f32; 3],
    pub yaw: f32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GameCommand {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub cmd_id: u64,
    pub action: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub moves: Vec<InboundMoveActor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_hex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_layout_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
pub struct GameCommandAck {
    pub cmd_id: u64,
    pub action: String,
    pub ok: bool,
    pub moved_count: usize,
    pub actor_count: usize,
    pub error: Option<String>,
}

/// Manages file mailbox I/O under `rmp-bridge/runtime/bridge_ipc/`.
#[derive(Clone, Debug)]
pub struct IpcMailbox {
    dir: PathBuf,
}

impl IpcMailbox {
    pub fn new(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    #[cfg(test)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path_for(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Removes stale relay-side command/temp files on startup without clobbering active RMP state.
    pub fn clear_stale_outbound_commands(&self) {
        for name in [
            "game_cmd.json",
            "game_cmd.json.tmp",
            "game_cmd_ack.json",
            "inbound_layout.json",
            "inbound_layout.json.tmp",
        ] {
            let _ = fs::remove_file(self.dir.join(name));
        }
    }

    /// Removes stale RMP-side mailbox state files before spawning a fresh shadow ReMakePlace process.
    pub fn clear_stale_rmp_state(&self) {
        for name in [
            "rmp_heartbeat.json",
            "rmp_heartbeat.json.tmp",
            "rmp_actors_sync.json",
            "rmp_actors_sync.json.tmp",
            "rmp_drag.json",
            "rmp_drag.json.tmp",
            "rmp_commit.json",
            "rmp_commit.json.tmp",
            "rmp_snapshot.json",
            "rmp_snapshot.json.tmp",
        ] {
            let _ = fs::remove_file(self.dir.join(name));
        }
    }

    pub fn read_text_with_retry(&self, path: &Path) -> Option<String> {
        for attempt in 0..5 {
            match fs::read_to_string(path) {
                Ok(text) => return Some(text),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
                Err(_) if attempt < 4 => thread::sleep(Duration::from_millis(8)),
                Err(_) => return None,
            }
        }
        None
    }

    pub fn read_json<T: DeserializeOwned>(&self, filename: &str) -> Option<T> {
        let raw = self.read_text_with_retry(&self.dir.join(filename))?;
        serde_json::from_str(&raw).ok()
    }

    pub fn write_bytes_atomic(&self, filename: &str, bytes: &[u8]) -> io::Result<()> {
        let target = self.dir.join(filename);
        let tmp = self.dir.join(format!("{filename}.tmp"));
        fs::write(&tmp, bytes)?;
        for attempt in 0..5 {
            match fs::rename(&tmp, &target) {
                Ok(()) => return Ok(()),
                Err(_) if attempt < 4 => {
                    let _ = fs::remove_file(&target);
                    thread::sleep(Duration::from_millis(8));
                }
                Err(err) => {
                    let _ = fs::remove_file(&tmp);
                    return Err(err);
                }
            }
        }
        Ok(())
    }

    pub fn write_json_atomic<T: Serialize>(&self, filename: &str, value: &T) -> io::Result<()> {
        let text = serde_json::to_string(value).map_err(io::Error::other)?;
        self.write_bytes_atomic(filename, text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_and_read_roundtrip() {
        let root = std::env::temp_dir().join(format!(
            "rmp_bridge_ipc_test_{}_{}",
            std::process::id(),
            999
        ));
        let _ = fs::remove_dir_all(&root);
        let mailbox = IpcMailbox::new(&root).unwrap();

        let cmd = GameCommand {
            session_id: None,
            cmd_id: 7,
            action: "move_actors".to_string(),
            moves: vec![InboundMoveActor {
                actor_key: "Furn_1".to_string(),
                loc: [10.0, 20.0, 30.0],
                yaw: 45.0,
            }],
            layout_path: None,
            actor_key: None,
            color_hex: None,
            fallback_layout_path: None,
        };
        mailbox.write_json_atomic("game_cmd.json", &cmd).unwrap();

        let read_back: GameCommand = mailbox.read_json("game_cmd.json").unwrap();
        assert_eq!(read_back, cmd);

        // Overwrite existing file atomically.
        let mut cmd2 = cmd.clone();
        cmd2.cmd_id = 8;
        mailbox.write_json_atomic("game_cmd.json", &cmd2).unwrap();
        let read_back2: GameCommand = mailbox.read_json("game_cmd.json").unwrap();
        assert_eq!(read_back2.cmd_id, 8);

        let _ = fs::remove_dir_all(&root);
    }
}
