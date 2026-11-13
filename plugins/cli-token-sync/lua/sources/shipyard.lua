-- Shipyard: bin/shipyard logs in to Okta (shipyard-cli app) and keeps the
-- tokens in tokens.json, keyed by server URL. Access tokens last an hour; the
-- refresh token lasts 90 days from the browser login, and refreshing does not
-- extend that. bin/shipyard refreshes only in a token's last minute, so the
-- mac refreshes here first.
local S = {
  name = "shipyard",
  title = "Shipyard",
  login = "bin/shipyard auth login",
  -- All a box gets of a token. Never the refresh token: Okta rotates them, so a
  -- second user of the mac's chain would log the mac out.
  entry = { "access_token", "expires_at", "issuer", "client_id" },
}

-- "https://shipyard.prod.p.perplexity.ai" → "shipyard.prod.p.perplexity.ai"
function S.label(key)
  return (tostring(key):gsub("^https?://", ""):gsub("/+$", ""))
end

-- MAC_PY's stdin.
function S.mac_input(config, window, introspect)
  return { path = config.shipyard_tokens, window = window, introspect = introspect }
end

-- Python, run on the mac with { path, window, introspect } on stdin. Prints
-- { missing = true } when the file does not exist, else { servers = [ row ] },
-- one row per server with an access token, sorted by key:
--   key, expires_at (unix s, 0 = no expiry), entry = { access_token,
--   expires_at, issuer, client_id }, refreshed (bool), error (refresh failed),
--   login_active / login_expires_at (from introspection), introspect_error.
-- A token with `window` seconds or less left is refreshed exactly as
-- bin/shipyard does it (same form, same scope) and written back (temp file,
-- rename, mode 600); if the file changed under the refresh, the file wins.
-- After a refresh (or when `introspect` is set) Okta's introspection endpoint
-- says when the login ends, without spending the refresh token. Messages
-- never carry a token: only Okta's error code and description.
S.MAC_PY = [==[
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
        raise RuntimeError(("HTTP %d %s" % (e.code, detail)).strip())

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

-- Python, run on a box with { servers = [ { key, entry } ] } on stdin, that
-- merges each server's token into bin/shipyard's file there ($XDG_CONFIG_HOME,
-- else ~/.config, then shipyard/tokens.json):
--   * the file missing: start from {} (and create the directory, mode 700);
--     invalid JSON, a top level or servers that is not an object: exit 1
--     before writing (never clobber it). A legacy flat file (top-level
--     access_token, no servers) starts over: bin/shipyard discards it too.
--   * keep every other top-level key and every other server's entry
--   * a server is written only when its access token lasts longer than the
--     box's; the box's refresh_token stays when issuer and client id match,
--     so a box that logged in itself carries on while the mac sleeps
--   * prints "updated <key>" for each server it changed, nothing for the rest
--   * when anything changed: a temp file (mode 600), fsync, os.replace over
--     the file; the temp file goes if any step fails. Nothing changed: the
--     file (and its mtime) is left alone.
S.MERGE_PY = [==[
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

return S
