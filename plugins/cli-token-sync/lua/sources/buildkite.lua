-- Buildkite: bin/bk logs in with OAuth and keeps the access token and the
-- refresh token in the mac's keychain (services "buildkite-cli" and
-- "buildkite-cli-refresh", account = the org slug, through go-keyring). On a
-- box without a keychain it uses /dev/shm/buildkite-cli-<uid>/credentials.json,
-- which a restart wipes. Access tokens last an hour. bk refreshes only after a
-- request gets a 401, so the mac refreshes here before expiry.
local S = {
  name = "buildkite",
  title = "Buildkite",
  login = "bin/bk auth login",
  -- All a box gets. Never the refresh token: bk rotates them, and deletes its
  -- own when a refresh is refused, so a second user would log the mac out.
  entry = { "access_token", "expires_at" },
}

-- "perplexity" → "buildkite.com/perplexity"
function S.label(key)
  return "buildkite.com/" .. tostring(key)
end

-- MAC_PY's stdin.
function S.mac_input(config, window)
  return { org = config.buildkite_org, window = window }
end

-- Python, run on the mac with { org, window } on stdin (tests add security,
-- api and oauth to swap in fakes). Prints { missing = true } when the keychain
-- has no token for the org, else { servers = [ row ] } with one row:
--   key (the org), expires_at (unix s, 0 = no expiry), entry = {
--   access_token, expires_at } while the token works, refreshed (bool), error
--   (refresh failed), login_active = false (Buildkite refused the refresh
--   token: a new `bk auth login` is needed).
-- Buildkite's /v2/access-token says when the token expires. With `window`
-- seconds or less left, or a 401, it is refreshed exactly as bk does it (same
-- form, bk's client id), and both tokens go back into the keychain as
-- go-keyring writes them (`security -i`, so no token is in argv; base64 with
-- go-keyring's prefix), the refresh token first: with only that written, bk
-- still recovers. A refused refresh whose refresh token bk has rotated
-- meanwhile uses bk's tokens. Messages never carry a token.
S.MAC_PY = [==[
import base64, calendar, json, subprocess, sys, time, urllib.error, urllib.parse, urllib.request

CLIENT_ID = "5214b230f06b48938ab5"  # bk's oauth.DefaultClientID
ACCESS, REFRESH = "buildkite-cli", "buildkite-cli-refresh"  # bk's keychain services
B64, HEX = "go-keyring-base64:", "go-keyring-encoded:"  # go-keyring's encodings

req = json.load(sys.stdin)
org = req["org"]
security = req.get("security", "/usr/bin/security")
api = req.get("api", "https://api.buildkite.com")
oauth = req.get("oauth", "https://buildkite.com")

class Refused(RuntimeError):
    pass

def get(service):
    r = subprocess.run([security, "find-generic-password", "-s", service, "-wa", org], capture_output=True, text=True)
    if r.returncode != 0:
        if "could not be found" in r.stdout + r.stderr:
            return None
        raise RuntimeError("keychain read failed (exit %d)" % r.returncode)
    v = r.stdout.strip()
    if v.startswith(B64):
        return base64.b64decode(v[len(B64):]).decode()
    if v.startswith(HEX):
        return bytes.fromhex(v[len(HEX):]).decode()
    return v

def put(service, value):
    secret = B64 + base64.b64encode(value.encode()).decode()
    cmd = "add-generic-password -U -s '%s' -a '%s' -w '%s'\n" % (service, org, secret)
    r = subprocess.run([security, "-i"], input=cmd, capture_output=True, text=True)
    # `security -i` can exit 0 on a failed command, and its output may echo
    # the command: check by reading back, and never show that output.
    if r.returncode != 0 or get(service) != value:
        raise RuntimeError("keychain write failed (exit %d)" % r.returncode)

def http(url, form=None, token=None):
    data = urllib.parse.urlencode(form).encode() if form is not None else None
    headers = {"Accept": "application/json"}
    if data is not None:
        headers["Content-Type"] = "application/x-www-form-urlencoded"
    if token:
        headers["Authorization"] = "Bearer " + token
    try:
        with urllib.request.urlopen(urllib.request.Request(url, data=data, headers=headers), timeout=20) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        try:
            body = json.load(e)
        except ValueError:
            body = {}
        body = body if isinstance(body, dict) else {}
        detail = " ".join(str(body.get(k, "")) for k in ("error", "error_description")).strip()
        err = Refused if 400 <= e.code < 500 else RuntimeError
        raise err(("HTTP %d %s" % (e.code, detail)).strip())

def info(token):
    """(works, expires_at unix s or 0) for an access token."""
    try:
        d = http(api + "/v2/access-token", token=token)
    except Refused as e:
        if str(e).startswith("HTTP 401"):
            return False, 0
        raise
    exp = d.get("expires_at")
    return True, calendar.timegm(time.strptime(exp[:19], "%Y-%m-%dT%H:%M:%S")) if exp else 0

acc = get(ACCESS)
if not acc:
    print(json.dumps({"missing": True}))
    sys.exit(0)
row = {"key": org}
works, exp = info(acc)
if not works or (exp and exp - time.time() <= req["window"]):
    ref = get(REFRESH)
    if not ref:
        row["error"] = "no refresh token"
    else:
        try:
            tok = http(oauth + "/oauth/token", {"grant_type": "refresh_token", "refresh_token": ref, "client_id": CLIENT_ID})
            put(REFRESH, tok.get("refresh_token") or ref)
            put(ACCESS, tok["access_token"])
            acc = tok["access_token"]
            works, exp = info(acc)
            row["refreshed"] = True
        except Refused as e:
            if get(REFRESH) != ref:  # bk refreshed meanwhile: its tokens win
                acc = get(ACCESS) or ""
                works, exp = info(acc) if acc else (False, 0)
            else:
                row["error"] = str(e)[:200]
                row["login_active"] = False
        except Exception as e:
            row["error"] = str(e)[:200]
row["expires_at"] = exp
if works:
    row["entry"] = {"access_token": acc, "expires_at": exp}
print(json.dumps({"servers": [row]}))
]==]

-- Python, run on a box with { servers = [ { key, entry } ] } on stdin, that
-- writes each org's access token into bk's /dev/shm store
-- ($BUILDKITE_CREDENTIAL_STORE_PATH, else /dev/shm/buildkite-cli-<uid>/
-- credentials.json), under bk's own lock (flock on .credentials.lock):
--   * the directory is created mode 700; no /dev/shm: exit 1
--   * the file missing or empty: start from {}; invalid JSON, a top level,
--     services or services["buildkite-cli"] that is not an object: exit 1
--     before writing (never clobber it)
--   * the org's token under services["buildkite-cli"], and "shm" as its
--     preferred store, as bk's own write does; the refresh token, other orgs
--     and other keys stay, so a box that logged in itself carries on while the
--     mac sleeps
--   * prints "updated <org>" for each org it changed, nothing for the rest
--   * when anything changed: a temp file (mode 600) renamed over the file.
--     Nothing changed: the file is left alone.
S.MERGE_PY = [==[
import fcntl, json, os, sys, tempfile

ACCESS = "buildkite-cli"
want = json.load(sys.stdin)["servers"]
path = os.environ.get("BUILDKITE_CREDENTIAL_STORE_PATH")
if not path:
    if not os.path.isdir("/dev/shm"):
        sys.exit("no /dev/shm")
    path = "/dev/shm/buildkite-cli-%d/credentials.json" % os.getuid()
d = os.path.dirname(path)
os.makedirs(d, 0o700, exist_ok=True)
if os.path.islink(d) or not os.path.isdir(d):
    sys.exit("credential store directory is not a directory")
os.chmod(d, 0o700)

lock = os.open(os.path.join(d, ".credentials.lock"), os.O_CREAT | os.O_RDWR, 0o600)
try:
    fcntl.flock(lock, fcntl.LOCK_EX)
    try:
        with open(path) as f:
            text = f.read()
    except FileNotFoundError:
        text = ""
    store = json.loads(text) if text.strip() else {}
    if not isinstance(store, dict):
        sys.exit("credentials.json: the top level is not an object")
    services = store.setdefault("services", {})
    preferred = store.setdefault("preferred_stores", {})
    if not isinstance(services, dict) or not isinstance(preferred, dict):
        sys.exit("credentials.json: services or preferred_stores is not an object")
    tokens = services.setdefault(ACCESS, {})
    stores = preferred.setdefault(ACCESS, {})
    if not isinstance(tokens, dict) or not isinstance(stores, dict):
        sys.exit("credentials.json: buildkite-cli is not an object")

    changed = False
    for s in want:
        org, token = s["key"], s["entry"]["access_token"]
        if tokens.get(org) == token and stores.get(org) == "shm":
            continue
        tokens[org] = token
        stores[org] = "shm"
        changed = True
        print("updated", org)

    if changed:
        fd, tmp = tempfile.mkstemp(dir=d, prefix=".credentials-", suffix=".tmp")
        try:
            os.fchmod(fd, 0o600)
            with os.fdopen(fd, "w") as f:
                json.dump(store, f, indent=2)
                f.write("\n")
            os.replace(tmp, path)
        finally:
            if os.path.exists(tmp):
                os.unlink(tmp)
finally:
    os.close(lock)
]==]

return S
