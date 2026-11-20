use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::client::{
    ApplyPresetJsonRequest, BridgeSnapshotResponse, FoundationApiClient, HousingSseEventMsg,
    LiveMoveBatchRequest, LiveMoveBatchResponse, LiveMoveItemRequest, SseBridgeDelta,
};
use crate::coords::{ffxiv_local_to_ue, ue_to_ffxiv_local};
use crate::ipc::{
    GameCommand, GameCommandAck, InboundMoveActor, IpcMailbox, RmpActorsSync, RmpCommitMessage,
    RmpDragMessage, RmpHeartbeat,
};
use crate::matcher::{ActorSlotBinder, enrich_actors_from_snapshot_json};

const DRAG_MIN_INTERVAL: Duration = Duration::from_millis(33);
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(150);
const HEARTBEAT_STALE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Default, serde_derive::Serialize, PartialEq)]
pub struct RelayStatusSnapshot {
    pub running: bool,
    pub attached: bool,
    pub hooks_registered: usize,
    pub hooks_total: usize,
    pub actor_count: usize,
    pub house_size: String,
    pub is_outside: bool,
    pub target_land_ident: Option<i32>,
    pub bound_actors: usize,
    pub outbound_moves: u64,
    pub outbound_commits: u64,
    pub inbound_cmds: u64,
    pub last_error: Option<String>,
}

/// Abstract backend so coordinator sync & echo suppression can be unit-tested without live HTTP/UE5.
pub trait BridgeBackend {
    fn resolve_target_estate(&mut self, override_ident: Option<i32>)
    -> Result<Option<i32>, String>;
    fn fetch_bridge_snapshot(&mut self, land_ident: i32) -> Result<BridgeSnapshotResponse, String>;
    fn post_live_move(
        &mut self,
        land_ident: i32,
        req: &LiveMoveBatchRequest,
    ) -> Result<LiveMoveBatchResponse, String>;
    fn post_preset_apply(
        &mut self,
        land_ident: i32,
        req: &ApplyPresetJsonRequest,
    ) -> Result<(), String>;
    fn fetch_export_remakeplace(&mut self, land_ident: i32, scope: &str) -> Result<String, String>;
}

pub struct LiveFoundationBackend {
    client: FoundationApiClient,
}

impl LiveFoundationBackend {
    pub fn new(client: FoundationApiClient) -> Self {
        Self { client }
    }
}

impl BridgeBackend for LiveFoundationBackend {
    fn resolve_target_estate(
        &mut self,
        override_ident: Option<i32>,
    ) -> Result<Option<i32>, String> {
        if let Some(ident) = override_ident.filter(|&id| id > 0) {
            return Ok(Some(ident));
        }
        match self.client.fetch_status() {
            Ok(status) => {
                if let Some(ident) = status
                    .online_sessions
                    .into_iter()
                    .find_map(|s| s.active_estate.map(|a| a.land_ident))
                {
                    return Ok(Some(ident));
                }
                let estates = self.client.fetch_estates()?;
                Ok(estates.first().map(|e| e.row.land_ident))
            }
            Err(err) => Err(err),
        }
    }

    fn fetch_bridge_snapshot(&mut self, land_ident: i32) -> Result<BridgeSnapshotResponse, String> {
        self.client.fetch_bridge_snapshot(land_ident)
    }

    fn post_live_move(
        &mut self,
        land_ident: i32,
        req: &LiveMoveBatchRequest,
    ) -> Result<LiveMoveBatchResponse, String> {
        self.client.post_live_move(land_ident, req)
    }

    fn post_preset_apply(
        &mut self,
        land_ident: i32,
        req: &ApplyPresetJsonRequest,
    ) -> Result<(), String> {
        self.client.post_preset_apply(land_ident, req)
    }

    fn fetch_export_remakeplace(&mut self, land_ident: i32, scope: &str) -> Result<String, String> {
        self.client.fetch_export_remakeplace(land_ident, scope)
    }
}

pub struct BridgeCoordinator<B: BridgeBackend> {
    backend: B,
    mailbox: IpcMailbox,
    override_land_ident: Option<i32>,
    target_land_ident: Option<i32>,
    binder: ActorSlotBinder,

    last_rmp_wall_ts: u64,
    last_hb_uptime_ms: u64,
    last_hb_seen_at: Option<Instant>,
    hooks_registered: usize,
    hooks_total: usize,
    actor_count: usize,
    house_size: String,
    rmp_is_outside: bool,
    has_attached_rmp: bool,

    last_drag_seq: u64,
    last_commit_seq: u64,
    last_actors_sync_seq: u64,
    last_ack_cmd_id: u64,
    next_cmd_id: u64,
    session_id: String,

