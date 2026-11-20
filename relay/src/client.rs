use std::io::{BufRead, BufReader};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use serde_derive::{Deserialize, Serialize};
use ureq::Agent;

use crate::matcher::FoundationSlotInfo;

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
pub struct ApiActiveEstate {
    pub land_ident: i32,
    pub indoors: bool,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
pub struct ApiOnlineSession {
    pub content_id: u64,
    pub character_name: String,
    pub zone_id: u16,
    pub active_estate: Option<ApiActiveEstate>,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
pub struct ApiStatusResponse {
    pub online_sessions: Vec<ApiOnlineSession>,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
pub struct ApiEstateRow {
    pub land_ident: i32,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
pub struct ApiEstateSummary {
    pub row: ApiEstateRow,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
pub struct BridgeSnapshotResponse {
    pub land_ident: i32,
    pub revision: u64,
    pub territory_type_id: u16,
    pub plot_size: u8,
    pub is_apartment: bool,
    pub house_size: String,
    pub light_level: i32,
    pub indoor: Vec<FoundationSlotInfo>,
    pub outdoor: Vec<FoundationSlotInfo>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LiveMoveItemRequest {
    pub flat_slot: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<u32>,
    pub pos_x: f32,
    pub pos_y: f32,
    pub pos_z: f32,
    pub rotation: f32,
}

#[derive(Clone, Debug, Serialize)]
pub struct LiveMoveBatchRequest {
    pub source: String,
    pub indoors: bool,
    pub plot_local: bool,
    pub commit_db: bool,
    pub moves: Vec<LiveMoveItemRequest>,
}

#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
pub struct LiveMoveBatchResponse {
    pub ok: bool,
    pub land_ident: i32,
    pub revision: u64,
    pub applied_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApplyPresetJsonRequest {
    pub raw_json: String,
    pub scope: String,
    pub reload_mode: String,
    pub preserve_pos: bool,
    pub source: String,
    pub skip_snapshot: bool,
    pub label: String,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
pub struct SseMoveItem {
    pub flat_slot: u16,
    pub item_id: u32,
    pub catalog_id: u16,
    pub pos_x: f32,
    pub pos_y: f32,
    pub pos_z: f32,
    pub rotation: f32,
    pub local_x: f32,
    pub local_y: f32,
    pub local_z: f32,
    pub local_rotation: f32,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SseBridgeDelta {
    FurnitureMoved {
        indoors: bool,
        moves: Vec<SseMoveItem>,
    },
    FurnitureDyed {
        indoors: bool,
        flat_slot: u16,
        item_id: u32,
        stain: u8,
        #[serde(default)]
        color_hex: Option<String>,
    },
    FurnitureAdded {
        indoors: bool,
        flat_slot: u16,
        item_id: u32,
        catalog_id: u16,
    },
    FurnitureRemoved {
        indoors: bool,
        flat_slot: u16,
        item_id: u32,
    },
    LayoutChanged {
        indoors: bool,
        reason: String,
    },
}

#[allow(dead_code)]
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(default)]
pub struct HousingSseEventMsg {
    pub kind: String,
    pub land_ident: Option<i32>,
    pub message: String,
    pub session: Option<ApiOnlineSession>,
    pub source: Option<String>,
    pub revision: Option<u64>,
    pub delta: Option<SseBridgeDelta>,
}

pub fn normalize_housing_api_base(raw_url: &str) -> String {
    let trimmed = raw_url.trim().trim_end_matches('/');
    if trimmed.ends_with("/api/v1/housing") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/api/v1/housing")
    }
}

pub struct FoundationApiClient {
    base_url: String,
    agent: Agent,
}

impl FoundationApiClient {
    pub fn new(raw_url: &str) -> Self {
        let config = Agent::config_builder()
            .timeout_connect(Some(Duration::from_millis(500)))
            .timeout_global(Some(Duration::from_secs(6)))
            .build();
        Self {
            base_url: normalize_housing_api_base(raw_url),
            agent: config.into(),
        }
    }

