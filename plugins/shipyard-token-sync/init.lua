-- shipyard-token-sync: every interval_minutes, refresh the mac's Shipyard
-- login when it is close to expiry, and copy its access token to every
-- devbox that has a workspace open.
--
-- bin/shipyard logs in to Okta (shipyard-cli app) and keeps the tokens in
-- tokens.json, keyed by server URL. Access tokens last an hour; the refresh
-- token lasts 90 days from the browser login, and refreshing does not extend
-- that. A new devbox would need its own `bin/shipyard auth login --device`.
-- With this plugin the mac's login covers every box.
--
-- A pass:
--   1. MAC_PY reads the mac's tokens.json. A token with refresh_minutes or
--      less left is refreshed with Okta, exactly as bin/shipyard does it (same
--      form, same scope), and written back (temp file, rename, mode 600). The
--      CLI refreshes only in its last minute, so the two don't collide; if the
--      file changed under the refresh anyway, the file wins. After a refresh
--      (and on the first pass of each load) Okta's introspection endpoint says
--      when the login ends, without spending the refresh token.
--   2. The payload has each server's access token, expiry, issuer and client
--      id. Never the refresh token: Okta rotates them, so a second user of the
--      mac's chain would log the mac out.
--   3. For each devbox host, one ssh call runs MERGE_PY, which writes the
--      token into ~/.config/shipyard/tokens.json when it lasts longer than
--      the box's own. A refresh token the box got from its own login stays, so
--      that box keeps working on its own chain while the mac sleeps.
-- A devbox workspace that opens is pushed to within 10 s, not at the next pass.
--
-- Token values never reach a log, a toast or the dashboard.

local M = {}

-- Python, run on the mac with { path, window, introspect } on stdin. Prints
-- { missing = true } when the file does not exist, else { servers = [ row ] },
-- one row per server with an access token, sorted by key:
--   key, expires_at (unix s, 0 = no expiry), entry = { access_token,
--   expires_at, issuer, client_id }, refreshed (bool), error (refresh failed),
--   login_active / login_expires_at (from introspection), introspect_error.
-- Messages never carry a token: only Okta's error code and description.
M.MAC_PY = [==[
import json, os, sys, tempfile, time, urllib.error, urllib.parse, urllib.request

SCOPE = "openid email profile groups offline_access"  # bin/shipyard's oidcLoginScope
ENTRY = ("access_token", "expires_at", "issuer", "client_id")

req = json.load(sys.stdin)
path = os.path.expanduser(req["path"])
discovered = {}

def fetch(url, form=None):
    data = urllib.parse.urlencode(form).encode() if form is not None else None
    headers = {"Accept": "application/json"}
    if data is not None:
        headers["Content-Type"] = "application/x-www-form-urlencoded"
    try:
        with urllib.request.urlopen(urllib.request.Request(url, data=data, headers=headers), timeout=20) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        try:
            body = json.load(e)
        except ValueError:
            body = {}
        detail = " ".join(str(body.get(k, "")) for k in ("error", "error_description")).strip()
        raise RuntimeError("HTTP %d %s" % (e.code, detail))

def endpoint(issuer, name):
    if issuer not in discovered:
        discovered[issuer] = fetch(issuer.rstrip("/") + "/.well-known/openid-configuration")
    return discovered[issuer][name]

def write(store):
    d = os.path.dirname(path)
    fd, tmp = tempfile.mkstemp(dir=d, prefix="tokens.json.")
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w") as f:
            json.dump(store, f, indent=2)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)

def load():
    with open(path) as f:
        store = json.load(f)
    servers = store.get("servers") if isinstance(store, dict) else None
    if not isinstance(servers, dict):
        sys.exit("tokens.json has no servers map")
    return store, servers

try:
    store, servers = load()
except FileNotFoundError:
    print(json.dumps({"missing": True}))
    sys.exit(0)

rows, refreshed = [], {}
for key in sorted(servers):
    t = servers[key]
    if not isinstance(t, dict) or not t.get("access_token"):
        continue
    row = {"key": key}
    exp = int(t.get("expires_at") or 0)
    if exp > 0 and exp - time.time() <= req["window"]:
        if t.get("refresh_token") and t.get("issuer") and t.get("client_id"):
            try:
                tok = fetch(endpoint(t["issuer"], "token_endpoint"), {
                    "grant_type": "refresh_token",
                    "refresh_token": t["refresh_token"],
                    "client_id": t["client_id"],
                    "scope": SCOPE,
                })
                new = {
                    "access_token": tok["access_token"],
                    "refresh_token": tok.get("refresh_token") or t["refresh_token"],
                    "issuer": t["issuer"],
                    "client_id": t["client_id"],
                }
                if tok.get("expires_in"):
                    new["expires_at"] = int(time.time()) + int(tok["expires_in"])
                refreshed[key] = (t, new)
                t = new
                row["refreshed"] = True
            except Exception as e:
                row["error"] = str(e)[:200]
        else:
            row["error"] = "no refresh token, issuer or client id"
    servers[key] = t
    rows.append(row)

if refreshed:
    # Re-read: bin/shipyard may have refreshed meanwhile. Its token wins.
    latest, latest_servers = load()
    applied = False
    for key, (old, new) in refreshed.items():
        if latest_servers.get(key) == old:
            latest_servers[key] = new
            applied = True
        else:
            servers[key] = latest_servers.get(key) or {}
            next(r for r in rows if r["key"] == key).pop("refreshed")
    if applied:
        write(latest)

for row in rows:
    t = servers[row["key"]]
    row["expires_at"] = int(t.get("expires_at") or 0)
    row["entry"] = {k: t[k] for k in ENTRY if k in t}
    if (req.get("introspect") or row.get("refreshed")) and t.get("refresh_token") and t.get("issuer"):
        try:
            r = fetch(endpoint(t["issuer"], "introspection_endpoint"), {
                "token": t["refresh_token"],
                "token_type_hint": "refresh_token",
                "client_id": t.get("client_id", ""),
            })
            row["login_active"] = bool(r.get("active"))
            if r.get("exp"):
                row["login_expires_at"] = int(r["exp"])
        except Exception as e:
            row["introspect_error"] = str(e)[:200]

print(json.dumps({"servers": rows}))
]==]