    last_drag_sent_at: Option<Instant>,
    last_drag_sig: u64,
    last_own_revision: u64,
    last_rmp_commit_at: Option<Instant>,
    remote_suppress_until: Option<Instant>,
    pending_reload_at: Option<Instant>,
    awaiting_post_reload_rebind: bool,
    last_estate_probe_at: Option<Instant>,

    outbound_moves: u64,
    outbound_commits: u64,
    inbound_cmds: u64,
    last_error: Option<String>,
}

impl<B: BridgeBackend> BridgeCoordinator<B> {
    pub fn new(backend: B, mailbox: IpcMailbox, override_land_ident: Option<i32>) -> Self {
        let epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let session_id = format!("{}-{}", std::process::id(), epoch_ms);
        Self {
            backend,
            mailbox,
            override_land_ident,
            target_land_ident: override_land_ident.filter(|&id| id > 0),
            binder: ActorSlotBinder::default(),
            last_rmp_wall_ts: 0,
            last_hb_uptime_ms: 0,
            last_hb_seen_at: None,
            hooks_registered: 0,
            hooks_total: 0,
            actor_count: 0,
            house_size: String::new(),
            rmp_is_outside: false,
            has_attached_rmp: false,
            last_drag_seq: 0,
            last_commit_seq: 0,
            last_actors_sync_seq: 0,
            last_ack_cmd_id: 0,
            next_cmd_id: 1,
            session_id,
            last_drag_sent_at: None,
            last_drag_sig: 0,
            last_own_revision: 0,
            last_rmp_commit_at: None,
            remote_suppress_until: None,
            pending_reload_at: None,
            awaiting_post_reload_rebind: false,
            last_estate_probe_at: None,
            outbound_moves: 0,
            outbound_commits: 0,
            inbound_cmds: 0,
            last_error: None,
        }
    }

    pub fn binder_len(&self) -> usize {
        self.binder.len()
    }

    pub fn status_snapshot(&self) -> RelayStatusSnapshot {
        RelayStatusSnapshot {
            running: true,
            attached: self.has_attached_rmp,
            hooks_registered: self.hooks_registered,
            hooks_total: self.hooks_total,
            actor_count: self.actor_count,
            house_size: self.house_size.clone(),
            is_outside: self.rmp_is_outside,
            target_land_ident: self.target_land_ident,
            bound_actors: self.binder_len(),
            outbound_moves: self.outbound_moves,
            outbound_commits: self.outbound_commits,
            inbound_cmds: self.inbound_cmds,
            last_error: self.last_error.clone(),
        }
    }

    #[cfg(test)]
    pub fn binder(&self) -> &ActorSlotBinder {
        &self.binder
    }

    fn issue_command(&mut self, mut cmd: GameCommand) {
        let id = self.next_cmd_id;
        self.next_cmd_id += 1;
        cmd.session_id = Some(self.session_id.clone());
        cmd.cmd_id = id;
        if let Err(err) = self.mailbox.write_json_atomic("game_cmd.json", &cmd) {
            let msg = format!("Failed writing game_cmd.json: {err}");
            tracing::warn!("{msg}");
            self.last_error = Some(msg);
        } else {
            self.inbound_cmds += 1;
        }
    }

    fn request_actors(&mut self) {
        self.issue_command(GameCommand {
            session_id: None,
            cmd_id: 0,
            action: "request_actors".to_string(),
            moves: Vec::new(),
            layout_path: None,
            actor_key: None,
            color_hex: None,
            fallback_layout_path: None,
        });
    }

    fn ensure_target_estate(&mut self, now: Instant) -> Option<i32> {
        if let Some(id) = self.target_land_ident {
            return Some(id);
        }
        if let Some(prev) = self.last_estate_probe_at
            && now.duration_since(prev) < Duration::from_secs(2)
        {
            return None;
        }
        let res = self.backend.resolve_target_estate(self.override_land_ident);
        self.last_estate_probe_at = Some(Instant::now());
        match res {
            Ok(Some(id)) => {
                tracing::info!(land_ident = id, "Resolved active Foundation estate");
                self.target_land_ident = Some(id);
                if self.has_attached_rmp {
                    self.request_actors();
                }
                Some(id)
            }
            Ok(None) => None,
            Err(err) => {
                tracing::debug!("Estate probe skipped: {err}");
                None
            }
        }
    }

