local UEHelpers = require("UEHelpers")

local DEFAULT_IPC_DIR = "E:/FF14/Repos/PrivateTest/rmp-bridge/runtime/bridge_ipc"
local IPC_DIR = (os.getenv("RMP_BRIDGE_IPC_DIR") or DEFAULT_IPC_DIR):gsub("\\", "/"):gsub("/+$", "")

local PATH_HEARTBEAT = IPC_DIR .. "/rmp_heartbeat.json"
local PATH_DRAG = IPC_DIR .. "/rmp_drag.json"
local PATH_COMMIT = IPC_DIR .. "/rmp_commit.json"
local PATH_SNAPSHOT = IPC_DIR .. "/rmp_snapshot.json"
local PATH_ACTORS_SYNC = IPC_DIR .. "/rmp_actors_sync.json"
local PATH_GAME_CMD = IPC_DIR .. "/game_cmd.json"
local PATH_GAME_CMD_ACK = IPC_DIR .. "/game_cmd_ack.json"

-- Runtime state
local now_ms = 0
local drag_seq = 0
local commit_seq = 0
local actors_sync_seq = 0
local last_heartbeat_ms = -2000
local last_hook_retry_ms = -2000
local last_processed_cmd_id = 0
local last_processed_session_id = ""

local is_applying_remote = false
local is_dumping_snapshot = false
local remote_suppress_until_ms = 0
local pending_commit_reason = nil
local pending_commit_at_ms = nil

local pending_remote_reload = false
local pending_remote_reload_cmd_id = 0
local pending_orig_save_path = ""
local pending_orig_save_name = nil
local pending_reload_deadline_ms = 0
local custom_prop_registered = false

local last_seen_target_transforms = {}
local last_drag_payload_sig = ""
local logged_errors = {}

local FurnitureClass = nil

-- Minimal UTF-8 safe JSON encoder & decoder (zero external native DLL dependencies)
local json = {}

local function utf8_from_codepoint(cp)
    if cp < 0x80 then
        return string.char(cp)
    elseif cp < 0x800 then
        return string.char(
            0xC0 + math.floor(cp / 64),
            0x80 + (cp % 64)
        )
    elseif cp < 0x10000 then
        return string.char(
            0xE0 + math.floor(cp / 4096),
            0x80 + (math.floor(cp / 64) % 64),
            0x80 + (cp % 64)
        )
    elseif cp <= 0x10FFFF then
        return string.char(
            0xF0 + math.floor(cp / 262144),
            0x80 + (math.floor(cp / 4096) % 64),
            0x80 + (math.floor(cp / 64) % 64),
            0x80 + (cp % 64)
        )
    end
    return "?"
end

local function json_escape_str(s)
    local out = s:gsub('[%z\1-\31\\"]', function(c)
        if c == '"' then return '\\"'
        elseif c == '\\' then return '\\\\'
        elseif c == '\b' then return '\\b'
        elseif c == '\f' then return '\\f'
        elseif c == '\n' then return '\\n'
        elseif c == '\r' then return '\\r'
        elseif c == '\t' then return '\\t'
        else return string.format("\\u%04x", string.byte(c))
        end
    end)
    return '"' .. out .. '"'
end

local function is_array(tbl)
    local count = 0
    for k, _ in pairs(tbl) do
        if type(k) ~= "number" or k < 1 or math.floor(k) ~= k then
            return false
        end
        count = count + 1
    end
    if count == 0 then
        return tbl[1] == nil and #tbl == 0
    end
    return count == #tbl
end

