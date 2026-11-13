-- cli-token-sync: every interval_minutes, refresh the mac's logins of OAuth
-- CLIs when they are close to expiry, and copy their access tokens to every
-- devbox that has a workspace open, so a box never needs its own login.
--
-- Each CLI is a source, lua/sources/<name>.lua: name, title, login (the
-- command that logs in), entry (the token fields a box gets), label(key),
-- mac_input(config, window, introspect), and two Python scripts:
--   MAC_PY   runs on the mac with mac_input on stdin. It refreshes a token
--            with refresh_minutes or less left, the way the CLI itself does,
--            writes it back where the CLI keeps it, and prints a report:
--            { missing = true } or { servers = [ row ] }, a row per login
--            (key, expires_at, entry, refreshed, error, login_active,
--            login_expires_at, introspect_error).
--   MERGE_PY runs on each box with { servers = [ { key, entry } ] } on stdin,
--            writes the tokens where the CLI looks for them there, and prints
--            "updated <key>" for each one it changed.
-- Refresh tokens never leave the mac: both CLIs' providers rotate them, so a
-- second user of the mac's chain would log the mac out. A refresh token a box
-- got from its own login stays, so that box carries on while the mac sleeps.
-- A devbox workspace that opens is pushed to within 10 s, not at the next pass.
--
-- Token values never reach a log, a toast or the dashboard.

local M = {}

M.sources = {
  shipyard = require("sources.shipyard"),
  buildkite = require("sources.buildkite"),
}

local MIN_LEFT = 60 -- s: a token closer to expiry than this stays home

-- The distinct devbox hosts of `workspaces` (kitchen.workspaces()), in
-- first-seen order, and host → list of workspace ids (for per-workspace
-- dashboard stats). "local" workspaces are skipped.
function M.hosts(workspaces)
  local hosts, ws_of = {}, {}
  for _, ws in ipairs(workspaces or {}) do
    local h = ws.host
    if h and h ~= "local" then
      if not ws_of[h] then
        ws_of[h] = {}
        table.insert(hosts, h)
      end
      table.insert(ws_of[h], ws.id)
    end
  end
  return hosts, ws_of
end

-- Whether `row`'s token (a MAC_PY report row) is worth sending at `now`: an
-- access token with more than MIN_LEFT seconds left, or no expiry.
function M.usable(row, now)
  local e = row.entry or {}
  local exp = tonumber(e.expires_at) or 0
  return type(e.access_token) == "string" and e.access_token ~= "" and (exp == 0 or exp - now > MIN_LEFT)
end

-- MERGE_PY's stdin for `src`, as a table to kitchen.json.encode: { servers =
-- { { key, entry }, ... } } for the usable rows of `report`. The entry is
-- rebuilt from src.entry, so nothing else (a refresh token) can leave the mac.
function M.payload(src, report, now)
  local list = {}
  for _, row in ipairs(type(report) == "table" and report.servers or {}) do
    if M.usable(row, now) then
      local entry = {}
      for _, k in ipairs(src.entry) do
        entry[k] = row.entry[k]
      end
      table.insert(list, { key = row.key, entry = entry })
    end
  end
  return { servers = list }
end

-- What to tell William about `row` of `src` at `now`, given when its login
-- ends (`login_exp`, unix s or nil) and `warn_days`: nil, or a message. Only
-- problems he has to fix: a failed refresh of a token that still works is
-- retried next pass, so it is logged, not shown.
function M.problem(src, row, login_exp, now, warn_days)
  local label, how = src.label(row.key), "run " .. src.login .. " on the mac"
  if row.login_active == false then
    return label .. ": the mac's " .. src.title .. " login has ended; " .. how
  end
  if row.error and not M.usable(row, now) then
    return label .. ": the mac's " .. src.title .. " token expired and refreshing failed (" .. row.error .. "); " .. how
  end
  if login_exp and login_exp - now <= warn_days * 86400 then
    return label .. ": the mac's " .. src.title .. " login ends " .. os.date("!%Y-%m-%d %H:%M UTC", login_exp) .. "; " .. how .. " before then"
  end
  return nil
