local OUTPUT_DIR = "E:/FF14/Repos/PrivateTest/rmp-bridge/dumps"
local REFLECTION_FILE = OUTPUT_DIR .. "/makeplace_reflection.txt"
local DONE_FLAG = OUTPUT_DIR .. "/dump_done.flag"

local function safe_call(fn, ...)
    local ok, res = pcall(fn, ...)
    if ok then
        return res
    end
    return nil
end

local function write_line(lines, s)
    lines[#lines + 1] = s
end

local function is_target_name(full_name)
    if not full_name then return false end
    return full_name:find("/Script/MakePlace", 1, true) ~= nil
        or full_name:find("/Game/Blueprints/", 1, true) ~= nil
        or full_name:find("PlaceController", 1, true) ~= nil
        or full_name:find("PlaceGameMode", 1, true) ~= nil
        or full_name:find("PlacePawn", 1, true) ~= nil
        or full_name:find("XivUtil", 1, true) ~= nil
        or full_name:find("HouseLight", 1, true) ~= nil
        or full_name:find("SaveStruct", 1, true) ~= nil
        or full_name:find("SaveProperty", 1, true) ~= nil
end

local function describe_property(prop)
    local offset = safe_call(function() return prop:GetOffset_Internal() end) or -1
    local size = safe_call(function() return prop:GetSize() end) or -1
    local pclass = safe_call(function() return prop:GetClass():GetFName():ToString() end) or "UnknownProp"
    local pname = safe_call(function() return prop:GetFName():ToString() end) or "?"
    local extra = ""

    if PropertyTypes then
        if PropertyTypes.ObjectProperty and safe_call(function() return prop:IsA(PropertyTypes.ObjectProperty) end) then
            local cls = safe_call(function() return prop:GetPropertyClass():GetFullName() end)
            if cls then extra = " -> " .. cls end
        elseif PropertyTypes.ClassProperty and safe_call(function() return prop:IsA(PropertyTypes.ClassProperty) end) then
            local cls = safe_call(function() return prop:GetMetaClass():GetFullName() end)
            if cls then extra = " -> class<" .. cls .. ">" end
        elseif PropertyTypes.StructProperty and safe_call(function() return prop:IsA(PropertyTypes.StructProperty) end) then
            local st = safe_call(function() return prop:GetStruct():GetFullName() end)
            if st then extra = " -> " .. st end
        elseif PropertyTypes.EnumProperty and safe_call(function() return prop:IsA(PropertyTypes.EnumProperty) end) then
            local en = safe_call(function() return prop:GetEnum():GetFullName() end)
            if en then extra = " -> " .. en end
        elseif PropertyTypes.ByteProperty and safe_call(function() return prop:IsA(PropertyTypes.ByteProperty) end) then
            local en = safe_call(function() return prop:GetEnum():GetFullName() end)
            if en then extra = " -> " .. en end
        elseif PropertyTypes.ArrayProperty and safe_call(function() return prop:IsA(PropertyTypes.ArrayProperty) end) then
            local inner = safe_call(function() return prop:GetInner() end)
            if inner then
                local ic = safe_call(function() return inner:GetClass():GetFName():ToString() end) or "?"
                local st = safe_call(function() return inner:GetStruct():GetFullName() end)
                local oc = safe_call(function() return inner:GetPropertyClass():GetFullName() end)
                extra = " -> Array<" .. (st or oc or ic) .. ">"
            end
        elseif PropertyTypes.MapProperty and safe_call(function() return prop:IsA(PropertyTypes.MapProperty) end) then
            local kp = safe_call(function() return prop:GetKeyProp() end)
            local vp = safe_call(function() return prop:GetValueProp() end)
            local kname = kp and (safe_call(function() return kp:GetStruct():GetFullName() end) or safe_call(function() return kp:GetClass():GetFName():ToString() end)) or "?"
            local vname = vp and (safe_call(function() return vp:GetStruct():GetFullName() end) or safe_call(function() return vp:GetPropertyClass():GetFullName() end) or safe_call(function() return vp:GetClass():GetFName():ToString() end)) or "?"
            extra = " -> Map<" .. kname .. ", " .. vname .. ">"
        elseif PropertyTypes.BoolProperty and safe_call(function() return prop:IsA(PropertyTypes.BoolProperty) end) then
            local fm = safe_call(function() return prop:GetFieldMask() end)
            if fm and fm ~= 255 then
                extra = string.format(" (FieldMask=0x%X)", fm)
            end
        end
    end

    return string.format("  [+0x%04X sz=%3d] %-18s %s%s", offset, size, pclass, pname, extra)
end

local function dump_struct_or_class(lines, obj, is_enum, is_script_struct)
    local full_name = safe_call(function() return obj:GetFullName() end) or "?"
    if is_enum then
        write_line(lines, "ENUM " .. full_name)
        safe_call(function()
            obj:ForEachName(function(name, val)
                write_line(lines, string.format("  %s = %s", name:ToString(), tostring(val)))
            end)
        end)
        write_line(lines, "")
        return
    end

    local super = safe_call(function() return obj:GetSuperStruct() end)
    local super_name = (super and safe_call(function() return super:GetFullName() end)) or "None"
    local struct_size = safe_call(function() return obj:GetPropertiesSize() end) or -1
    local kind = is_script_struct and "STRUCT" or "CLASS"
    write_line(lines, string.format("%s %s : %s (PropertiesSize=%d)", kind, full_name, super_name, struct_size))

    local props = {}
    safe_call(function()
        obj:ForEachProperty(function(prop)
            props[#props + 1] = describe_property(prop)
        end)
    end)
    if #props > 0 then
        write_line(lines, "  -- Properties --")
        for _, p in ipairs(props) do
            write_line(lines, p)
        end
    end

    if not is_script_struct and obj.ForEachFunction then
        local funcs = {}
        safe_call(function()
            obj:ForEachFunction(function(fn)
                local fname = safe_call(function() return fn:GetFName():ToString() end) or "?"
                local fflags = safe_call(function() return fn:GetFunctionFlags() end) or 0
                local params = {}
                safe_call(function()
                    fn:ForEachProperty(function(p)
                        params[#params + 1] = describe_property(p):gsub("^%s+", "")
                    end)
                end)
                funcs[#funcs + 1] = string.format("  FN %s (Flags=0x%X)", fname, fflags)
                for _, pm in ipairs(params) do
                    funcs[#funcs + 1] = "    " .. pm
                end
            end)
        end)
        if #funcs > 0 then
            write_line(lines, "  -- Functions --")
            for _, f in ipairs(funcs) do
                write_line(lines, f)
            end
        end
    end
    write_line(lines, "")
end

local function dump_instances(lines)
    write_line(lines, "=== ACTIVE RUNTIME INSTANCES ===")
    local probes = {
        "PlaceControllerBase",
        "PlaceController_C",
        "TargetManagerBase",
        "TargetManager",
        "HouseManager",
        "HouseFixture",
        "PlacePawn",
        "PlacePawnBP_C",
        "XivUtil_C",
        "Furniture",
        "MoveGizmo",
        "Gizmo",
        "Selectable",
        "MainUIBase",
    }
    for _, cname in ipairs(probes) do
        local list = safe_call(function() return FindAllOf(cname) end)
        if list and #list > 0 then
            write_line(lines, string.format("%s (%d):", cname, #list))
            for i = 1, math.min(#list, 10) do
                local inst = list[i]
                local fn = safe_call(function() return inst:GetFullName() end) or "?"
                write_line(lines, "  [" .. i .. "] " .. fn)
            end
        else
            write_line(lines, string.format("%s (0)", cname))
        end
    end
    write_line(lines, "")
end

local function run_dump()
    print("[FoundationDumper] Starting UE 5.3 reflection & SDK dump...\n")

    local sdk_ok, sdk_err = pcall(function()
        if GenerateSDK then
            GenerateSDK()
        end
    end)
    print(string.format("[FoundationDumper] GenerateSDK ok=%s err=%s\n", tostring(sdk_ok), tostring(sdk_err)))

    local UClassClass = StaticFindObject("/Script/CoreUObject.Class")
    local UStructClass = StaticFindObject("/Script/CoreUObject.ScriptStruct")
    local UEnumClass = StaticFindObject("/Script/CoreUObject.Enum")

    local entries = {}
    local visited = {}
    safe_call(function()
        ForEachUObject(function(obj)
            local full = safe_call(function() return obj:GetFullName() end)
            if full and not visited[full] and is_target_name(full) then
                local is_cls = UClassClass and safe_call(function() return obj:IsA(UClassClass) end)
                local is_st = UStructClass and safe_call(function() return obj:IsA(UStructClass) end)
                local is_en = UEnumClass and safe_call(function() return obj:IsA(UEnumClass) end)
                if is_cls or is_st or is_en then
                    visited[full] = true
                    entries[#entries + 1] = {
                        full = full,
                        obj = obj,
                        is_enum = is_en == true,
                        is_struct = (is_st == true) and not is_cls,
                    }
                end
            end
        end)
    end)

    table.sort(entries, function(a, b) return a.full < b.full end)

    local lines = {}
    write_line(lines, "=== ReMakePlace UE 5.3 Reflection Dump ===")
    write_line(lines, string.format("Matched types: %d", #entries))
    write_line(lines, "")

    for _, e in ipairs(entries) do
        dump_struct_or_class(lines, e.obj, e.is_enum, e.is_struct)
    end

    dump_instances(lines)

    local f, err = io.open(REFLECTION_FILE, "w+")
    if f then
        f:write(table.concat(lines, "\n"))
        f:close()
        print("[FoundationDumper] Wrote " .. REFLECTION_FILE .. " (" .. tostring(#lines) .. " lines)\n")
    else
        print("[FoundationDumper] Failed writing reflection file: " .. tostring(err) .. "\n")
    end

    local df, derr = io.open(DONE_FLAG, "w+")
    if df then
        df:write("done\n")
        df:close()
        print("[FoundationDumper] Wrote " .. DONE_FLAG .. "\n")
    else
        print("[FoundationDumper] Failed writing done flag: " .. tostring(derr) .. "\n")
    end
end

print("[FoundationDumper] Loaded, scheduling dump in 6000ms...\n")
ExecuteWithDelay(6000, function()
    ExecuteInGameThread(function()
        run_dump()
    end)
end)