-- Python, run on a box with M.payload's JSON on stdin, that merges each
-- server's token into bin/shipyard's file there ($XDG_CONFIG_HOME, else
-- ~/.config, then shipyard/tokens.json):
--   * the file missing: start from {} (and create the directory, mode 700);
--     invalid JSON, a top level or servers that is not an object: exit 1
--     before writing (never clobber it). A legacy flat file (top-level
--     access_token, no servers) starts over: bin/shipyard discards it too.
--   * keep every other top-level key and every other server's entry
--   * a server is written only when its access token lasts longer than the
--     box's; the box's refresh_token stays when issuer and client id match
--   * prints "updated <key>" for each server it changed, nothing for the rest
--   * when anything changed: a temp file (mode 600), fsync, os.replace over
--     the file; the temp file goes if any step fails. Nothing changed: the
--     file (and its mtime) is left alone.
M.MERGE_PY = [==[
import json, os, sys, tempfile

want = json.load(sys.stdin)["servers"]
d = os.path.join(os.environ.get("XDG_CONFIG_HOME") or os.path.expanduser("~/.config"), "shipyard")
path = os.path.join(d, "tokens.json")

try:
    with open(path) as f:
        store = json.load(f)
except FileNotFoundError:
    store = {}
if not isinstance(store, dict):
    sys.exit("tokens.json: the top level is not an object")
if "servers" not in store and store.get("access_token"):
    store = {}
servers = store.setdefault("servers", {})
if not isinstance(servers, dict):
    sys.exit("tokens.json: servers is not an object")

changed = False
for s in want:
    key, entry = s["key"], s["entry"]
    cur = servers.get(key) if isinstance(servers.get(key), dict) else {}
    if cur.get("access_token") and int(cur.get("expires_at") or 0) >= int(entry.get("expires_at") or 0):
        continue
    new = dict(entry)
    if cur.get("refresh_token") and cur.get("issuer") == entry.get("issuer") and cur.get("client_id") == entry.get("client_id"):
        new["refresh_token"] = cur["refresh_token"]
    servers[key] = new
    changed = True
    print("updated", key)

if changed:
    os.makedirs(d, 0o700, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=d, prefix="tokens.json.")
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w") as f:
            json.dump(store, f, indent=2)
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, path)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)
]==]

local ENTRY = { "access_token", "expires_at", "issuer", "client_id" } -- all a box gets
local MIN_LEFT = 60 -- s: a token closer to expiry than this stays home

-- The server's host name, for logs and toasts ("https://shipyard.prod.p.perplexity.ai"
-- → "shipyard.prod.p.perplexity.ai").
function M.label(key)
  return (tostring(key):gsub("^https?://", ""):gsub("/+$", ""))
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

-- MERGE_PY's stdin, as a table to kitchen.json.encode: { servers = { { key,
-- entry }, ... } } for the rows of MAC_PY's report with an access token that
-- has more than MIN_LEFT seconds left at `now` (or no expiry). The entry is
-- rebuilt from ENTRY, so nothing else (a refresh token) can leave the mac.
function M.payload(report, now)
  local list = {}
  for _, row in ipairs(type(report) == "table" and report.servers or {}) do
    local e = row.entry or {}
    local exp = tonumber(e.expires_at) or 0
    if type(e.access_token) == "string" and e.access_token ~= "" and (exp == 0 or exp - now > MIN_LEFT) then
      local entry = {}
      for _, k in ipairs(ENTRY) do
        entry[k] = e[k]
      end
      table.insert(list, { key = row.key, entry = entry })
    end
  end
  return { servers = list }
end