end

-- A failed job's reason, for logs and toasts: stderr only, one line, at most
-- 200 characters. Never stdout: MAC_PY prints the tokens there.
local function why(res)
  if res.timed_out then
    return "timed out"
  end
  local text = (res.stderr or ""):gsub("%s+", " "):match("^%s*(.-)%s*$")
  if text == "" then
    text = "exit " .. tostring(res.code)
  end
  return text:sub(1, 200)
end

local warned = {} -- key → the message toasted while that problem stands

-- Toast `msg` once per `key` (again if the message changes); warn(key, nil)
-- clears it. 5-minute ticks would repeat it otherwise.
local function warn(key, msg)
  if msg == nil then
    warned[key] = nil
  elseif warned[key] ~= msg then
    warned[key] = msg
    kitchen.log(msg)
    kitchen.notify("cli-token-sync: " .. msg, "warn")
  end
end

local login_exp = {} -- "<source>|<key>" → when the mac's login ends (unix s)
local introspected = {} -- source name → this load has asked when its logins end

-- `src`'s MAC_PY report, decoded. Errors raise with why(res) or a fixed
-- text, never the report's contents.
local function mac_report(src, window)
  local stdin = kitchen.json.encode(src.mac_input(kitchen.config, window, not introspected[src.name]))
  local res = kitchen.run_sync({ host = "local", timeout = 90, cmd = { "python3", "-c", src.MAC_PY }, stdin = stdin })
  if res.code ~= 0 then
    error("mac: " .. why(res), 0)
  end
  local ok, report = pcall(kitchen.json.decode, res.stdout)
  if not ok or type(report) ~= "table" then
    error("mac: the report is not JSON", 0)
  end
  introspected[src.name] = true
  return report
end

-- Push `stdin` (M.payload, encoded) of `src` to `host`; log what it updated.
local function push(src, host, stdin)
  local res = kitchen.run_sync({ host = host, timeout = 60, cmd = { "python3", "-c", src.MERGE_PY }, stdin = stdin })
  if res.code ~= 0 then
    error(src.name .. " on " .. host .. ": " .. why(res), 0)
  end
  for key in (res.stdout or ""):gmatch("updated ([^\r\n]+)") do
    kitchen.log("[" .. host .. "]", "updated", src.label(key))
  end
end

-- The sources named in config.sources, in order; an unknown name is warned
-- about and skipped.
local function enabled()
  local list = {}
  for _, name in ipairs(type(kitchen.config.sources) == "table" and kitchen.config.sources or {}) do
    local src = M.sources[name]
    warn("source:" .. tostring(name), not src and ("no source named " .. tostring(name)) or nil)
    if src then
      table.insert(list, src)
    end
  end
  return list
end

-- The mac side of `src` for one pass: refresh, warnings, stats. Returns the
-- payload to push.
local function mac_side(src, window, warn_days, stats)
  local ok, report = pcall(mac_report, src, window)
  local now = os.time()
  if not ok then
    warn(src.name .. ":mac", src.title .. ": " .. tostring(report))
    return { servers = {} }
  end
  if report.missing then
    warn(src.name .. ":mac", src.title .. ": no login on this mac; run " .. src.login)
    return { servers = {} }
  end
  warn(src.name .. ":mac", nil)
  for _, row in ipairs(report.servers or {}) do
    local label, id = src.label(row.key), src.name .. "|" .. tostring(row.key)
    if row.refreshed then
      kitchen.log("refreshed", label)
    end
    if row.error then
      kitchen.log("refreshing", label, "failed:", row.error)
    end
    if row.introspect_error then
      kitchen.log("introspecting", label, "failed:", row.introspect_error)
    end
    if row.login_expires_at then
      login_exp[id] = tonumber(row.login_expires_at)
    end
    warn("login:" .. id, M.problem(src, row, login_exp[id], now, warn_days))
    local exp = tonumber(row.expires_at) or 0
    local key = src.name .. "_minutes_left"
    if M.usable(row, now) and exp > 0 and (not stats[key] or (exp - now) / 60 < stats[key]) then
      stats[key] = math.floor((exp - now) / 60)
    end
    local ends, days = login_exp[id], src.name .. "_login_days_left"
    if ends and (not stats[days] or (ends - now) / 86400 < stats[days]) then
      stats[days] = math.floor((ends - now) / 8640 + 0.5) / 10
    end
  end
  return M.payload(src, report, now)
