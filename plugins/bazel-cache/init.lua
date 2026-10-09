-- bazel-cache: keep the Bazel caches on each workspace's host under a budget.
--
-- Every interval_minutes, for each distinct host in kitchen.workspaces()
-- (and the mac): measure the output bases, repository cache and disk cache
-- under `root`, and the free space on its volume. Output bases whose worktree
-- is gone always go. When the total is over max_gb or the volume is under
-- min_free_percent free, evict least-recently-used units down to target_gb:
-- repo cache entries (repo contents and downloads) by last use, and output
-- bases with no server by last command, in one list.
--
-- Never under a running build: an output base goes only when no server runs
-- in it and no command holds it; a repo contents entry never while a running
-- server's output base links to it (its external/ symlinks; a busy server's
-- links pin, an idle server, no command for idle_minutes, is stopped first).
-- Entries used within idle_minutes are never evicted, re-checked right before
-- each deletion. The disk cache is counted, not evicted: agi bounds it with
-- Bazel's own GC.

local M = {}

local GIB = 1024 * 1024 -- KiB

local function basename(path)
  return path:match("[^/]+$") or path
end

-- A repo contents entry's "<hash>/<uuid>", how output bases' links and
-- entries are matched (so differently spelled roots agree).
local function key(path)
  return path:match("[^/]+/[^/]+$") or path
end

-- The config root → a shell word: a leading ~ becomes "$HOME", and it's
-- double-quoted so it can't split (`$`, `"`, `\` and backquotes escaped).
function M.shell_path(p)
  local home, rest = "", p
  if p == "~" or p:sub(1, 2) == "~/" then
    home, rest = "$HOME", p:sub(2)
  end
  return '"' .. home .. rest:gsub('[%$"\\`]', "\\%0") .. '"'
end

-- Shell (POSIX sh) that defines `st` (prints "<mtime> <path>" for each file
-- argument, following symlinks), `servers` (pid and args of each Bazel
-- server of this user), `client_busy <output base>` (a Bazel client holds
-- its lock: the pid in `lock` is alive and is Bazel) and `pins <output
-- base>` (prints "pin <base folder> <hash>/<uuid>" for each repo contents
-- entry its external/ links to).
local PRELUDE = [[
if [ "$(uname)" = Darwin ]; then sf=-f sv='%m %N'; else sf=-c sv='%Y %n'; fi
st() { stat -L "$sf" "$sv" "$@" 2>/dev/null; }
servers() { ps -ww -U "$(id -u)" -o pid=,args= | grep 'A-server\.jar'; }
client_busy() {
  c=$(sed -n 's/^pid=//p' "$1/lock" 2>/dev/null | head -n1)
  [ -n "$c" ] && kill -0 "$c" 2>/dev/null && ps -o args= -p "$c" 2>/dev/null | grep -q bazel
}
pins() { find "$1/external" -mindepth 1 -maxdepth 1 -type l -exec readlink {} + 2>/dev/null | awk -F/ -v b="${1##*/}" 'NF > 2 && $(NF-2) == "contents" { print "pin", b, $(NF-1) "/" $NF }'; }
]]