    pub fn poll_tick(&mut self, sse_events: &[HousingSseEventMsg], now: Instant) {
        for evt in sse_events {
            self.handle_sse_event(evt, now);
        }

        self.poll_heartbeat(now);
        self.poll_ack();

        let _ = self.ensure_target_estate(now);

        if let Some(sync_msg) = self
            .mailbox
            .read_json::<RmpActorsSync>("rmp_actors_sync.json")
            && sync_msg.seq > self.last_actors_sync_seq
        {
            self.last_actors_sync_seq = sync_msg.seq;
            self.handle_actors_sync(sync_msg);
        }

        if let Some(commit_msg) = self
            .mailbox
            .read_json::<RmpCommitMessage>("rmp_commit.json")
            && commit_msg.seq > self.last_commit_seq
        {
            self.last_commit_seq = commit_msg.seq;
            self.handle_rmp_commit(commit_msg, now);
        }

        if let Some(drag_msg) = self.mailbox.read_json::<RmpDragMessage>("rmp_drag.json")
            && drag_msg.seq > self.last_drag_seq
        {
            self.last_drag_seq = drag_msg.seq;
            self.handle_rmp_drag(drag_msg, now);
        }

        if let Some(deadline) = self.pending_reload_at
            && now >= deadline
        {
            self.pending_reload_at = None;
            self.flush_reload_layout(now);
        }
    }

    fn poll_heartbeat(&mut self, now: Instant) {
        let Some(hb) = self.mailbox.read_json::<RmpHeartbeat>("rmp_heartbeat.json") else {
            if self.has_attached_rmp
                && self
                    .last_hb_seen_at
                    .is_some_and(|t| now.duration_since(t) >= HEARTBEAT_STALE_TIMEOUT)
            {
                self.has_attached_rmp = false;
            }
            return;
        };

        if self.last_hb_seen_at.is_some()
            && (hb.uptime_ms < self.last_hb_uptime_ms || hb.wall_ts < self.last_rmp_wall_ts)
        {
            tracing::info!(
                prev_uptime = self.last_hb_uptime_ms,
                new_uptime = hb.uptime_ms,
                "Detected ReMakePlace restart via heartbeat reset; resetting sequence counters"
            );
            self.last_drag_seq = 0;
            self.last_commit_seq = 0;
            self.last_actors_sync_seq = 0;
            self.has_attached_rmp = false;
            self.binder = ActorSlotBinder::default();
            self.last_drag_sig = 0;
            self.awaiting_post_reload_rebind = false;
        }

        let hb_changed = self.last_hb_seen_at.is_none()
            || hb.uptime_ms != self.last_hb_uptime_ms
            || hb.wall_ts != self.last_rmp_wall_ts
            || hb.hooks_registered != self.hooks_registered
            || hb.actor_count != self.actor_count;
        if hb_changed {
            self.last_hb_seen_at = Some(now);
        }

        self.rmp_is_outside = hb.is_outside;
        self.last_rmp_wall_ts = hb.wall_ts;
        self.last_hb_uptime_ms = hb.uptime_ms;
        self.hooks_registered = hb.hooks_registered;
        self.hooks_total = hb.hooks_total;
        self.actor_count = hb.actor_count;
        if !hb.house_size.is_empty() {
            self.house_size = hb.house_size.clone();
        }

        if self.has_attached_rmp
            && self
                .last_hb_seen_at
                .is_some_and(|t| now.duration_since(t) >= HEARTBEAT_STALE_TIMEOUT)
        {
            self.has_attached_rmp = false;
            return;
        }

        if !self.has_attached_rmp && hb.hooks_registered > 0 && hb_changed {
            self.has_attached_rmp = true;
            tracing::info!(
                hooks = hb.hooks_registered,
                actors = hb.actor_count,
                house_size = %hb.house_size,
                is_outside = hb.is_outside,
                "Attached to shadow ReMakePlace FoundationBridge mod"
            );
            self.request_actors();
        }
    }

    fn poll_ack(&mut self) {
        if let Some(ack) = self
            .mailbox
            .read_json::<GameCommandAck>("game_cmd_ack.json")
            && ack.cmd_id > self.last_ack_cmd_id
        {
            self.last_ack_cmd_id = ack.cmd_id;
            if !ack.ok {
                let msg = format!(
                    "ReMakePlace returned error for {}: {}",
                    ack.action,
                    ack.error.as_deref().unwrap_or("unknown")
                );
                tracing::warn!(
                    cmd_id = ack.cmd_id,
                    action = %ack.action,
                    error = ?ack.error,
                    "ReMakePlace returned command error"
                );
                self.last_error = Some(msg);
            }
        }
    }