end

local manual = false -- the next pass was asked for with cli-token-sync.run
local tried = {} -- host → true once a pass of this load has started with it

-- One pass (see the header). Returns the dashboard stats and per-workspace
-- stats { [ws_id] = { pushed = <sources> } or { failed = "…" } }.
local function pass()
  local was_manual = manual
  manual = false
  for _, host in ipairs((M.hosts(kitchen.workspaces()))) do
    tried[host] = true
  end

  local window = math.max(1, tonumber(kitchen.config.refresh_minutes) or 15) * 60
  local warn_days = tonumber(kitchen.config.warn_days) or 7
  local hosts, ws_of = M.hosts(kitchen.workspaces())
  local stats, pushed, failed = { hosts = #hosts, pushed = 0, failed = 0 }, {}, {}
  for _, src in ipairs(enabled()) do
    local payload = mac_side(src, window, warn_days, stats)
    if #payload.servers > 0 then
      local stdin = kitchen.json.encode(payload)
      for _, host in ipairs(hosts) do
        local ok, err = pcall(push, src, host, stdin)
        if ok then
          stats.pushed = stats.pushed + 1
          pushed[host] = (pushed[host] or 0) + 1
        else
          stats.failed = stats.failed + 1
          kitchen.log("[" .. host .. "]", err)
          failed[host] = tostring(err):sub(1, 120)
        end
      end
    end
  end

  local per_ws = {}
  for _, host in ipairs(hosts) do
    local row = failed[host] and { failed = failed[host] } or (pushed[host] and { pushed = pushed[host] }) or nil
    for _, id in ipairs(ws_of[host]) do
      per_ws[id] = row
    end
  end
  if was_manual then
    kitchen.notify("cli-token-sync: " .. stats.pushed .. " push(es) to " .. #hosts .. " host(s), " .. stats.failed .. " failed")
  end
  return stats, per_ws
end

-- The kitchen runs it on schedule and never starts one while another runs.
local timer = kitchen.every("pass", math.max(1, tonumber(kitchen.config.interval_minutes) or 5) * 60, pass, {
  desc = "refresh the mac's CLI logins and push their access tokens to every devbox",
  first_delay = 40,
})

-- A newly opened devbox workspace gets the tokens within NEW_HOST_POLL
-- seconds, not at the next pass. Each host counts once per load (so a box
-- that can't be reached doesn't start a pass every tick), and a reload
-- counts every host as new.
local NEW_HOST_POLL = 10
kitchen.every("new-hosts", NEW_HOST_POLL, function()
  for _, host in ipairs((M.hosts(kitchen.workspaces()))) do
    if not tried[host] then
      timer:run() -- false while a pass runs; then the next tick asks again
      return { new_host = host }
    end
  end
  return {}
end, { desc = "start a pass when a devbox workspace opens", first_delay = 55 }) -- after the first pass's 40 s

kitchen.command("run", function()
  manual = true
  if not timer:run() then
    manual = false
    kitchen.notify("cli-token-sync: a pass is already running")
  end
end, { desc = "refresh the mac's CLI logins and push them to the devboxes now" })

return M