-- Shell, run on the host, that prints (KiB, epoch seconds; the path last so
-- it may hold spaces):
--   df <size> <avail>                         the volume holding root
--   server <pid> <output_base>                each running Bazel server
--   base <kib> <active> <exists 0|1> <busy 0|1> <path>\t<workspace>
--                                             each output base in root/workspace (not install/):
--                                             active = newest mtime of lock, java.log and
--                                             command.profile.gz; exists = the workspace named
--                                             in its README is a folder (or there's no README);
--                                             busy = a client holds it (client_busy)
--   pin <base folder> <hash>/<uuid>           each repo contents entry an output base with a
--                                             running server, or a busy one, links to (pins)
--   size <kind> <kib> <path>                  each repo cache entry: kind "contents"
--                                             (repo/contents/<h>/<uuid>) or "download"
--                                             (repo/content_addressable/<algo>/<h>)
--   used <kind> <mtime> <path>                when that entry was last used: Bazel touches
--                                             <uuid>.recorded_inputs and the download's `file`
--                                             on every hit
--   disk <kib>                                root/disk
-- A missing root prints just df. Fails (non-zero) when df or ps fails.
function M.scan_sh(root)
  return PRELUDE .. "r=" .. M.shell_path(root) .. "\n" .. [[
v=$r; while [ ! -e "$v" ]; do v=$(dirname "$v"); done
d=$(df -Pk "$v" | awk 'NR==2 {printf "df %d %d\n", $2, $4}')
[ -n "$d" ] || exit 1
echo "$d"
p=$(ps -ww -U "$(id -u)" -o pid=,args=) || exit 1
printf '%s\n' "$p" | awk '/A-server\.jar/ { for (i = 2; i <= NF; i++) if ($i ~ /^--output_base=/) { sub(/^--output_base=/, "", $i); print "server", $1, $i } }'
srv=" $(printf '%s\n' "$p" | awk '/A-server\.jar/ { for (i = 2; i <= NF; i++) if ($i ~ /^--output_base=/) { n = split($i, a, "/"); printf "%s ", a[n] } }')"
[ -d "$r" ] || exit 0
for b in "$r"/workspace/*/; do
  b=${b%/}
  [ -d "$b" ] && [ "${b##*/}" != install ] || continue
  k=$(du -sxk "$b" 2>/dev/null | awk '{print $1}')
  a=$(st "$b/lock" "$b/java.log" "$b/command.profile.gz" | awk '$1 > m {m = $1} END {print m + 0}')
  w=$(sed -n '1s/^WORKSPACE: //p' "$b/README" 2>/dev/null)
  e=1; [ -n "$w" ] && [ ! -d "$w" ] && e=0
  u=0; client_busy "$b" && u=1
  printf 'base %s %s %s %s %s\t%s\n' "${k:-0}" "$a" "$e" "$u" "$b" "$w"
  case "$srv" in *" ${b##*/} "*) pins "$b" ;; *) [ $u = 0 ] || pins "$b" ;; esac
done
sizes() { find "$1" -mindepth 2 -maxdepth 2 -type d -exec du -sxk {} + 2>/dev/null | awk -v k="$2" '{s = $1; sub(/^[0-9]+[ \t]+/, ""); print "size", k, s, $0}'; }
[ -d "$r/repo/contents" ] && sizes "$r/repo/contents" contents
[ -d "$r/repo/contents" ] && find "$r/repo/contents" -mindepth 2 -maxdepth 2 -name '*.recorded_inputs' -exec stat -L "$sf" "$sv" {} + 2>/dev/null | awk '{t = $1; sub(/^[0-9]+ /, ""); sub(/\.recorded_inputs$/, ""); print "used contents", t, $0}'
[ -d "$r/repo/content_addressable" ] && sizes "$r/repo/content_addressable" download
[ -d "$r/repo/content_addressable" ] && find "$r/repo/content_addressable" -mindepth 3 -maxdepth 3 -name file -exec stat -L "$sf" "$sv" {} + 2>/dev/null | awk '{t = $1; sub(/^[0-9]+ /, ""); sub(/\/file$/, ""); print "used download", t, $0}'
[ -d "$r/disk" ] && du -sxk "$r/disk" 2>/dev/null | awk '{print "disk", $1}'
true
]]
end