    fn handle_actors_sync(&mut self, sync_msg: RmpActorsSync) {
        self.rmp_is_outside = sync_msg.is_outside;
        let Some(land_ident) = self.target_land_ident else {
            return;
        };
        let Ok(snap) = self.backend.fetch_bridge_snapshot(land_ident) else {
            return;
        };
        let indoors = !sync_msg.is_outside;
        let slots = if indoors { &snap.indoor } else { &snap.outdoor };
        let preserve = !self.awaiting_post_reload_rebind;
        self.awaiting_post_reload_rebind = false;

        let bound = self
            .binder
            .rebind(indoors, &sync_msg.actors, &[], slots, preserve);
        tracing::info!(
            land_ident,
            indoors,
            actors = sync_msg.actors.len(),
            slots = slots.len(),
            bound,
            "Synchronized RMP actor <-> Foundation slot bindings"
        );
    }

    fn handle_rmp_drag(&mut self, drag_msg: RmpDragMessage, now: Instant) {
        self.rmp_is_outside = drag_msg.is_outside;
        if let Some(until) = self.remote_suppress_until
            && now < until
        {
            return;
        }
        if let Some(last) = self.last_drag_sent_at
            && now.duration_since(last) < DRAG_MIN_INTERVAL
        {
            return;
        }
        let Some(land_ident) = self.target_land_ident else {
            return;
        };

        let expected_indoors = !drag_msg.is_outside;
        let mut moves = Vec::new();
        for actor in &drag_msg.actors {
            let Some((bound_indoors, flat_slot, item_id)) =
                self.binder.resolve_slot(&actor.actor_key)
            else {
                continue;
            };
            if bound_indoors != expected_indoors {
                continue;
            }
            let (pos, rot) = ue_to_ffxiv_local(actor.loc, actor.rot[1]);
            moves.push(LiveMoveItemRequest {
                flat_slot,
                item_id,
                pos_x: pos[0],
                pos_y: pos[1],
                pos_z: pos[2],
                rotation: rot,
            });
        }
        if moves.is_empty() {
            return;
        }

        let sig = hash_moves(&moves);
        if sig == self.last_drag_sig {
            return;
        }

        let req = LiveMoveBatchRequest {
            source: "rmp".to_string(),
            indoors: expected_indoors,
            plot_local: true,
            commit_db: false,
            moves,
        };
        match self.backend.post_live_move(land_ident, &req) {
            Ok(resp) => {
                self.last_drag_sig = sig;
                self.last_drag_sent_at = Some(now);
                self.last_own_revision = self.last_own_revision.max(resp.revision);
                self.outbound_moves += 1;
                self.last_error = None;
            }
            Err(err) => {
                tracing::debug!("live/move failed: {err}");
                self.last_error = Some(err);
            }
        }
    }

    fn handle_rmp_commit(&mut self, commit_msg: RmpCommitMessage, now: Instant) {
        self.rmp_is_outside = commit_msg.is_outside;
        if let Some(until) = self.remote_suppress_until
            && now < until
        {
            return;
        }
        let Some(land_ident) = self.ensure_target_estate(now) else {
            return;
        };

        let snap_path = if commit_msg.snapshot_path.trim().is_empty() {
            self.mailbox.path_for("rmp_snapshot.json")
        } else {
            Path::new(&commit_msg.snapshot_path).to_path_buf()
        };
        let Some(raw_json) = self.mailbox.read_text_with_retry(&snap_path) else {
            tracing::warn!(path = %snap_path.display(), "RMP commit missing snapshot JSON");
            return;
        };

        let indoors = !commit_msg.is_outside;
        let scope = if commit_msg.is_outside {
            "exterior"
        } else {
            "interior"
        };
        let req = ApplyPresetJsonRequest {
            raw_json: raw_json.clone(),
            scope: scope.to_string(),
            reload_mode: "auto".to_string(),
            preserve_pos: true,
            source: "rmp".to_string(),
            skip_snapshot: true,
            label: format!("RMP Sync ({})", commit_msg.reason),
        };

        if let Err(err) = self.backend.post_preset_apply(land_ident, &req) {
            tracing::warn!(land_ident, reason = %commit_msg.reason, "Failed applying RMP commit: {err}");
            self.last_error = Some(err);
            return;
        }
        self.last_rmp_commit_at = Some(now);
        self.outbound_commits += 1;
        self.last_error = None;

        if let Ok(snap) = self.backend.fetch_bridge_snapshot(land_ident) {
            self.last_own_revision = self.last_own_revision.max(snap.revision);
            let enriched = enrich_actors_from_snapshot_json(
                &raw_json,
                commit_msg.is_outside,
                &commit_msg.actors,
            );
            let slots = if indoors { &snap.indoor } else { &snap.outdoor };
            let bound = self
                .binder
                .rebind(indoors, &commit_msg.actors, &enriched, slots, true);
            tracing::info!(
                land_ident,
                reason = %commit_msg.reason,
                scope,
                actors = commit_msg.actors.len(),
                bound,
                rev = snap.revision,
                "Applied RMP commit -> Foundation"
            );
        }
    }