function json.encode(val)
    local t = type(val)
    if val == nil then
        return "null"
    elseif t == "boolean" then
        return val and "true" or "false"
    elseif t == "number" then
        if val ~= val or val == math.huge or val == -math.huge then
            return "0"
        end
        if math.type(val) == "integer" or (math.floor(val) == val and math.abs(val) < 9007199254740992) then
            return string.format("%d", val)
        end
        local s = string.format("%.6f", val):gsub("0+$", ""):gsub("%.$", "")
        if s == "-0" or s == "" then
            return "0"
        end
        return s
    elseif t == "string" then
        return json_escape_str(val)
    elseif t == "table" then
        if val._json_empty_array then
            return "[]"
        end
        if #val > 0 and is_array(val) then
            local parts = {}
            for i = 1, #val do
                parts[i] = json.encode(val[i])
            end
            return "[" .. table.concat(parts, ",") .. "]"
        else
            local parts = {}
            for k, v in pairs(val) do
                if type(k) == "string" or type(k) == "number" then
                    parts[#parts + 1] = json_escape_str(tostring(k)) .. ":" .. json.encode(v)
                end
            end
            return "{" .. table.concat(parts, ",") .. "}"
        end
    end
    return "null"
end

function json.decode(str)
    if type(str) ~= "string" then
        return nil, "expected string"
    end
    -- Strip UTF-8 BOM if present
    if str:sub(1, 3) == "\xEF\xBB\xBF" then
        str = str:sub(4)
    end
    local pos = 1
    local len = #str

    local function skip_ws()
        while pos <= len do
            local b = str:byte(pos)
            if b == 32 or b == 9 or b == 10 or b == 13 then
                pos = pos + 1
            else
                break
            end
        end
    end

    local parse_value

    local function parse_string()
        pos = pos + 1 -- skip opening '"'
        local buf = {}
        while pos <= len do
            local c = str:sub(pos, pos)
            if c == '"' then
                pos = pos + 1
                return table.concat(buf)
            elseif c == '\\' then
                pos = pos + 1
                local esc = str:sub(pos, pos)
                pos = pos + 1
                if esc == '"' or esc == '\\' or esc == '/' then
                    buf[#buf + 1] = esc
                elseif esc == 'b' then buf[#buf + 1] = '\b'
                elseif esc == 'f' then buf[#buf + 1] = '\f'
                elseif esc == 'n' then buf[#buf + 1] = '\n'
                elseif esc == 'r' then buf[#buf + 1] = '\r'
                elseif esc == 't' then buf[#buf + 1] = '\t'
                elseif esc == 'u' then
                    local hex = str:sub(pos, pos + 3)
                    pos = pos + 4
                    local cp = tonumber(hex, 16) or 0
                    if cp >= 0xD800 and cp <= 0xDBFF and str:sub(pos, pos + 1) == "\\u" then
                        local hex2 = str:sub(pos + 2, pos + 5)
                        local cp2 = tonumber(hex2, 16) or 0
                        if cp2 >= 0xDC00 and cp2 <= 0xDFFF then
                            pos = pos + 6
                            cp = 0x10000 + ((cp - 0xD800) * 0x400) + (cp2 - 0xDC00)
                        end
                    end
                    buf[#buf + 1] = utf8_from_codepoint(cp)
                else
                    buf[#buf + 1] = esc
                end
            else
                buf[#buf + 1] = c
                pos = pos + 1
            end
        end
        return table.concat(buf)
    end

    local function parse_number()
        local start = pos
        local s = str:match("^-?%d+%.?%d*[eE]?[+-]?%d*", pos)
        if not s or #s == 0 then
            error("Invalid JSON number at " .. tostring(start))
        end
        pos = pos + #s
        return tonumber(s)
    end

    local function parse_array()
        pos = pos + 1 -- skip '['
        local arr = {}
        skip_ws()
        if str:sub(pos, pos) == ']' then
            pos = pos + 1
            return arr
        end
        while pos <= len do
            arr[#arr + 1] = parse_value()
            skip_ws()
            local ch = str:sub(pos, pos)
            if ch == ']' then
                pos = pos + 1
                return arr
            elseif ch == ',' then
                pos = pos + 1
            else
                error("Expected ',' or ']' at " .. tostring(pos))
            end
        end
        return arr
    end

    local function parse_object()
        pos = pos + 1 -- skip '{'
        local obj = {}
        skip_ws()
        if str:sub(pos, pos) == '}' then
            pos = pos + 1
            return obj
        end
        while pos <= len do
            skip_ws()
            if str:sub(pos, pos) ~= '"' then
                error("Expected object key at " .. tostring(pos))
            end
            local key = parse_string()
            skip_ws()
            if str:sub(pos, pos) ~= ':' then
                error("Expected ':' at " .. tostring(pos))
            end
            pos = pos + 1
            local v = parse_value()
            obj[key] = v
            skip_ws()
            local ch = str:sub(pos, pos)
            if ch == '}' then
                pos = pos + 1
                return obj
            elseif ch == ',' then
                pos = pos + 1
            else
                error("Expected ',' or '}' at " .. tostring(pos))
            end
        end
        return obj
    end

    parse_value = function()
        skip_ws()
        local ch = str:sub(pos, pos)
        if ch == '"' then return parse_string()
        elseif ch == '{' then return parse_object()
        elseif ch == '[' then return parse_array()
        elseif ch == 't' and str:sub(pos, pos + 3) == "true" then
            pos = pos + 4
            return true
        elseif ch == 'f' and str:sub(pos, pos + 4) == "false" then
            pos = pos + 5
            return false
        elseif ch == 'n' and str:sub(pos, pos + 3) == "null" then
            pos = pos + 4
            return nil
        else
            return parse_number()
        end
    end

    local ok, res = pcall(parse_value)
    if not ok then
        return nil, res
    end
    return res
end

local function log_info(msg)
    print("[FoundationBridge] " .. tostring(msg) .. "\n")
end

local function log_error_once(key, msg)
    if not logged_errors[key] then
        logged_errors[key] = true
        print("[FoundationBridge][ERROR] " .. key .. ": " .. tostring(msg) .. "\n")
    end
end

local function safe_call(fn, ...)
    local ok, res = pcall(fn, ...)
    if ok then
        return res
    end
    return nil
end

local function is_valid_uobj(obj)
    if obj == nil then
        return false
    end
    local ok, valid = pcall(function() return obj:IsValid() end)
    return ok and valid == true
end

local function for_each_uarray(arr, fn)
    if not arr then
        return
    end
    local num = safe_call(function() return arr:GetArrayNum() end)
    if type(num) ~= "number" or num <= 0 then
        return
    end
    safe_call(function()
        arr:ForEach(function(index, elem)
            local val = safe_call(function() return elem:get() end)
            if val ~= nil then
                fn(val, index)
            end
        end)
    end)
end

local function read_text_file(path)
    local f = io.open(path, "rb")
    if not f then
        return nil
    end
    local data = f:read("*a")
    f:close()
    return data
end

local function write_json_atomic(path, tbl)
    local encoded = json.encode(tbl)
    local tmp = path .. ".tmp"
    local f, err = io.open(tmp, "wb")
    if not f then
        log_error_once("write_" .. path, "failed to open temp file: " .. tostring(err))
        return false
    end
    f:write(encoded)
    f:close()
    os.remove(path)
    local ok, rerr = os.rename(tmp, path)
    if not ok then
        -- Retry direct write if rename hits transient AV/indexer lock
        local df = io.open(path, "wb")
        if df then
            df:write(encoded)
            df:close()
            os.remove(tmp)
            return true
        end
        log_error_once("rename_" .. path, tostring(rerr))
        return false
    end
    return true
end

local function get_controller()
    local ctrl = safe_call(function() return FindFirstOf("PlaceController_C") end)
    if is_valid_uobj(ctrl) then return ctrl end
    ctrl = safe_call(function() return FindFirstOf("XivController") end)
    if is_valid_uobj(ctrl) then return ctrl end
    ctrl = safe_call(function() return UEHelpers.GetPlayerController() end)
    if is_valid_uobj(ctrl) then return ctrl end
    return nil
end

local function get_xiv_instance()
    local gi = safe_call(function() return FindFirstOf("XivInstance") end)
    if is_valid_uobj(gi) then return gi end
    gi = safe_call(function() return UEHelpers.GetGameInstance() end)
    if is_valid_uobj(gi) then return gi end
    return nil
end

local function read_scene_meta()
    local ctrl = get_controller()
    local gi = get_xiv_instance()

    local is_outside = false
    local light_level = 0.0
    local house_size = ""

    if ctrl then
        is_outside = (safe_call(function() return ctrl.IsOutside end) == true)
        light_level = safe_call(function() return ctrl.LightLevel end) or 0.0
        local hm = safe_call(function() return ctrl.HouseManager end)
        if is_valid_uobj(hm) then
            local hm_size = safe_call(function() return hm.CurrentSize:ToString() end)
            if hm_size and #hm_size > 0 then
                house_size = hm_size
            end
        end
    end

    if house_size == "" and gi then
        local ss_size = safe_call(function() return gi.SaveStruct.HouseSize:ToString() end)
        if ss_size and #ss_size > 0 then
            house_size = ss_size
        end
    end

    if gi and light_level == 0.0 then
        local ss_light = safe_call(function() return gi.SaveStruct.LightLevel end)
        if type(ss_light) == "number" then
            light_level = ss_light
        end
    end

    return ctrl, gi, is_outside, house_size, light_level
end

local function read_actor_transform(actor)
    local loc = safe_call(function() return actor:K2_GetActorLocation() end)
    local rot = safe_call(function() return actor:K2_GetActorRotation() end)
    if not loc or not rot then
        return nil
    end
    return {
        loc = { loc.X or 0.0, loc.Y or 0.0, loc.Z or 0.0 },
        rot = { rot.Pitch or 0.0, rot.Yaw or 0.0, rot.Roll or 0.0 },
    }
end

local function ensure_furniture_class()
    if is_valid_uobj(FurnitureClass) then
        return FurnitureClass
    end
    FurnitureClass = safe_call(function() return StaticFindObject("/Script/MakePlaceCore.Furniture") end)
    return FurnitureClass
end

local function is_furniture_actor(obj)
    if not is_valid_uobj(obj) then
        return false
    end
    local fcls = ensure_furniture_class()
    if is_valid_uobj(fcls) then
        return safe_call(function() return obj:IsA(fcls) end) == true
    end
    local full = safe_call(function() return obj:GetFullName() end) or ""
    return full:find("Furniture", 1, true) ~= nil
end

local function collect_furniture_actors()
    local list = safe_call(function() return FindAllOf("Furniture") end)
    local actors = {}
    if not list then
        return actors
    end
    for _, actor in ipairs(list) do
        if is_valid_uobj(actor) then
            local key = safe_call(function() return actor:GetFullName() end)
            local xf = read_actor_transform(actor)
            if key and xf then
                local item_name = safe_call(function() return actor:GetItemName():ToString() end) or ""
                if item_name == "" then
                    local disp = safe_call(function() return actor:GetDisplayText():ToString() end)
                    if type(disp) == "string" and #disp > 0 then
                        item_name = disp
                    end
                end
                actors[#actors + 1] = {
                    actor_key = key,
                    item_name = item_name,
                    loc = xf.loc,
                    rot = xf.rot,
                }
            end
        end
    end
    return actors
end

local function build_actor_map()
    local list = safe_call(function() return FindAllOf("Furniture") end)
    local map = {}
    if not list then
        return map
    end
    for _, actor in ipairs(list) do
        if is_valid_uobj(actor) then
            local key = safe_call(function() return actor:GetFullName() end)
            if key then
                map[key] = actor
            end
        end
    end
    return map
end

local function write_actors_sync(reason)
    local _, _, is_outside, house_size, light_level = read_scene_meta()
    local actors = collect_furniture_actors()
    actors_sync_seq = actors_sync_seq + 1
    write_json_atomic(PATH_ACTORS_SYNC, {
        seq = actors_sync_seq,
        wall_ts = os.time(),
        reason = reason or "sync",
        is_outside = is_outside,
        house_size = house_size,
        light_level = light_level,
        actors = (#actors > 0) and actors or { _json_empty_array = true },
    })
    return #actors
end

local function schedule_commit(reason, delay_ms)
    if is_applying_remote or is_dumping_snapshot or now_ms < remote_suppress_until_ms then
        return
    end
    pending_commit_reason = reason or "changed"
    pending_commit_at_ms = now_ms + (delay_ms or 100)
end

local function ensure_file_download_property()
    if custom_prop_registered then
        return true
    end
    local ok, err = pcall(function()
        RegisterCustomProperty({
            Name = "BridgeFilePath",
            Type = PropertyTypes.StrProperty,
            BelongsToClass = "/Script/MakePlace.FileDownload",
            OffsetInternal = 0x28,
        })
    end)
    if ok then
        custom_prop_registered = true
        log_info("Registered custom property UFileDownload.BridgeFilePath (+0x28)")
        return true
    end
    log_error_once("reg_custom_prop", tostring(err))
    return false
end

local function dump_layout_snapshot(reason, force)
    if is_dumping_snapshot then
        return false
    end
    if not force and (is_applying_remote or now_ms < remote_suppress_until_ms) then
        return false
    end

    local ctrl, gi, is_outside, house_size, light_level = read_scene_meta()
    if not ctrl or not gi then
        return false
    end

    is_dumping_snapshot = true
    local snapshot_ok = false
    local orig_path = safe_call(function() return gi.SavePath:ToString() end) or ""
    local main_ui = safe_call(function() return ctrl.MainUI end)
    local save_box = is_valid_uobj(main_ui) and safe_call(function() return main_ui.SaveNameBox end) or nil
    local orig_box_name = is_valid_uobj(save_box) and safe_call(function() return save_box:GetText():ToString() end) or nil

    local ok, err = pcall(function()
        gi.SavePath = PATH_SNAPSHOT
        if is_valid_uobj(save_box) then
            safe_call(function() save_box:SetText(FText("rmp_snapshot")) end)
        end
        local effective_name = (is_valid_uobj(save_box) and safe_call(function() return save_box:GetText():ToString() end)) or "rmp_snapshot"
        if not effective_name or #effective_name == 0 then
            effective_name = "rmp_snapshot"
        end
        local save_ok, save_err = pcall(function()
            ctrl:Save(false)
        end)
        if not save_ok then
            log_error_once("save_snapshot", tostring(save_err))
        else
            local alt_path = IPC_DIR .. "/" .. effective_name .. ".json"
            if effective_name ~= "rmp_snapshot" and alt_path ~= PATH_SNAPSHOT then
                local alt_data = read_text_file(alt_path)
                if alt_data and #alt_data > 0 then
                    os.remove(PATH_SNAPSHOT)
                    os.rename(alt_path, PATH_SNAPSHOT)
                end
            end
            local data = read_text_file(PATH_SNAPSHOT)
            snapshot_ok = (data ~= nil and #data > 0)
        end
    end)

    -- Ensure SavePath and SaveNameBox are always restored even on error
    safe_call(function() gi.SavePath = orig_path end)
    if orig_box_name ~= nil and is_valid_uobj(save_box) then
        safe_call(function() save_box:SetText(FText(orig_box_name)) end)
    end
    is_dumping_snapshot = false

    if not ok then
        log_error_once("dump_layout_snapshot", tostring(err))
        return false
    end

    if force then
        return snapshot_ok
    end

    local actors = collect_furniture_actors()
    commit_seq = commit_seq + 1
    write_json_atomic(PATH_COMMIT, {
        seq = commit_seq,
        wall_ts = os.time(),
        reason = reason or "commit",
        is_outside = is_outside,
        house_size = house_size,
        light_level = light_level,
        snapshot_ok = snapshot_ok,
        snapshot_path = PATH_SNAPSHOT,
        actors = (#actors > 0) and actors or { _json_empty_array = true },
    })
    return snapshot_ok
end

local function seed_target_transforms(targets)
    if not targets then
        return
    end
    for_each_uarray(targets, function(t)
        if is_furniture_actor(t) then
            local key = safe_call(function() return t:GetFullName() end)
            local xf = key and read_actor_transform(t)
            if key and xf then
                last_seen_target_transforms[key] = xf
            end
        end
    end)
end

local function finish_remote_reload(reason)
    if not pending_remote_reload then
        return
    end
    local ctrl, gi = get_controller(), get_xiv_instance()
    if gi and pending_orig_save_path ~= nil then
        safe_call(function() gi.SavePath = pending_orig_save_path end)
    end
    if ctrl and pending_orig_save_name ~= nil then
        local main_ui = safe_call(function() return ctrl.MainUI end)
        local save_box = is_valid_uobj(main_ui) and safe_call(function() return main_ui.SaveNameBox end) or nil
        if is_valid_uobj(save_box) then
            safe_call(function() save_box:SetText(FText(pending_orig_save_name)) end)
        end
        pending_orig_save_name = nil
    end
    pending_remote_reload = false
    is_applying_remote = false
    remote_suppress_until_ms = now_ms + 1200
    last_seen_target_transforms = {}

    if ctrl then
        local tm = safe_call(function() return ctrl.TargetManager end)
        if is_valid_uobj(tm) then
            local tlist = safe_call(function() return tm.Targets end)
            seed_target_transforms(tlist)
        end
    end

    local count = write_actors_sync(reason or "post_remote_reload")
    if pending_remote_reload_cmd_id > 0 then
        write_json_atomic(PATH_GAME_CMD_ACK, {
            cmd_id = pending_remote_reload_cmd_id,
            action = "reload_layout",
            ok = true,
            actor_count = count,
            wall_ts = os.time(),
        })
        pending_remote_reload_cmd_id = 0
    end
end

local function apply_move_actors(cmd)
    is_applying_remote = true
    remote_suppress_until_ms = now_ms + 800

    local actor_map = build_actor_map()
    local moved_count = 0
    local missing = {}

    if type(cmd.moves) == "table" then
        for _, m in ipairs(cmd.moves) do
            local key = m.actor_key
            local actor = key and actor_map[key]
            if actor and is_valid_uobj(actor) and type(m.loc) == "table" and #m.loc >= 3 then
                local cur_rot = safe_call(function() return actor:K2_GetActorRotation() end) or { Pitch = 0.0, Yaw = 0.0, Roll = 0.0 }
                local new_loc = { X = tonumber(m.loc[1]) or 0.0, Y = tonumber(m.loc[2]) or 0.0, Z = tonumber(m.loc[3]) or 0.0 }
                local new_yaw = tonumber(m.yaw) or cur_rot.Yaw or 0.0
                local new_rot = {
                    Pitch = cur_rot.Pitch or 0.0,
                    Yaw = new_yaw,
                    Roll = cur_rot.Roll or 0.0,
                }

                local ok = safe_call(function()
                    return actor:K2_SetActorLocationAndRotation(new_loc, new_rot, false, {}, true)
                end)
                if not ok then
                    safe_call(function()
                        actor:K2_TeleportTo(new_loc, new_rot)
                        actor:K2_SetActorRotation(new_rot, true)
                    end)
                end

                local applied_xf = read_actor_transform(actor)
                if applied_xf then
                    last_seen_target_transforms[key] = applied_xf
                else
                    last_seen_target_transforms[key] = {
                        loc = { new_loc.X, new_loc.Y, new_loc.Z },
                        rot = { new_rot.Pitch, new_rot.Yaw, new_rot.Roll },
                    }
                end
                moved_count = moved_count + 1
            elseif key then
                missing[#missing + 1] = key
            end
        end
    end

    is_applying_remote = false
    write_json_atomic(PATH_GAME_CMD_ACK, {
        cmd_id = cmd.cmd_id,
        action = "move_actors",
        ok = true,
        moved_count = moved_count,
        missing_count = #missing,
        missing_keys = (#missing > 0) and missing or { _json_empty_array = true },
        wall_ts = os.time(),
    })
end

local function start_reload_layout(cmd, action_label)
    local ctrl, gi = get_controller(), get_xiv_instance()
    if not ctrl or not gi or not cmd.layout_path then
        write_json_atomic(PATH_GAME_CMD_ACK, {
            cmd_id = cmd.cmd_id,
            action = action_label or "reload_layout",
            ok = false,
            error = "Missing controller, XivInstance, or layout_path",
            wall_ts = os.time(),
        })
        return
    end

    pending_commit_reason = nil
    pending_commit_at_ms = nil
    is_applying_remote = true
    pending_remote_reload = true
    pending_remote_reload_cmd_id = cmd.cmd_id
    remote_suppress_until_ms = now_ms + 4500
    pending_reload_deadline_ms = now_ms + 4000
    pending_orig_save_path = safe_call(function() return gi.SavePath:ToString() end) or ""
    local main_ui = safe_call(function() return ctrl.MainUI end)
    local save_box = is_valid_uobj(main_ui) and safe_call(function() return main_ui.SaveNameBox end) or nil
    pending_orig_save_name = is_valid_uobj(save_box) and safe_call(function() return save_box:GetText():ToString() end) or nil

    local ok, err = pcall(function()
        ensure_file_download_property()
        local house_data_class = StaticFindObject("/Script/MakePlace.HouseData")
        if not is_valid_uobj(house_data_class) then
            error("UHouseData class not found")
        end
        local hd = StaticConstructObject(house_data_class, ctrl)
        if not is_valid_uobj(hd) then
            error("StaticConstructObject(UHouseData) returned invalid object")
        end
        local house_dl = hd.House
        if not is_valid_uobj(house_dl) then
            error("UHouseData.House is invalid")
        end
        house_dl.BridgeFilePath = tostring(cmd.layout_path)
        hd:LoadHouse()
    end)
    if not ok then
        safe_call(function() gi.SavePath = pending_orig_save_path end)
        pending_orig_save_name = nil
        pending_remote_reload = false
        is_applying_remote = false
        pending_remote_reload_cmd_id = 0
        write_json_atomic(PATH_GAME_CMD_ACK, {
            cmd_id = cmd.cmd_id,
            action = action_label or "reload_layout",
            ok = false,
            error = tostring(err),
            wall_ts = os.time(),
        })
    end
end

local function apply_dye_actor(cmd)
    local ctrl = get_controller()
    if ctrl and cmd.actor_key and cmd.color_hex then
        local actor_map = build_actor_map()
        local actor = actor_map[cmd.actor_key]
        local tm = safe_call(function() return ctrl.TargetManager end)
        local main_ui = safe_call(function() return ctrl.MainUI end)
        local xiv_option = main_ui and safe_call(function() return main_ui.XivOption end)
        if is_valid_uobj(actor) and is_valid_uobj(tm) and is_valid_uobj(xiv_option) then
            is_applying_remote = true
            remote_suppress_until_ms = now_ms + 900
            local ok = pcall(function()
                tm:Select(actor)
                xiv_option:SetColorCode(tostring(cmd.color_hex))
            end)
            is_applying_remote = false
            if ok then
                write_json_atomic(PATH_GAME_CMD_ACK, {
                    cmd_id = cmd.cmd_id,
                    action = "dye_actor",
                    ok = true,
                    used_reload = false,
                    wall_ts = os.time(),
                })
                return
            end
        end
    end

    if cmd.fallback_layout_path then
        cmd.layout_path = cmd.fallback_layout_path
        start_reload_layout(cmd, "dye_actor_reload")
        return
    end

    write_json_atomic(PATH_GAME_CMD_ACK, {
        cmd_id = cmd.cmd_id,
        action = "dye_actor",
        ok = false,
        error = "Actor or XivOption unavailable and no fallback_layout_path",
        wall_ts = os.time(),
    })
end

local function process_inbound_command()
    local raw = read_text_file(PATH_GAME_CMD)
    if not raw or #raw == 0 then
        return
    end
    local cmd, err = json.decode(raw)
    if not cmd then
        log_error_once("decode_cmd", tostring(err))
        return
    end
    local session_id = (type(cmd.session_id) == "string") and cmd.session_id or ""
    if session_id ~= last_processed_session_id then
        last_processed_session_id = session_id
        last_processed_cmd_id = 0
    end
    local cmd_id = tonumber(cmd.cmd_id) or 0
    if cmd_id <= last_processed_cmd_id then
        return
    end
    last_processed_cmd_id = cmd_id

    if cmd.action == "move_actors" then
        apply_move_actors(cmd)
    elseif cmd.action == "dye_actor" then
        apply_dye_actor(cmd)
    elseif cmd.action == "reload_layout" then
        start_reload_layout(cmd, "reload_layout")
    elseif cmd.action == "request_actors" then
        local count = write_actors_sync("request_actors")
        write_json_atomic(PATH_GAME_CMD_ACK, {
            cmd_id = cmd_id,
            action = "request_actors",
            ok = true,
            actor_count = count,
            wall_ts = os.time(),
        })
    elseif cmd.action == "dump_snapshot" then
        local ok = dump_layout_snapshot("manual_dump", true)
        write_json_atomic(PATH_GAME_CMD_ACK, {
            cmd_id = cmd_id,
            action = "dump_snapshot",
            ok = ok,
            snapshot_path = PATH_SNAPSHOT,
            wall_ts = os.time(),
        })
    else
        write_json_atomic(PATH_GAME_CMD_ACK, {
            cmd_id = cmd_id,
            action = tostring(cmd.action),
            ok = false,
            error = "Unknown action",
            wall_ts = os.time(),
        })
    end
end

local function poll_active_drag()
    if is_applying_remote or is_dumping_snapshot or now_ms < remote_suppress_until_ms then
        return
    end
    local ctrl, _, is_outside, house_size, light_level = read_scene_meta()
    if not ctrl then
        return
    end
    local tm = safe_call(function() return ctrl.TargetManager end)
    if not is_valid_uobj(tm) then
        return
    end
    local targets = safe_call(function() return tm.Targets end)
    if not targets then
        return
    end

    local changed = {}
    local active_keys = {}
    for_each_uarray(targets, function(t)
        if is_furniture_actor(t) then
            local key = safe_call(function() return t:GetFullName() end)
            local xf = key and read_actor_transform(t)
            if key and xf then
                active_keys[key] = true
                local prev = last_seen_target_transforms[key]
                last_seen_target_transforms[key] = xf
                if prev then
                    local dx = math.abs(xf.loc[1] - prev.loc[1])
                    local dy = math.abs(xf.loc[2] - prev.loc[2])
                    local dz = math.abs(xf.loc[3] - prev.loc[3])
                    local dyaw = math.abs(xf.rot[2] - prev.rot[2])
                    if (dx + dy + dz) > 0.05 or dyaw > 0.05 then
                        local item_name = safe_call(function() return t:GetItemName():ToString() end) or ""
                        changed[#changed + 1] = {
                            actor_key = key,
                            item_name = item_name,
                            loc = xf.loc,
                            rot = xf.rot,
                        }
                    end
                end
            end
        end
    end)

    for k, _ in pairs(last_seen_target_transforms) do
        if not active_keys[k] then
            last_seen_target_transforms[k] = nil
        end
    end

    if #changed > 0 then
        local sig = json.encode(changed)
        if sig ~= last_drag_payload_sig then
            last_drag_payload_sig = sig
            drag_seq = drag_seq + 1
            write_json_atomic(PATH_DRAG, {
                seq = drag_seq,
                wall_ts = os.time(),
                is_outside = is_outside,
                house_size = house_size,
                light_level = light_level,
                actors = changed,
            })
        end
    end
end

-- Hook definitions
local HOOK_SPECS = {
    {
        id = "finalize_change",
        path = "/Script/MakePlaceCore.TargetManagerBase:FinalizeChange",
        fn = function()
            schedule_commit("finalize_change", 80)
        end,
    },
    {
        id = "update_target",
        path = "/Script/MakePlace.TargetManager:UpdateTarget",
        fn = function()
            schedule_commit("update_target", 120)
        end,
    },
    {
        id = "update_target_properties",
        path = "/Script/MakePlace.TargetManager:UpdateTargetProperties",
        fn = function()
            schedule_commit("update_props", 120)
        end,
    },
    {
        id = "copy_target",
        path = "/Script/MakePlace.TargetManager:CopyTarget",
        fn = function()
            schedule_commit("copy", 120)
        end,
    },
    {
        id = "delete_target",
        path = "/Script/MakePlace.TargetManager:DeleteTarget",
        fn = function()
            schedule_commit("delete", 120)
        end,
    },
    {
        id = "flip_active_target",
        path = "/Script/MakePlace.TargetManager:FlipActiveTarget",
        fn = function()
            schedule_commit("flip", 100)
        end,
    },
    {
        id = "mirror_group_around_active",
        path = "/Script/MakePlace.TargetManager:MirrorGroupAroundActive",
        fn = function()
            schedule_commit("mirror", 100)
        end,
    },
    {
        id = "undo",
        path = "/Script/MakePlaceCore.TargetManagerBase:Undo",
        fn = function()
            schedule_commit("undo", 120)
        end,
    },
    {
        id = "redo",
        path = "/Script/MakePlaceCore.TargetManagerBase:Redo",
        fn = function()
            schedule_commit("redo", 120)
        end,
    },
    {
        id = "set_exterior_wall",
        path = "/Script/MakePlace.HouseManager:SetExteriorWall",
        fn = function()
            schedule_commit("exterior_wall", 200)
        end,
    },
    {
        id = "set_layout",
        path = "/Script/MakePlace.HouseManager:SetLayout",
        fn = function()
            schedule_commit("house_layout", 200)
        end,
    },
    {
        id = "set_light_level",
        path = "/Script/MakePlace.XivController:SetLightLevel",
        fn = function()
            schedule_commit("light_level", 150)
        end,
    },
    {
        id = "post_level_load",
        path = "/Script/MakePlace.PlaceControllerBase:PostLevelLoad",
        fn = function()
            last_seen_target_transforms = {}
            if pending_remote_reload then
                -- Let PostLevelLoad finish rebuilding scene actors on this tick before syncing.
                pending_reload_deadline_ms = now_ms + 120
            else
                ExecuteWithDelay(120, function()
                    ExecuteInGameThread(function()
                        if is_applying_remote or pending_remote_reload or now_ms < remote_suppress_until_ms then
                            return
                        end
                        local actors = collect_furniture_actors()
                        write_actors_sync("level_loaded")
                        if #actors > 0 then
                            schedule_commit("level_loaded", 200)
                        end
                    end)
                end)
            end
        end,
    },
}

local registered_hooks = {}
local registered_hook_count = 0

local function try_register_hooks()
    local total = #HOOK_SPECS
    if registered_hook_count >= total then
        return
    end
    for _, spec in ipairs(HOOK_SPECS) do
        if not registered_hooks[spec.id] then
            local ok = pcall(function()
                RegisterHook(spec.path, function()
                    local h_ok, h_err = pcall(spec.fn)
                    if not h_ok then
                        log_error_once("hook_" .. spec.id, tostring(h_err))
                    end
                end)
            end)
            if ok then
                registered_hooks[spec.id] = true
                registered_hook_count = registered_hook_count + 1
                log_info("Registered hook: " .. spec.path)
            end
        end
    end
end

local function write_heartbeat()
    local _, _, is_outside, house_size, light_level = read_scene_meta()
    local list = safe_call(function() return FindAllOf("Furniture") end)
    local actor_count = (list and #list) or 0
    write_json_atomic(PATH_HEARTBEAT, {
        wall_ts = os.time(),
        uptime_ms = now_ms,
        is_outside = is_outside,
        house_size = house_size,
        light_level = light_level,
        actor_count = actor_count,
        hooks_registered = registered_hook_count,
        hooks_total = #HOOK_SPECS,
    })
end

local logged_first_tick = false

local function game_tick()
    now_ms = now_ms + 40
    if not logged_first_tick then
        logged_first_tick = true
        log_info("First game_tick executing on GameThread")
    end

    if registered_hook_count < #HOOK_SPECS and (now_ms - last_hook_retry_ms) >= 1000 then
        last_hook_retry_ms = now_ms
        try_register_hooks()
    end

    if pending_remote_reload and now_ms >= pending_reload_deadline_ms then
        finish_remote_reload("remote_reload_timeout_fallback")
    end

    process_inbound_command()

    if pending_commit_at_ms ~= nil and now_ms >= pending_commit_at_ms then
        local reason = pending_commit_reason or "commit"
        pending_commit_reason = nil
        pending_commit_at_ms = nil
        dump_layout_snapshot(reason)
    end

    poll_active_drag()

    if (now_ms - last_heartbeat_ms) >= 1000 then
        last_heartbeat_ms = now_ms
        write_heartbeat()
    end
end

log_info("FoundationBridge starting (IPC_DIR=" .. IPC_DIR .. ")")

ExecuteInGameThread(function()
    local cp_ok, cp_err = pcall(ensure_file_download_property)
    if not cp_ok then
        log_error_once("init_custom_prop", tostring(cp_err))
    end
    local h_ok, h_err = pcall(try_register_hooks)
    if not h_ok then
        log_error_once("init_hooks", tostring(h_err))
    end
    local hb_ok, hb_err = pcall(write_heartbeat)
    if not hb_ok then
        log_error_once("init_heartbeat", tostring(hb_err))
    end
end)

_G.FOUNDATION_BRIDGE_LOOP_HANDLE = LoopInGameThreadWithDelay(40, function()
    local ok, err = pcall(game_tick)
    if not ok then
        log_error_once("game_tick", tostring(err))
    end
    return false
end)