-- What to tell William about `row` (a MAC_PY report row) at `now`, given
-- when its login ends (`login_exp`, unix s or nil) and `warn_days`: nil, or a
-- message. Only problems he has to fix: a failed refresh of a token that
-- still works is retried next pass, so it is logged, not shown.
function M.problem(row, login_exp, now, warn_days)
  local label = M.label(row.key)
  local exp = tonumber(row.expires_at) or 0
  if row.login_active == false then
    return label .. ": the mac's Shipyard login has ended; run bin/shipyard auth login on the mac"
  end
  if row.error and exp > 0 and exp <= now then
    return label .. ": the mac's Shipyard token expired and refreshing failed (" .. row.error .. "); run bin/shipyard auth login on the mac"
  end
  if login_exp and login_exp - now <= warn_days * 86400 then
    return label
      .. ": the mac's Shipyard login ends "
      .. os.date("!%Y-%m-%d %H:%M UTC", login_exp)
      .. "; run bin/shipyard auth login on the mac before then"
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
    kitchen.notify("shipyard-token-sync: " .. msg, "warn")
  end
end

local login_exp = {} -- server key → when the mac's login ends (unix s), from introspection
local introspected = false -- this load has asked Okta when the logins end

-- MAC_PY's report, decoded. Errors raise with why(res) or a fixed text,
-- never the report's contents.
local function mac_report(window)
  local stdin = kitchen.json.encode({ path = kitchen.config.mac_tokens, window = window, introspect = not introspected })
  local res = kitchen.run_sync({ host = "local", timeout = 90, cmd = { "python3", "-c", M.MAC_PY }, stdin = stdin })
  if res.code ~= 0 then
    error("mac: " .. why(res), 0)
  end
  local ok, report = pcall(kitchen.json.decode, res.stdout)
  if not ok or type(report) ~= "table" then
    error("mac: the report is not JSON", 0)
  end
  introspected = true
  return report
end

-- Push `stdin` (M.payload, encoded) to `host`; log each server it updated.
local function push(host, stdin)
  local res = kitchen.run_sync({ host = host, timeout = 60, cmd = { "python3", "-c", M.MERGE_PY }, stdin = stdin })
  if res.code ~= 0 then
    error("merge on " .. host .. ": " .. why(res), 0)
  end
  for key in (res.stdout or ""):gmatch("updated ([^\r\n]+)") do
    kitchen.log("[" .. host .. "]", "updated", M.label(key))
  end
end

local manual = false -- the next pass was asked for with shipyard-token-sync.run
local tried = {} -- host → true once a pass of this load has started with it

-- One pass (see the header). Returns the dashboard stats and per-workspace
-- stats { [ws_id] = { pushed = 1 } or { failed = "…" } }.
local function pass()
  local was_manual = manual
  manual = false
  for _, host in ipairs((M.hosts(kitchen.workspaces()))) do
    tried[host] = true
  end

  local window = math.max(1, tonumber(kitchen.config.refresh_minutes) or 15) * 60
  local warn_days = tonumber(kitchen.config.warn_days) or 7
  local report, now = mac_report(window), os.time()
  if report.missing then
    warn("mac", "no Shipyard login on this mac; run bin/shipyard auth login in agi")
    return { servers = 0 }, {}
  end
  warn("mac", nil)

  local stats = { servers = 0, pushed = 0, failed = 0 }
  for _, row in ipairs(report.servers or {}) do
    local label = M.label(row.key)
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
      login_exp[row.key] = tonumber(row.login_expires_at)
    end
    warn("login:" .. row.key, M.problem(row, login_exp[row.key], now, warn_days))
    local exp = tonumber(row.expires_at) or 0
    if exp > now and (not stats.minutes_left or (exp - now) / 60 < stats.minutes_left) then
      stats.minutes_left = math.floor((exp - now) / 60)
    end
    local ends = login_exp[row.key]
    if ends and (not stats.login_days_left or (ends - now) / 86400 < stats.login_days_left) then
      stats.login_days_left = math.floor((ends - now) / 8640 + 0.5) / 10
    end
  end

  local payload = M.payload(report, now)
  stats.servers = #payload.servers
  local hosts, ws_of = M.hosts(kitchen.workspaces())
  stats.hosts = #hosts
  local per_ws = {}
  if #payload.servers > 0 then
    local stdin = kitchen.json.encode(payload)
    for _, host in ipairs(hosts) do
      local ok, err = pcall(push, host, stdin)
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
    kitchen.notify("shipyard-token-sync: pushed to " .. stats.pushed .. " host(s), " .. stats.failed .. " failed")
  end
  return stats, per_ws
end

-- The kitchen runs it on schedule and never starts one while another runs.
local timer = kitchen.every("pass", math.max(1, tonumber(kitchen.config.interval_minutes) or 5) * 60, pass, {
  desc = "refresh the mac's Shipyard login and push its access token to every devbox",
  first_delay = 40,
})

-- A newly opened devbox workspace gets the token within NEW_HOST_POLL
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
    kitchen.notify("shipyard-token-sync: a pass is already running")
  end
end, { desc = "refresh the mac's Shipyard login and push it to the devboxes now" })

return M