    fn handle_sse_event(&mut self, evt: &HousingSseEventMsg, now: Instant) {
        // Track active estate switch from live game sessions.
        if (evt.kind == "session_state" || evt.kind == "session_switched")
            && self.override_land_ident.is_none()
            && let Some(new_ident) = evt
                .session
                .as_ref()
                .and_then(|s| s.active_estate.as_ref().map(|a| a.land_ident))
                .or(evt.land_ident)
            && self.target_land_ident != Some(new_ident)
        {
            self.target_land_ident = Some(new_ident);
            self.binder.clear_scope(true);
            self.binder.clear_scope(false);
            self.pending_reload_at = Some(now + RELOAD_DEBOUNCE);
        }

        // Strict echo suppression: never mirror RMP's own changes back to RMP.
        if evt.source.as_deref() == Some("rmp") {
            if let Some(rev) = evt.revision {
                self.last_own_revision = self.last_own_revision.max(rev);
            }
            return;
        }
        if evt.source.is_none()
            && let Some(rev) = evt.revision
            && self.last_own_revision > 0
            && rev <= self.last_own_revision
        {
            return;
        }

        let Some(target_ident) = self.target_land_ident else {
            return;
        };
        if let Some(evt_ident) = evt.land_ident
            && evt_ident != target_ident
        {
            return;
        }

        match evt.delta.as_ref() {
            Some(SseBridgeDelta::FurnitureMoved { indoors, moves }) => {
                if *indoors == self.rmp_is_outside {
                    return;
                }
                let mut cmd_moves = Vec::with_capacity(moves.len());
                for m in moves {
                    if let Some(actor_key) = self.binder.resolve_actor_key(*indoors, m.flat_slot) {
                        let (loc, yaw) =
                            ffxiv_local_to_ue([m.local_x, m.local_y, m.local_z], m.local_rotation);
                        cmd_moves.push(InboundMoveActor {
                            actor_key: actor_key.to_string(),
                            loc,
                            yaw,
                        });
                    } else {
                        // Missing actor binding -> schedule layout reload so RMP creates/aligns actors cleanly.
                        self.pending_reload_at = Some(now + RELOAD_DEBOUNCE);
                        return;
                    }
                }
                if !cmd_moves.is_empty() {
                    self.remote_suppress_until = Some(now + Duration::from_millis(900));
                    self.issue_command(GameCommand {
                        session_id: None,
                        cmd_id: 0,
                        action: "move_actors".to_string(),
                        moves: cmd_moves,
                        layout_path: None,
                        actor_key: None,
                        color_hex: None,
                        fallback_layout_path: None,
                    });
                }
            }
            Some(SseBridgeDelta::FurnitureDyed {
                indoors,
                flat_slot,
                color_hex,
                ..
            }) => {
                if *indoors == self.rmp_is_outside {
                    return;
                }
                let actor_key = self
                    .binder
                    .resolve_actor_key(*indoors, *flat_slot)
                    .map(ToString::to_string);
                if let (Some(key), Some(hex)) = (actor_key, color_hex.clone()) {
                    if let Some(layout_path) = self.write_inbound_export(target_ident) {
                        self.remote_suppress_until = Some(now + Duration::from_millis(2500));
                        self.issue_command(GameCommand {
                            session_id: None,
                            cmd_id: 0,
                            action: "dye_actor".to_string(),
                            moves: Vec::new(),
                            layout_path: None,
                            actor_key: Some(key),
                            color_hex: Some(hex),
                            fallback_layout_path: Some(layout_path),
                        });
                    }
                } else {
                    self.pending_reload_at = Some(now + RELOAD_DEBOUNCE);
                }
            }
            Some(SseBridgeDelta::FurnitureAdded { .. })
            | Some(SseBridgeDelta::FurnitureRemoved { .. })
            | Some(SseBridgeDelta::LayoutChanged { .. }) => {
                self.pending_reload_at = Some(now + RELOAD_DEBOUNCE);
            }
            None => {
                if matches!(
                    evt.kind.as_str(),
                    "preset_applied" | "estate_reset" | "estate_updated" | "snapshot_restored"
                ) {
                    if let Some(commit_at) = self.last_rmp_commit_at
                        && evt.source.is_none()
                        && now.duration_since(commit_at) < Duration::from_millis(500)
                    {
                        return;
                    }
                    self.pending_reload_at = Some(now + RELOAD_DEBOUNCE);
                }
            }
        }
    }

