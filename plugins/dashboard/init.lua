-- dashboard: what the workspace's host is doing, on its dashboard (⌘D).
--
-- While a workspace's dashboard is on screen, every refresh_seconds this
-- counts its worktrees (`kitchenctl worktree list`) and reads RAM and the
-- biggest processes on its host (the mac or the devbox). Disk (df, and du of
-- the folders next to the repo plus the workspace's worktrees) is slow on big
-- checkouts, so it is measured in the background every disk_minutes and
-- cached. The kitchen draws the sections and the plugin timers itself.

local M = {}

local function q(s)
  return "'" .. tostring(s):gsub("'", "'\\''") .. "'"
end

local function dirname(path)
  return path:match("^(.*)/[^/]+$") or "/"
end

local function basename(path)
  return path:match("[^/]+$") or path
end

-- KiB → "1.9G", "512M", "40K".
function M.human(k)
  if k >= 1024 * 1024 then
    return string.format("%.1fG", k / 1024 / 1024)
  elseif k >= 1024 then
    return string.format("%.0fM", k / 1024)
  end
  return string.format("%.0fK", k)
end

-- Worktree counts from `kitchenctl --json worktree list`.
function M.worktree_counts(list)
  local c = { total = 0, edit = 0, edit_tabs = 0, spare = 0 }
  for _, w in ipairs(list) do
    c.total = c.total + 1
    if w.role == "edit" then
      c.edit = c.edit + 1
      if w.tab then
        c.edit_tabs = c.edit_tabs + 1
      end
    elseif w.role == "spare" then
      c.spare = c.spare + 1
    end
  end
  return c
end

-- RAM: "mem <total KiB> <used KiB>", then "proc <rss KiB> <name>" summed per
-- command name. Linux (devboxes) and macOS.
M.MEM_SH = [[
if [ "$(uname)" = Darwin ]; then
  vm_stat | awk -v ps="$(sysctl -n hw.pagesize)" -v t="$(sysctl -n hw.memsize)" \
    '/Pages active/ {a=$3} /Pages wired/ {w=$4} /occupied by compressor/ {c=$5} END {printf "mem %d %d\n", t/1024, (a+w+c)*ps/1024}'
  ps -axo rss=,comm=
else
  awk '/^MemTotal/ {t=$2} /^MemAvailable/ {a=$2} END {printf "mem %d %d\n", t, t-a}' /proc/meminfo
  ps -eo rss=,comm=
fi | awk '/^mem / {print; next} {r=$1; $1=""; n=$0; sub(/^ +/, "", n); sub(/.*\//, "", n); s[n]+=r} END {for (n in s) printf "proc %d %s\n", s[n], n}'
]]

-- Disk: "df <size KiB> <used KiB> <mount>" for the volume holding `parent`,
-- then "du <KiB> <path>" for each folder in `parent` and each of `extra`.
-- `parent` is resolved first (/tmp → /private/tmp), so its folders are
-- spelled like git spells worktrees, and a path given twice is listed once.
function M.disk_sh(parent, extra)
  local paths = { '"$p"/*' }
  for _, p in ipairs(extra or {}) do
    table.insert(paths, q(p))
  end
  return "p=$(cd " .. q(parent) .. " && pwd -P) || exit 1; "
    .. "df -Pk \"$p\" | awk 'NR==2 {printf \"df %d %d %s\\n\", $2, $3, $6}'; "
    .. "du -sk " .. table.concat(paths, " ") .. " 2>/dev/null | awk '{k=$1; $1=\"\"; sub(/^ +/, \"\"); if (!seen[$0]++) printf \"du %d %s\\n\", k, $0}'"
end

-- The lines above → { mem = {total, used}, procs = {{k, name}}, df = {size, used, mount}, dirs = {{k, path}} },
-- procs and dirs biggest first.
function M.parse(text)
  local out = { procs = {}, dirs = {} }
  for line in text:gmatch("[^\n]+") do
    local kind, rest = line:match("^(%a+) (.*)$")
    if kind == "mem" then
      local t, u = rest:match("^(%d+) (%d+)")
      out.mem = { total = tonumber(t), used = tonumber(u) }
    elseif kind == "proc" then
      local k, name = rest:match("^(%d+) (.+)$")
      if k then
        table.insert(out.procs, { k = tonumber(k), name = name })
      end
    elseif kind == "df" then
      local s, u, m = rest:match("^(%d+) (%d+) (.*)$")
      out.df = { size = tonumber(s), used = tonumber(u), mount = m }
    elseif kind == "du" then
      local k, p = rest:match("^(%d+) (.+)$")
      if k then
        table.insert(out.dirs, { k = tonumber(k), path = p })
      end
    end
  end
  table.sort(out.procs, function(a, b) return a.k > b.k end)
  table.sort(out.dirs, function(a, b) return a.k > b.k end)
  return out
end

-- The dashboard sections. `counts` from M.worktree_counts, `mem` and `disk`
-- from M.parse (either may be nil: not measured yet), `paths` the workspace's
-- worktree paths (their folders are marked), `top` rows per list.
function M.sections(counts, mem, disk, paths, top, measured_at)
  local out = {}
  if counts then
    local edit = tostring(counts.edit) .. (counts.edit_tabs > 0 and (" (" .. counts.edit_tabs .. " with tabs)") or "")
    table.insert(out, {
      title = "Worktrees",
      note = counts.total .. " total",
      rows = {
        { label = "checkouts", value = edit },
        { label = "spare", value = tostring(counts.spare) },
      },
    })
  end
  local disk_rows = {}
  if disk and disk.df then
    table.insert(disk_rows, {
      label = disk.df.mount,
      value = M.human(disk.df.used) .. " / " .. M.human(disk.df.size),
      fraction = disk.df.size > 0 and disk.df.used / disk.df.size or nil,
    })
    for i, d in ipairs(disk.dirs) do
      if i > top then
        break
      end
      local mine = false
      for _, p in ipairs(paths or {}) do
        if p == d.path or p:sub(1, #d.path + 1) == d.path .. "/" then
          mine = true
        end
      end
      table.insert(disk_rows, { label = basename(d.path), value = M.human(d.k), fraction = disk.df.size > 0 and d.k / disk.df.size or nil, mark = mine })
    end
  end
  table.insert(out, {
    title = "Disk",
    note = measured_at and ("measured " .. os.date("%H:%M", measured_at) .. " · blue = this workspace") or "measuring…",
    rows = disk_rows,
  })
  if mem and mem.mem then
    local rows = { { label = "used", value = M.human(mem.mem.used) .. " / " .. M.human(mem.mem.total), fraction = mem.mem.total > 0 and mem.mem.used / mem.mem.total or nil } }
    for i, p in ipairs(mem.procs) do
      if i > top then
        break
      end
      table.insert(rows, { label = p.name, value = M.human(p.k), fraction = mem.mem.total > 0 and p.k / mem.mem.total or nil })
    end
    table.insert(out, { title = "RAM", rows = rows })
  end
  return out
end

-- ------------------------------------------------------------------ live

local cfg = kitchen.config
local top = tonumber(cfg.top) or 10
local disk_secs = math.max(1, tonumber(cfg.disk_minutes) or 30) * 60
-- By workspace id: { counts, paths, mem, disk, disk_at, measuring }.
local cache = {}

local function entry(id)
  cache[id] = cache[id] or {}
  return cache[id]
end

local function workspace(id)
  for _, ws in ipairs(kitchen.workspaces()) do
    if ws.id == id then
      return ws
    end
  end
end

local function publish(ws)
  local c = entry(ws.id)
  kitchen.dashboard.set(ws.id, M.sections(c.counts, c.mem, c.disk, c.paths, top, c.disk_at))
end

-- Worktrees and RAM, now.
local function refresh(ws)
  local c = entry(ws.id)
  local res = kitchen.run_sync({
    host = "local",
    timeout = 60,
    sh = "export PATH=" .. (cfg.path or "") .. ':"$PATH"; kitchenctl --json worktree list -w ' .. q(ws.id),
  })
  local ok, list = pcall(kitchen.json.decode, res.stdout)
  if res.code == 0 and ok and type(list) == "table" then
    c.counts = M.worktree_counts(list)
    c.paths = {}
    for _, w in ipairs(list) do
      table.insert(c.paths, w.path)
    end
  else
    kitchen.log("[" .. ws.name .. "] worktree list failed:", res.stderr)
  end
  local mem = kitchen.run_sync({ host = ws.host, timeout = 30, sh = M.MEM_SH })
  if mem.code == 0 then
    c.mem = M.parse(mem.stdout)
  else
    kitchen.log("[" .. ws.name .. "] memory failed:", mem.stderr)
  end
  publish(ws)
end

-- Disk, in the background (du can take minutes on a devbox).
local function measure_disk(ws)
  local c = entry(ws.id)
  if c.measuring then
    return false
  end
  c.measuring = true
  local parent = dirname(ws.repo_path)
  local extra = {}
  for _, p in ipairs(c.paths or {}) do
    if dirname(p) ~= parent then
      table.insert(extra, p)
    end
  end
  kitchen.run({ host = ws.host, timeout = 1800, sh = M.disk_sh(parent, extra) }, function(res)
    c.measuring = false
    if res.code ~= 0 and res.stdout == "" then
      kitchen.log("[" .. ws.name .. "] disk failed:", res.timed_out and "timed out" or res.stderr)
      return
    end
    c.disk, c.disk_at = M.parse(res.stdout), os.time()
    local now = workspace(ws.id)
    if now then
      publish(now)
    end
  end)
  return true
end

local function showing()
  local out = {}
  for _, id in ipairs(kitchen.dashboard.showing()) do
    local ws = workspace(id)
    if ws then
      table.insert(out, ws)
    end
  end
  return out
end

kitchen.every("refresh", math.max(5, tonumber(cfg.refresh_seconds) or 30), function()
  local shown = showing()
  for _, ws in ipairs(shown) do
    refresh(ws)
    if not entry(ws.id).disk_at then
      measure_disk(ws) -- e.g. after a reload, which forgets the last measurement
    end
  end
  return { workspaces_shown = #shown }
end, { desc = "worktrees and RAM for the dashboard on screen" })

kitchen.every("disk", disk_secs, function()
  local started = 0
  for _, ws in ipairs(showing()) do
    if measure_disk(ws) then
      started = started + 1
    end
  end
  return { measurements_started = started }
end, { desc = "disk use (du) for the dashboard on screen" })

kitchen.on("dashboard", function(ev)
  local ws = ev.open and workspace(ev.workspace)
  if not ws then
    return
  end
  refresh(ws)
  local c = entry(ws.id)
  if not c.disk_at or os.time() - c.disk_at >= disk_secs then
    measure_disk(ws)
  end
end)

return M