-- scan_sh's output → { df = {size, avail}, bases = {{kib, active, exists,
-- busy, path, workspace, pid}}, entries = {{kind, kib, used, path}},
-- pins = { [base folder] = { ["<hash>/<uuid>"] = true } }, disk_kib },
-- or nil when the df line is missing (the scan failed: do nothing). A base's
-- pid is its running server's (matched by folder name, so /private and
-- symlinked spellings agree; servers outside root aren't ours). Entries
-- without a `used` line are still being written and are left out, as is
-- anything in a hidden folder (Bazel's own GC trash).
function M.parse_scan(stdout)
  local s = { bases = {}, entries = {}, pins = {}, disk_kib = 0 }
  local servers, sizes, used, order = {}, {}, {}, {}
  for line in (stdout or ""):gmatch("[^\n]+") do
    local size, avail = line:match("^df (%d+) (%d+)$")
    if size then
      s.df = { size = tonumber(size), avail = tonumber(avail) }
    end
    local pid, ob = line:match("^server (%d+) (.+)$")
    if pid then
      servers[basename(ob)] = tonumber(pid)
    end
    local kib, active, exists, busy, rest = line:match("^base (%d+) (%d+) ([01]) ([01]) (.*)$")
    if kib then
      local path, ws = rest:match("^([^\t]*)\t?(.*)$")
      table.insert(s.bases, { kib = tonumber(kib), active = tonumber(active), exists = exists == "1", busy = busy == "1", path = path, workspace = ws ~= "" and ws or nil })
    end
    local kind, n, path = line:match("^size (%a+) (%d+) (.+)$")
    if kind and not path:find("/%.") then
      local key = kind .. " " .. path
      if not sizes[key] then
        table.insert(order, key)
      end
      sizes[key] = { kind = kind, kib = tonumber(n), path = path }
    end
    kind, n, path = line:match("^used (%a+) (%d+) (.+)$")
    if kind then
      used[kind .. " " .. path] = tonumber(n)
    end
    local folder, pin = line:match("^pin (%S+) (.+)$")
    if folder then
      s.pins[folder] = s.pins[folder] or {}
      s.pins[folder][pin] = true
    end
    local d = line:match("^disk (%d+)$")
    if d then
      s.disk_kib = tonumber(d)
    end
  end
  if not s.df then
    return nil
  end
  for _, b in ipairs(s.bases) do
    b.pid = servers[basename(b.path)]
  end
  for _, key in ipairs(order) do
    if used[key] then
      local e = sizes[key]
      e.used = used[key]
      table.insert(s.entries, e)
    end
  end
  return s
end

-- What to do on one host. `scan` from parse_scan; `cfg` the numeric config
-- (max_gb, target_gb, min_free_percent, idle_minutes); `now` epoch seconds.
-- Returns {
--   stop = { pids },             idle servers to stop before evicting
--   evict = { {kind, path, kib} }, orphan bases, then least recently used first
--   total_kib, freed_kib,        the caches now, and what the plan frees
--   over = "size" | "disk" | nil, why it cleans past the orphans
--   pinned_kib,                  idle repo contents kept because a busy server links to them
--   blocked = "<why>" | nil,     short, and a busy server kept some of the repo cache
--   short_kib,                   still over after the plan (0: it fits)
-- }
function M.plan(scan, cfg, now)
  local idle = (cfg.idle_minutes or 30) * 60
  local pins = scan.pins or {}
  local total = scan.disk_kib or 0
  for _, b in ipairs(scan.bases) do
    total = total + b.kib
  end
  for _, e in ipairs(scan.entries) do
    total = total + e.kib
  end
  local p = { stop = {}, evict = {}, total_kib = total, freed_kib = 0, pinned_kib = 0, short_kib = 0 }
  local taken = {}
  local function take(kind, path, kib)
    table.insert(p.evict, { kind = kind, path = path, kib = kib })
    p.freed_kib = p.freed_kib + kib
    taken[path] = true
  end
  -- Orphans: the worktree is gone and nothing runs in it.
  for _, b in ipairs(scan.bases) do
    if not b.exists and not b.pid and not b.busy then
      take("base", b.path, b.kib)
    end
  end

  local size, avail = scan.df.size, scan.df.avail + p.freed_kib
  local left = total - p.freed_kib
  local min_free = (cfg.min_free_percent or 15) / 100 * size
  if left > (cfg.max_gb or 30) * GIB then
    p.over = "size"
  elseif size > 0 and avail < min_free then
    p.over = "disk"
  else
    return p
  end
  local need = math.max(left - (cfg.target_gb or 20) * GIB, min_free - avail, 0)
  local got = 0

  -- Servers under root: one that's busy (a command holds it, or one ran
  -- within idle_minutes) keeps the repo contents its output base links to;
  -- idle ones may be stopped.
  local stopping, pinned, busy_in = {}, {}, nil
  for _, b in ipairs(scan.bases) do
    local recent = now - b.active < idle
    if b.busy or (b.pid and recent) then
      busy_in = busy_in or basename(b.workspace or b.path)
      for k in pairs(pins[basename(b.path)] or {}) do
        pinned[k] = true
      end
    elseif b.pid and not taken[b.path] then
      stopping[b.path] = true
    end
  end

  -- One least-recently-used list: repo cache entries by last use (none a
  -- busy server links to), output bases by last command (none a client
  -- holds, none whose server stays running). Nothing used within idle_minutes.
  local units = {}
  for _, e in ipairs(scan.entries) do
    if now - e.used < idle then
      -- (in use: kept)
    elseif e.kind == "contents" and pinned[key(e.path)] then
      p.pinned_kib = p.pinned_kib + e.kib
    else
      table.insert(units, { kind = e.kind, path = e.path, kib = e.kib, at = e.used })
    end
  end
  for _, b in ipairs(scan.bases) do
    if not taken[b.path] and not b.busy and (not b.pid or stopping[b.path]) and now - b.active >= idle then
      table.insert(units, { kind = "base", path = b.path, kib = b.kib, at = b.active })
    end
  end
  table.sort(units, function(a, b)
    return a.at < b.at
  end)
  local gone = {}
  for _, u in ipairs(units) do
    if got >= need then
      break
    end
    take(u.kind, u.path, u.kib)
    got = got + u.kib
    if u.kind == "contents" then
      gone[key(u.path)] = true
    end
  end
  -- Stop only what the plan needs: the idle servers of the bases it removes,
  -- and of the bases linking to repo contents it removes.
  local function links_gone(b)
    for k in pairs(pins[basename(b.path)] or {}) do
      if gone[k] then
        return true
      end
    end
    return false
  end
  for _, b in ipairs(scan.bases) do
    if stopping[b.path] and (taken[b.path] or links_gone(b)) then
      table.insert(p.stop, b.pid)
    end
  end
  p.short_kib = math.max(need - got, 0)
  if p.short_kib > 0 and p.pinned_kib > 0 then
    p.blocked = "server busy in " .. busy_in
  end
  return p