    fn write_inbound_export(&mut self, land_ident: i32) -> Option<String> {
        let json = match self.backend.fetch_export_remakeplace(land_ident, "all") {
            Ok(j) => j,
            Err(err) => {
                tracing::warn!(land_ident, "Failed exporting layout from Foundation: {err}");
                return None;
            }
        };
        if let Err(err) = self
            .mailbox
            .write_bytes_atomic("inbound_layout.json", json.as_bytes())
        {
            tracing::warn!("Failed writing inbound_layout.json: {err}");
            return None;
        }
        // Check that target dir wasn't somehow invalid; forward slashes keep UE FString happy.
        let p = self
            .mailbox
            .path_for("inbound_layout.json")
            .to_string_lossy()
            .replace('\\', "/");
        if !Path::new(&p).exists() {
            let _ = fs::write(&p, json.as_bytes());
        }
        Some(p)
    }

    fn flush_reload_layout(&mut self, now: Instant) {
        let Some(land_ident) = self.target_land_ident else {
            return;
        };
        let Some(layout_path) = self.write_inbound_export(land_ident) else {
            return;
        };
        self.remote_suppress_until = Some(now + Duration::from_millis(3200));
        self.awaiting_post_reload_rebind = true;
        self.issue_command(GameCommand {
            session_id: None,
            cmd_id: 0,
            action: "reload_layout".to_string(),
            moves: Vec::new(),
            layout_path: Some(layout_path),
            actor_key: None,
            color_hex: None,
            fallback_layout_path: None,
        });
    }
}

