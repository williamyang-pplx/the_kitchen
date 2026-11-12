-- mcp-token-sync: every interval_minutes, copy the mac's MCP OAuth tokens to
-- every devbox that has a workspace open.
--
-- Claude Code on a devbox can't finish an MCP OAuth login (the callback goes
-- to the mac's localhost). So the mac stays the only place that logs in and
-- refreshes, and the boxes get copies.
--
-- A pass, for each server in `servers`:
--   1. Read Claude's keychain item on the mac ("Claude Code-credentials").
--   2. When the token has 5 minutes or less left (Claude's own refresh
--      window), run `claude mcp get <server>` on the mac: Claude uses the
--      token and refreshes it with its own logic and lock. Then read again.
--      The plugin never refreshes a token itself: Slack's refresh tokens are
--      single-use, so a second refresher can log the mac out.
--   3. Drop refreshToken (and clientSecret) from the entry. A box that could
--      refresh would revoke the mac's chain.
-- Then, for each devbox host, one ssh call sends every entry on stdin to a
-- Python script that merges it into ~/.claude/.credentials.json (matched by
-- serverName, written to a temp file and renamed, mode 600). It writes only
-- when something changed, so Claude's mtime watch fires only on a real
-- change. It also clears the server from Claude's needs-auth cache there,
-- which would otherwise keep new sessions from trying the token. After a
-- write, `claude mcp get <server>` on the box checks it (that command ignores
-- the cache). A devbox workspace that opens is pushed to within 10 s, not at
-- the next pass.
--
-- Token values never reach a log, a toast or the dashboard.

local M = {}

local SERVICE = "Claude Code-credentials" -- Claude Code's keychain item (service name)
local REFRESH_WINDOW = 300 -- s: Claude refreshes a token only this close to expiry
local STRIP = { refreshToken = true, clientSecret = true } -- never leave the mac

local function q(s)
  return "'" .. tostring(s):gsub("'", "'\\''") .. "'"
end

-- The mac's entry for each server in `names`, from the keychain JSON
-- `store` ({ mcpOAuth = { ["<name>|<hash>"] = entry } }), matched by
-- entry.serverName. Returns, in `names` order:
--   { name, key, entry = <copy without STRIP fields>, expires_at = <ms or nil> }
-- or { name, missing = true } when the mac has no entry with an accessToken.
-- (kitchen.json.decode drops JSON nulls, so the copy has no null fields.)
function M.mac_entries(store, names)
  local oauth = type(store) == "table" and type(store.mcpOAuth) == "table" and store.mcpOAuth or {}
  local out = {}
  for _, name in ipairs(names) do
    -- Several entries (stale config hashes): the one that lasts longest; a
    -- tie goes to the smaller key, so the choice doesn't depend on pairs().
    local key, best, best_exp
    for k, e in pairs(oauth) do
      if type(e) == "table" and e.serverName == name and type(e.accessToken) == "string" and e.accessToken ~= "" then
        local exp = tonumber(e.expiresAt) or -math.huge
        if not best or exp > best_exp or (exp == best_exp and k < key) then
          key, best, best_exp = k, e, exp
        end
      end
    end
    if best then
      local entry = {}
      for field, v in pairs(best) do
        if not STRIP[field] then
          entry[field] = v
        end
      end
      table.insert(out, { name = name, key = key, entry = entry, expires_at = tonumber(best.expiresAt) })
    else
      table.insert(out, { name = name, missing = true })
    end
  end
  return out
end

-- Seconds `e` (an M.mac_entries result) has left at unix time `now`, or nil
-- when it is missing or has no expiresAt (it never expires, as far as Claude knows).
function M.seconds_left(e, now)
  if e.missing or not e.expires_at then
    return nil
  end
  return e.expires_at / 1000 - now
end

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

-- MERGE_PY's stdin, as a table to kitchen.json.encode: { servers = { {
-- name, key, entry }, ... } } for the entries in `entries` that are not
-- missing and not expired at `now` (an expired token helps no box).
function M.payload(entries, now)
  local list = {}
  for _, e in ipairs(entries) do
    local left = M.seconds_left(e, now)
    if not e.missing and (left == nil or left > 0) then
      table.insert(list, { name = e.name, key = e.key, entry = e.entry })
    end
  end
  return { servers = list }
end