    fn get_string(&self, relative: &str) -> Result<String, String> {
        let url = format!("{}/{}", self.base_url, relative.trim_start_matches('/'));
        let mut resp = self
            .agent
            .get(&url)
            .call()
            .map_err(|e| format!("GET {url} failed: {e}"))?;
        resp.body_mut()
            .read_to_string()
            .map_err(|e| format!("Failed reading {url} body: {e}"))
    }

    fn post_json_raw(&self, relative: &str, body_json: &str) -> Result<String, String> {
        let url = format!("{}/{}", self.base_url, relative.trim_start_matches('/'));
        let mut resp = self
            .agent
            .post(&url)
            .header("Content-Type", "application/json")
            .send(body_json)
            .map_err(|e| format!("POST {url} failed: {e}"))?;
        resp.body_mut()
            .read_to_string()
            .map_err(|e| format!("Failed reading {url} response: {e}"))
    }

    pub fn fetch_status(&self) -> Result<ApiStatusResponse, String> {
        let raw = self.get_string("status")?;
        serde_json::from_str(&raw).map_err(|e| format!("Invalid status JSON: {e}"))
    }

    pub fn fetch_estates(&self) -> Result<Vec<ApiEstateSummary>, String> {
        let raw = self.get_string("estates")?;
        serde_json::from_str(&raw).map_err(|e| format!("Invalid estates JSON: {e}"))
    }

    pub fn fetch_bridge_snapshot(&self, land_ident: i32) -> Result<BridgeSnapshotResponse, String> {
        let raw = self.get_string(&format!("estates/{land_ident}/bridge/snapshot"))?;
        serde_json::from_str(&raw).map_err(|e| format!("Invalid bridge snapshot JSON: {e}"))
    }

    pub fn post_live_move(
        &self,
        land_ident: i32,
        req: &LiveMoveBatchRequest,
    ) -> Result<LiveMoveBatchResponse, String> {
        let body = serde_json::to_string(req).map_err(|e| e.to_string())?;
        let raw = self.post_json_raw(&format!("estates/{land_ident}/live/move"), &body)?;
        serde_json::from_str(&raw).map_err(|e| format!("Invalid live/move response JSON: {e}"))
    }

    pub fn post_preset_apply(
        &self,
        land_ident: i32,
        req: &ApplyPresetJsonRequest,
    ) -> Result<(), String> {
        let body = serde_json::to_string(req).map_err(|e| e.to_string())?;
        let _ = self.post_json_raw(&format!("estates/{land_ident}/preset/apply"), &body)?;
        Ok(())
    }

    pub fn fetch_export_remakeplace(&self, land_ident: i32, scope: &str) -> Result<String, String> {
        self.get_string(&format!(
            "estates/{land_ident}/export/remakeplace?scope={scope}"
        ))
    }
}

pub fn spawn_sse_listener(
    raw_url: &str,
    tx: Sender<HousingSseEventMsg>,
    stop_flag: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    let events_url = format!("{}/events", normalize_housing_api_base(raw_url));
    thread::spawn(move || {
        let agent: Agent = Agent::config_builder()
            .timeout_global(None)
            .timeout_connect(Some(Duration::from_secs(5)))
            .build()
            .into();

        while !stop_flag.load(Ordering::Relaxed) {
            match agent
                .get(&events_url)
                .header("Accept", "text/event-stream")
                .call()
            {
                Ok(mut resp) => {
                    tracing::info!(url = %events_url, "Connected to Foundation Housing SSE stream");
                    let reader = BufReader::new(resp.body_mut().as_reader());
                    for line_res in reader.lines() {
                        if stop_flag.load(Ordering::Relaxed) {
                            return;
                        }
                        match line_res {
                            Ok(line) => {
                                if let Some(rest) = line.strip_prefix("data:") {
                                    let payload = rest.trim();
                                    if !payload.is_empty()
                                        && let Ok(evt) =
                                            serde_json::from_str::<HousingSseEventMsg>(payload)
                                        && tx.send(evt).is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                            Err(err) => {
                                tracing::warn!("SSE stream read ended: {err}");
                                break;
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::debug!("SSE connect retry in 1.5s: {err}");
                }
            }

            for _ in 0..15 {
                if stop_flag.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    })
}