fn hash_moves(moves: &[LiveMoveItemRequest]) -> u64 {
    let mut h = DefaultHasher::new();
    for m in moves {
        m.flat_slot.hash(&mut h);
        m.pos_x.to_bits().hash(&mut h);
        m.pos_y.to_bits().hash(&mut h);
        m.pos_z.to_bits().hash(&mut h);
        m.rotation.to_bits().hash(&mut h);
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::SseMoveItem;
    use crate::matcher::FoundationSlotInfo;

    struct MockBackend {
        land_ident: i32,
        snapshot: BridgeSnapshotResponse,
        export_json: String,
        live_moves: Vec<LiveMoveBatchRequest>,
        applied_presets: Vec<ApplyPresetJsonRequest>,
        next_rev: u64,
    }

    impl MockBackend {
        fn new() -> Self {
            Self {
                land_ident: 34000001,
                snapshot: BridgeSnapshotResponse {
                    land_ident: 34000001,
                    revision: 1,
                    indoor: vec![FoundationSlotInfo {
                        flat_slot: 0,
                        item_id: 21873,
                        catalog_id: 18,
                        name: "Alpine Chair".to_string(),
                        ue_loc: [100.0, 200.0, 0.0],
                        ue_yaw_deg: 0.0,
                    }],
                    outdoor: Vec::new(),
                    ..Default::default()
                },
                export_json: r#"{"houseSize":"Small","interiorScale":100,"interiorFurniture":[]}"#
                    .to_string(),
                live_moves: Vec::new(),
                applied_presets: Vec::new(),
                next_rev: 10,
            }
        }
    }

    impl BridgeBackend for MockBackend {
        fn resolve_target_estate(&mut self, _: Option<i32>) -> Result<Option<i32>, String> {
            Ok(Some(self.land_ident))
        }

        fn fetch_bridge_snapshot(&mut self, _: i32) -> Result<BridgeSnapshotResponse, String> {
            let mut s = self.snapshot.clone();
            s.revision = self.next_rev;
            Ok(s)
        }

        fn post_live_move(
            &mut self,
            land_ident: i32,
            req: &LiveMoveBatchRequest,
        ) -> Result<LiveMoveBatchResponse, String> {
            self.live_moves.push(req.clone());
            self.next_rev += 1;
            Ok(LiveMoveBatchResponse {
                ok: true,
                land_ident,
                revision: self.next_rev,
                applied_count: req.moves.len(),
            })
        }

        fn post_preset_apply(
            &mut self,
            _: i32,
            req: &ApplyPresetJsonRequest,
        ) -> Result<(), String> {
            self.applied_presets.push(req.clone());
            self.next_rev += 1;
            Ok(())
        }

        fn fetch_export_remakeplace(&mut self, _: i32, _: &str) -> Result<String, String> {
            Ok(self.export_json.clone())
        }
    }

    fn temp_mailbox(tag: &str) -> IpcMailbox {
        let dir =
            std::env::temp_dir().join(format!("rmp_bridge_sync_test_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        IpcMailbox::new(dir).unwrap()
    }

    #[test]
    fn suppresses_rmp_own_sse_echoes_and_relays_live_drag() {
        let mailbox = temp_mailbox("echo");
        let dir = mailbox.dir().to_path_buf();
        let mut coord = BridgeCoordinator::new(MockBackend::new(), mailbox.clone(), Some(34000001));
        let t0 = Instant::now();

        // Attach via initial actor sync.
        mailbox
            .write_json_atomic(
                "rmp_actors_sync.json",
                &serde_json::json!({
                    "seq": 1,
                    "wall_ts": 1000,
                    "reason": "init",
                    "is_outside": false,
                    "actors": [{
                        "actor_key": "Chair_1",
                        "item_name": "Alpine Chair",
                        "loc": [100.0, 200.0, 0.0],
                        "rot": [0.0, 0.0, 0.0]
                    }]
                }),
            )
            .unwrap();
        coord.poll_tick(&[], t0);
        assert_eq!(coord.binder().resolve_actor_key(true, 0), Some("Chair_1"));

        // Drag frame in RMP -> should call post_live_move(commit_db=false, source="rmp").
        mailbox
            .write_json_atomic(
                "rmp_drag.json",
                &serde_json::json!({
                    "seq": 1,
                    "wall_ts": 1001,
                    "is_outside": false,
                    "actors": [{
                        "actor_key": "Chair_1",
                        "item_name": "Alpine Chair",
                        "loc": [250.0, -100.0, 50.0],
                        "rot": [0.0, -45.0, 0.0]
                    }]
                }),
            )
            .unwrap();
        coord.poll_tick(&[], t0 + Duration::from_millis(50));
        assert_eq!(coord.backend.live_moves.len(), 1);
        assert!(!coord.backend.live_moves[0].commit_db);
        assert_eq!(coord.backend.live_moves[0].source, "rmp");
        assert!((coord.backend.live_moves[0].moves[0].pos_x - 2.5).abs() < 1e-4);

        // Echoed SSE from source="rmp" must NOT write game_cmd.json!
        let rmp_echo = HousingSseEventMsg {
            kind: "preset_applied".to_string(),
            land_ident: Some(34000001),
            source: Some("rmp".to_string()),
            revision: Some(99),
            ..Default::default()
        };
        coord.poll_tick(&[rmp_echo], t0 + Duration::from_millis(100));
        coord.poll_tick(&[], t0 + Duration::from_millis(400));
        assert!(mailbox.read_json::<GameCommand>("game_cmd.json").is_none());

        // Real game move SSE (source="game") -> emits move_actors command immediately!
        let game_move = HousingSseEventMsg {
            kind: "furniture_moved".to_string(),
            land_ident: Some(34000001),
            source: Some("game".to_string()),
            revision: Some(120),
            delta: Some(SseBridgeDelta::FurnitureMoved {
                indoors: true,
                moves: vec![SseMoveItem {
                    flat_slot: 0,
                    item_id: 21873,
                    catalog_id: 18,
                    pos_x: 4.0,
                    pos_y: 1.0,
                    pos_z: -1.0,
                    rotation: 0.0,
                    local_x: 4.0,
                    local_y: 1.0,
                    local_z: -1.0,
                    local_rotation: 0.0,
                }],
            }),
            ..Default::default()
        };
        coord.poll_tick(&[game_move], t0 + Duration::from_millis(500));
        let cmd: GameCommand = mailbox.read_json("game_cmd.json").unwrap();
        assert_eq!(cmd.action, "move_actors");
        assert_eq!(cmd.moves[0].actor_key, "Chair_1");
        assert!((cmd.moves[0].loc[0] - 400.0).abs() < 0.1);
        assert!((cmd.moves[0].loc[1] - (-100.0)).abs() < 0.1);
        assert!((cmd.moves[0].loc[2] - 100.0).abs() < 0.1);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn structural_game_delta_debounces_reload_layout_command() {
        let mailbox = temp_mailbox("reload");
        let dir = mailbox.dir().to_path_buf();
        let mut coord = BridgeCoordinator::new(MockBackend::new(), mailbox.clone(), Some(34000001));
        let t0 = Instant::now();

        let add_evt = HousingSseEventMsg {
            kind: "furniture_added".to_string(),
            land_ident: Some(34000001),
            source: Some("game".to_string()),
            revision: Some(50),
            delta: Some(SseBridgeDelta::FurnitureAdded {
                indoors: true,
                flat_slot: 1,
                item_id: 28901,
                catalog_id: 331,
            }),
            ..Default::default()
        };
        coord.poll_tick(&[add_evt], t0);
        // Debounce window not elapsed yet.
        coord.poll_tick(&[], t0 + Duration::from_millis(80));
        assert!(mailbox.read_json::<GameCommand>("game_cmd.json").is_none());

        // Past 150ms debounce -> exports layout and issues reload_layout.
        coord.poll_tick(&[], t0 + Duration::from_millis(180));
        let cmd: GameCommand = mailbox.read_json("game_cmd.json").unwrap();
        assert_eq!(cmd.action, "reload_layout");
        assert!(cmd.layout_path.is_some());
        assert!(mailbox.path_for("inbound_layout.json").exists());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn heartbeat_uptime_reset_reattaches_and_resets_sequences() {
        let mailbox = temp_mailbox("hb_reset");
        let dir = mailbox.dir().to_path_buf();
        let mut coord = BridgeCoordinator::new(MockBackend::new(), mailbox.clone(), Some(34000001));
        let t0 = Instant::now();

        // Initial attach with uptime 5000ms and seq 10 drag.
        mailbox
            .write_json_atomic(
                "rmp_heartbeat.json",
                &serde_json::json!({
                    "wall_ts": 1000,
                    "uptime_ms": 5000,
                    "is_outside": false,
                    "house_size": "Small",
                    "actor_count": 1,
                    "hooks_registered": 13,
                    "hooks_total": 13
                }),
            )
            .unwrap();
        mailbox
            .write_json_atomic(
                "rmp_actors_sync.json",
                &serde_json::json!({
                    "seq": 5,
                    "wall_ts": 1000,
                    "reason": "init",
                    "is_outside": false,
                    "actors": [{
                        "actor_key": "Chair_1",
                        "item_name": "Alpine Chair",
                        "loc": [100.0, 200.0, 0.0],
                        "rot": [0.0, 0.0, 0.0]
                    }]
                }),
            )
            .unwrap();
        coord.poll_tick(&[], t0);
        assert!(coord.status_snapshot().attached);
        assert_eq!(coord.binder().resolve_actor_key(true, 0), Some("Chair_1"));

        // Clear outbound command before simulated restart.
        mailbox.clear_stale_outbound_commands();

        // Simulate shadow RMP restarting: uptime drops to 120ms and actor sync seq resets to 1.
        mailbox
            .write_json_atomic(
                "rmp_heartbeat.json",
                &serde_json::json!({
                    "wall_ts": 1005,
                    "uptime_ms": 120,
                    "is_outside": false,
                    "house_size": "Small",
                    "actor_count": 1,
                    "hooks_registered": 13,
                    "hooks_total": 13
                }),
            )
            .unwrap();
        mailbox
            .write_json_atomic(
                "rmp_actors_sync.json",
                &serde_json::json!({
                    "seq": 1,
                    "wall_ts": 1005,
                    "reason": "restart",
                    "is_outside": false,
                    "actors": [{
                        "actor_key": "Chair_Restarted",
                        "item_name": "Alpine Chair",
                        "loc": [100.0, 200.0, 0.0],
                        "rot": [0.0, 0.0, 0.0]
                    }]
                }),
            )
            .unwrap();

        coord.poll_tick(&[], t0 + Duration::from_millis(50));
        let snap = coord.status_snapshot();
        assert!(snap.attached);
        assert_eq!(
            coord.binder().resolve_actor_key(true, 0),
            Some("Chair_Restarted")
        );
        let cmd: GameCommand = mailbox.read_json("game_cmd.json").unwrap();
        assert_eq!(cmd.action, "request_actors");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn stale_heartbeat_marks_rmp_detached() {
        let mailbox = temp_mailbox("hb_stale");
        let dir = mailbox.dir().to_path_buf();
        let mut coord = BridgeCoordinator::new(MockBackend::new(), mailbox.clone(), Some(34000001));
        let t0 = Instant::now();

        mailbox
            .write_json_atomic(
                "rmp_heartbeat.json",
                &serde_json::json!({
                    "wall_ts": 2000,
                    "uptime_ms": 1000,
                    "is_outside": false,
                    "house_size": "Medium",
                    "actor_count": 2,
                    "hooks_registered": 13,
                    "hooks_total": 13
                }),
            )
            .unwrap();
        coord.poll_tick(&[], t0);
        assert!(coord.status_snapshot().attached);

        // No heartbeat update for >= 5s -> detached, without spuriously reattaching on old file.
        coord.poll_tick(&[], t0 + Duration::from_secs(6));
        assert!(!coord.status_snapshot().attached);

        // Subsequent poll still detached until heartbeat changes again.
        coord.poll_tick(&[], t0 + Duration::from_secs(7));
        assert!(!coord.status_snapshot().attached);

        // Fresh heartbeat resumes attached status.
        mailbox
            .write_json_atomic(
                "rmp_heartbeat.json",
                &serde_json::json!({
                    "wall_ts": 2008,
                    "uptime_ms": 8000,
                    "is_outside": false,
                    "house_size": "Medium",
                    "actor_count": 2,
                    "hooks_registered": 13,
                    "hooks_total": 13
                }),
            )
            .unwrap();
        coord.poll_tick(&[], t0 + Duration::from_secs(8));
        assert!(coord.status_snapshot().attached);

        let _ = fs::remove_dir_all(dir);
    }
}