-- What `claude mcp get <name>` on a box said: "connected", "missing" (not
-- configured there) or the text of its Status line (e.g. "⚠ Needs
-- authentication"), "unknown" when there is none.
function M.box_status(code, stdout, stderr)
  stdout, stderr = stdout or "", stderr or ""
  if code ~= 0 and (stdout .. stderr):find("No MCP server named", 1, true) then
    return "missing"
  end
  local status = stdout:match("Status: ([^\r\n]*)")
  if not status then
    return "unknown"
  end
  return status:find("Connected", 1, true) and "connected" or status
end

-- Python, run on a box with M.payload's JSON on stdin, that merges each
-- entry into ~/.claude/.credentials.json:
--   * the file missing: start from {} (and create ~/.claude, mode 700);
--     invalid JSON, a top level or mcpOAuth that is not an object: exit 1
--     before writing (never clobber it)
--   * keep every other top-level key and every other server's entry
--   * the entry's target keys: every mcpOAuth key whose value is an object
--     with the same serverName (the box may hash a different config), else
--     the mac's key
--   * prints "updated <name>" for each server it changed, nothing for the rest
--   * when anything changed: a temp file in ~/.claude (mode 600), fsync,
--     os.replace over the file; the temp file goes if any step fails.
--     Nothing changed: the file (and its mtime) is left alone.
-- Then it drops each server from ~/.claude/mcp-needs-auth-cache.json
-- ({ "<server name>": { timestamp, ... } }), the same way, and prints
-- "cleared <name>". A claude run that found no token (box setup's `claude
-- mcp list`, before the first push) leaves an entry there, and for up to
-- 4 hours every new Claude session then skips the server without trying the
-- token. A cache that is missing or not a JSON object is left alone: Claude
-- reads it as empty. Only sessions started after this see the change; a
-- running session needs /mcp → reconnect.
-- A level-2 long bracket, so Python's `]]` can't end the Lua string.
M.MERGE_PY = [==[
import json, os, sys, tempfile

want = json.load(sys.stdin)["servers"]
d = os.path.expanduser("~/.claude")
path = os.path.join(d, ".credentials.json")
cache_path = os.path.join(d, "mcp-needs-auth-cache.json")

def write(path, obj):
    os.makedirs(d, 0o700, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=d, prefix=os.path.basename(path) + ".")
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w") as f:
            json.dump(obj, f, indent=2)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)

try:
    with open(path) as f:
        store = json.load(f)
except FileNotFoundError:
    store = {}
if not isinstance(store, dict):
    sys.exit("credentials: the top level is not an object")
oauth = store.setdefault("mcpOAuth", {})
if not isinstance(oauth, dict):
    sys.exit("credentials: mcpOAuth is not an object")

changed = False
for s in want:
    keys = [k for k, v in oauth.items() if isinstance(v, dict) and v.get("serverName") == s["name"]] or [s["key"]]
    stale = [k for k in keys if oauth.get(k) != s["entry"]]
    for k in stale:
        oauth[k] = s["entry"]
    if stale:
        changed = True
        print("updated", s["name"])

if changed:
    write(path, store)

try:
    with open(cache_path) as f:
        cache = json.load(f)
except (OSError, ValueError):
    cache = None
if isinstance(cache, dict):
    cleared = [s["name"] for s in want if s["name"] in cache]
    for name in cleared:
        del cache[name]
        print("cleared", name)
    if cleared:
        write(cache_path, cache)
]==]

-- run_sync with kitchen.config.path on the PATH: claude lives there on the
-- mac when the kitchen was started from Finder, and in ~/.local/bin on a box.
local function sh(spec)
  spec.sh = "export PATH=" .. (kitchen.config.path or "") .. ':"$PATH"; ' .. spec.sh
  return kitchen.run_sync(spec)
end

-- A failed job's reason, for logs and toasts: stderr only, one line, at most
-- 200 characters. Never stdout: the keychain read prints the secret there.
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

-- The keychain item, decoded. Errors (no item, locked keychain, bad JSON)
-- raise with why(res) or a fixed text, never the item's contents.
local function read_keychain()
  local res = kitchen.run_sync({ host = "local", timeout = 30, cmd = { "security", "find-generic-password", "-s", SERVICE, "-w" } })
  if res.code ~= 0 then
    error("keychain: " .. why(res), 0)
  end
  local ok, store = pcall(kitchen.json.decode, res.stdout)
  if not ok or type(store) ~= "table" then
    error("keychain entry is not JSON", 0)
  end
  return store
end

-- Let Claude refresh `name` on the mac: `claude mcp get` uses the token, and
-- Claude refreshes it when it is in its window. The result is read back from
-- the keychain, not from this command's output.
local function refresh_on_mac(name)
  kitchen.log("asked claude to refresh", name)
  local res = sh({ host = "local", timeout = 60, sh = "cd && claude mcp get " .. q(name) })
  if res.code ~= 0 then
    kitchen.log("claude mcp get", name, "failed:", why(res))
  end
end

local warned = {} -- key → true while that warning stands

-- Toast `msg` once per `key`; warn(key, nil) clears it. 5-minute ticks would
-- repeat it otherwise.
local function warn(key, msg)
  if msg == nil then
    warned[key] = nil
  elseif not warned[key] then
    warned[key] = true
    kitchen.log(msg)
    kitchen.notify("mcp-token-sync: " .. msg, "warn")
  end
end

-- `claude mcp get <name>` on `host`; warn when the server is not installed
-- there or does not say Connected, clear that warning when it does.
local function check_box(host, name)
  local res = sh({ host = host, timeout = 60, sh = "cd && claude mcp get " .. q(name) })
  local s = M.box_status(res.code, res.stdout, res.stderr)
  local key = "box:" .. host .. ":" .. name
  if s == "connected" then
    warn(key, nil)
    kitchen.log("[" .. host .. "]", "checked", name)
  elseif s == "missing" then
    warn(key, name .. " is not installed on " .. host .. " (dotfiles mcp-setup.sh)")
  else
    warn(key, name .. " on " .. host .. ": " .. (s == "unknown" and why(res) or s))
  end
end

-- Push `stdin` (M.payload, encoded) to `host`, then check there each server
-- it updated, and each server whose box warning stands (so a warning clears
-- once the problem is fixed, though the entry is unchanged).
local function push(host, stdin, names)
  local res = kitchen.run_sync({ host = host, timeout = 60, cmd = { "python3", "-c", M.MERGE_PY }, stdin = stdin })
  if res.code ~= 0 then
    error("merge on " .. host .. ": " .. why(res), 0)
  end
  local updated = {}
  for name in (res.stdout or ""):gmatch("updated ([^\r\n]+)") do
    updated[name] = true
    kitchen.log("[" .. host .. "]", "updated", name)
  end
  for name in (res.stdout or ""):gmatch("cleared ([^\r\n]+)") do
    kitchen.log("[" .. host .. "]", "cleared the needs-auth mark of", name)
  end
  for _, name in ipairs(names) do
    if updated[name] or warned["box:" .. host .. ":" .. name] then
      check_box(host, name)
    end
  end
end

local manual = false -- the next pass was asked for with mcp-token-sync.run
local tried = {} -- host → true once a pass of this load has started with it

-- One pass (see the header). Returns the dashboard stats and per-workspace
-- stats { [ws_id] = { pushed = 1 } or { failed = "…" } }.
local function pass()
  local was_manual = manual
  manual = false
  for _, host in ipairs((M.hosts(kitchen.workspaces()))) do
    tried[host] = true
  end
  local names = type(kitchen.config.servers) == "table" and kitchen.config.servers or {}
  if #names == 0 then
    return { servers = 0 }, {}
  end

  local entries, now = M.mac_entries(read_keychain(), names), os.time()
  local refreshed = false
  for _, e in ipairs(entries) do
    local left = M.seconds_left(e, now)
    if left and left <= REFRESH_WINDOW then
      refresh_on_mac(e.name)
      refreshed = true
    end
  end
  if refreshed then
    entries, now = M.mac_entries(read_keychain(), names), os.time()
  end
  for _, e in ipairs(entries) do
    local left = M.seconds_left(e, now)
    if e.missing then
      warn("mac:" .. e.name, e.name .. ": no MCP OAuth token on this mac; log in with /mcp in a Claude session here")
    elseif left and left <= 0 then
      warn("mac:" .. e.name, e.name .. ": the mac's token expired and Claude could not refresh it; log in again with /mcp")
    else
      warn("mac:" .. e.name, nil)
    end
  end

  local payload = M.payload(entries, now)
  local hosts, ws_of = M.hosts(kitchen.workspaces())
  local stats, per_ws = { servers = #payload.servers, hosts = #hosts, pushed = 0, failed = 0 }, {}
  local min_left
  for _, e in ipairs(entries) do
    local left = M.seconds_left(e, now)
    if left and left > 0 and (not min_left or left < min_left) then
      min_left = left
    end
  end
  if min_left then
    stats.hours_left = math.floor(min_left / 360 + 0.5) / 10
  end
  if #payload.servers > 0 then
    local stdin = kitchen.json.encode(payload)
    for _, host in ipairs(hosts) do
      local ok, err = pcall(push, host, stdin, names)
      local row
      if ok then
        stats.pushed = stats.pushed + 1
        row = { pushed = 1 }
      else
        stats.failed = stats.failed + 1
        kitchen.log("[" .. host .. "]", err)
        row = { failed = tostring(err):sub(1, 120) }
      end
      for _, id in ipairs(ws_of[host]) do
        per_ws[id] = row
      end
    end
  end
  if was_manual then
    kitchen.notify("mcp-token-sync: pushed to " .. stats.pushed .. " host(s), " .. stats.failed .. " failed")
  end
  return stats, per_ws
end

-- The kitchen runs it on schedule and never starts one while another runs.
local timer = kitchen.every("pass", math.max(1, tonumber(kitchen.config.interval_minutes) or 5) * 60, pass, {
  desc = "push the mac's MCP OAuth tokens (no refresh tokens) to every devbox",
  first_delay = 30,
})

-- A newly opened devbox workspace gets its tokens within NEW_HOST_POLL
-- seconds, not at the next pass: a tab opened there before them would come up
-- logged out. Each host counts once per load (so a box that can't be reached
-- doesn't start a pass every tick), and a reload counts every host as new.
local NEW_HOST_POLL = 10
kitchen.every("new-hosts", NEW_HOST_POLL, function()
  for _, host in ipairs((M.hosts(kitchen.workspaces()))) do
    if not tried[host] then
      timer:run() -- false while a pass runs; then the next tick asks again
      return { new_host = host }
    end
  end
  return {}
end, { desc = "start a pass when a devbox workspace opens", first_delay = 45 }) -- after the first pass's 30 s

kitchen.command("run", function()
  manual = true
  if not timer:run() then
    manual = false
    kitchen.notify("mcp-token-sync: a pass is already running")
  end
end, { desc = "push MCP OAuth tokens to the devboxes now" })

return M