end

-- Shell that carries out plan `p`, and its stdin (one "<kind> <path>" line
-- per item, so thousands of paths never hit an argument-length limit).
-- SIGTERMs p.stop, waits up to 30 s for them to exit (one still running:
-- "stuck <pid>"). Right before each deletion it re-checks: an output base,
-- that no server runs in it and no client holds it; a repo cache entry,
-- that it wasn't used within `idle_minutes` and (contents) that no output
-- base with a running server or a client links to it (those links read
-- before the first entry and every 100th). A repo contents entry loses its
-- <uuid>.recorded_inputs first, so Bazel sees a miss, never a half-gone
-- hit. Prints "freed <path>" or "skip <path> <why>" per item.
function M.evict_sh(p, root, idle_minutes)
  local pids = {}
  for _, pid in ipairs(p.stop or {}) do
    table.insert(pids, tostring(math.floor(pid)))
  end
  local lines = {}
  for _, it in ipairs(p.evict) do
    table.insert(lines, it.kind .. " " .. it.path)
  end
  local idle = math.floor((idle_minutes or 30) * 60)
  local script = PRELUDE .. "r=" .. M.shell_path(root) .. "\npids='" .. table.concat(pids, " ") .. "'\nidle=" .. idle .. "\n" .. [[
in_root() { servers | awk -v p="--output_base=$r/workspace/" -v h="$1" '{ for (i = 2; i <= NF; i++) if (index($i, p) == 1 && substr($i, length(p) + 1) == h) f = 1 } END { exit !f }'; }
pf=$(mktemp); trap 'rm -f "$pf"' EXIT
live_pins() {
  for b in "$r"/workspace/*/; do
    b=${b%/}
    [ -d "$b" ] && [ "${b##*/}" != install ] || continue
    if in_root "${b##*/}" || client_busy "$b"; then pins "$b"; fi
  done | awk '{ print $3 }' > "$pf"
}
for x in $pids; do kill -TERM "$x" 2>/dev/null; done
i=0
while [ -n "$pids" ] && [ $i -lt 30 ]; do
  s=0; for x in $pids; do kill -0 "$x" 2>/dev/null && s=1; done
  [ $s = 0 ] && break
  sleep 1; i=$((i + 1))
done
for x in $pids; do if kill -0 "$x" 2>/dev/null; then echo "stuck $x"; fi; done
n=0
while IFS=' ' read -r kind path; do
  [ -n "$path" ] && [ -e "$path" ] || continue
  if [ "$kind" = base ]; then
    if in_root "${path##*/}"; then echo "skip $path server"; continue; fi
    if client_busy "$path"; then echo "skip $path client"; continue; fi
  else
    # (not from the item list on stdin)
    [ $((n % 100)) = 0 ] && live_pins < /dev/null
    n=$((n + 1))
    if [ "$kind" = contents ]; then
      if grep -qxF "${path#"${path%/*/*}"/}" "$pf"; then echo "skip $path linked"; continue; fi
      m=$path.recorded_inputs
    else
      m=$path/file
    fi
    t=$(st "$m" | awk '{ print $1 }')
    if [ $((${t:-0} + idle)) -gt "$(date +%s)" ]; then echo "skip $path used"; continue; fi
    [ "$kind" = contents ] && rm -f "$m"
  fi
  chmod -R u+w "$path" 2>/dev/null
  rm -rf "$path" 2>/dev/null
  if [ -e "$path" ]; then echo "skip $path rm failed"; else echo "freed $path"; fi
done
true
]]
  return script, table.concat(lines, "\n") .. "\n"
end

-- evict_sh's output → { freed = { [path] = true }, skipped = { "<path> <why>" }, stuck = n }.
function M.parse_evict(stdout)
  local r = { freed = {}, skipped = {}, stuck = 0 }
  for line in (stdout or ""):gmatch("[^\n]+") do
    local path = line:match("^freed (.+)$")
    if path then
      r.freed[path] = true
    elseif line:match("^skip ") then
      table.insert(r.skipped, line:sub(6))
    elseif line:match("^stuck ") then
      r.stuck = r.stuck + 1
    end
  end
  return r
end

-- KiB → "1.9G", "512M", "40K" (as the dashboard plugin's).
function M.human(k)
  if k >= GIB then
    return string.format("%.1fG", k / GIB)
  elseif k >= 1024 then
    return string.format("%.0fM", k / 1024)
  end
  return string.format("%.0fK", k)
end

local function why(res)
  if res.timed_out then
    return "timed out"
  end
  local text = (res.stderr ~= "" and res.stderr or res.stdout or ""):gsub("%s+", " ")
  return text
end

local function config()
  local c = kitchen.config
  return {
    root = c.root or "~/bazel-cache",
    max_gb = tonumber(c.max_gb) or 30,
    target_gb = tonumber(c.target_gb) or 20,
    min_free_percent = tonumber(c.min_free_percent) or 15,
    idle_minutes = tonumber(c.idle_minutes) or 30,
  }
end

-- One host: scan, plan, evict. Returns its stats for the dashboard, and
-- what it freed (KiB) and whether it's still over budget.
local function clean_host(host, cfg)
  local log = function(...)
    kitchen.log("[" .. host .. "]", ...)
  end
  local res = kitchen.run_sync({ host = host, timeout = 1800, sh = M.scan_sh(cfg.root) })
  local scan = res.code == 0 and M.parse_scan(res.stdout) or nil
  if not scan then
    error("scan failed: " .. why(res))
  end
  local p = M.plan(scan, cfg, os.time())
  local free = scan.df.size > 0 and string.format("%.0f%%", scan.df.avail / scan.df.size * 100) or "?"
  local stats = { cache = M.human(p.total_kib), free = free }
  if p.blocked then
    log("over budget (" .. p.over .. "), " .. M.human(p.pinned_kib) .. " of the repo cache kept:", p.blocked)
    stats.blocked = p.blocked
  end
  if #p.evict == 0 then
    log("cache " .. stats.cache .. ", " .. free .. " free: nothing to evict")
    return stats, 0, p.over ~= nil
  end
  local script, input = M.evict_sh(p, cfg.root, cfg.idle_minutes)
  local rm = kitchen.run_sync({ host = host, timeout = 1800, sh = script, stdin = input })
  if rm.timed_out then
    log("gave up waiting on the eviction; it may still finish")
  end
  local r = M.parse_evict(rm.stdout)
  local freed, counts = 0, {}
  for _, it in ipairs(p.evict) do
    if r.freed[it.path] then
      freed = freed + it.kib
      counts[it.kind] = (counts[it.kind] or 0) + 1
      if it.kind == "base" then
        log("removed output base", it.path, "(" .. M.human(it.kib) .. ")")
      end
    end
  end
  for i, s in ipairs(r.skipped) do
    if i > 20 then
      log("... and " .. (#r.skipped - 20) .. " more skipped")
      break
    end
    log("skipped", s)
  end
  if r.stuck > 0 then
    log(r.stuck .. " server(s) didn't stop; what they use was kept")
  end
  local parts = {}
  for _, kind in ipairs({ "base", "contents", "download" }) do
    if counts[kind] then
      table.insert(parts, counts[kind] .. " " .. kind)
    end
  end
  log("cache " .. stats.cache .. ", " .. free .. " free" .. (p.over and " (over: " .. p.over .. ")" or "") .. ": freed " .. M.human(freed) .. (#parts > 0 and " (" .. table.concat(parts, ", ") .. ")" or ""))
  stats.freed = M.human(freed)
  -- Still over: the plan fell short, or something it counted on wasn't freed.
  return stats, freed, p.over ~= nil and p.short_kib + p.freed_kib - freed > 0
end

local manual = false -- the next pass was asked for with bazel-cache.run

-- A pass over every distinct host: the mac and each workspace's. Returns
-- the stats the dashboard shows: the totals, and per workspace (its host's).
local function pass()
  local was_manual = manual
  manual = false
  local cfg = config()
  local hosts, seen, by_host = { "local" }, { ["local"] = true }, {}
  for _, ws in ipairs(kitchen.workspaces()) do
    if not seen[ws.host] then
      seen[ws.host] = true
      table.insert(hosts, ws.host)
    end
  end
  local totals, freed_on, short_on, freed_all = { freed = "0K", hosts = #hosts, blocked = 0, failed = 0 }, {}, {}, 0
  for _, host in ipairs(hosts) do
    local ok, stats, freed, short = pcall(clean_host, host, cfg)
    if ok then
      by_host[host] = stats
      if stats.blocked then
        totals.blocked = totals.blocked + 1
      end
      if freed > 0 then
        freed_all = freed_all + freed
        table.insert(freed_on, M.human(freed) .. " on " .. host)
      end
      if short then
        table.insert(short_on, host .. " (" .. stats.cache .. ", " .. stats.free .. " free" .. (stats.blocked and ", " .. stats.blocked or "") .. ")")
      end
    else
      kitchen.log("[" .. host .. "] failed:", stats)
      by_host[host] = { failed = tostring(stats):sub(1, 120) }
      totals.failed = totals.failed + 1
    end
  end
  totals.freed = M.human(freed_all)
  local per_ws = {}
  for _, ws in ipairs(kitchen.workspaces()) do
    per_ws[ws.id] = by_host[ws.host]
  end
  if #freed_on > 0 then
    kitchen.notify("bazel-cache: freed " .. table.concat(freed_on, ", "))
  end
  if #short_on > 0 then
    kitchen.notify("Bazel caches still over budget on " .. table.concat(short_on, "; "), "warn")
  elseif #freed_on == 0 and was_manual then
    kitchen.notify("bazel-cache: nothing to clean (kitchenctl plugin logs bazel-cache says why)")
  end
  return totals, per_ws
end

-- The kitchen runs it on schedule and never starts one while another is going.
local timer = kitchen.every("pass", math.max(1, tonumber(kitchen.config.interval_minutes) or 30) * 60, pass, {
  desc = "keep Bazel caches under max_gb",
  first_delay = 120,
})

kitchen.command("run", function()
  manual = true
  if not timer:run() then
    manual = false
    kitchen.notify("a bazel-cache pass is already running")
  end
end, { desc = "clean Bazel caches now" })

return M
