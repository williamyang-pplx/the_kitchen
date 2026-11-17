-- kitchen.util: small helpers the bundled plugins share.
local M = {}

-- `s` as one single-quoted shell word.
function M.q(s)
  return "'" .. tostring(s):gsub("'", "'\\''") .. "'"
end

-- A config path → a shell word: a leading ~ becomes "$HOME", and it's
-- double-quoted so it can't split (`$`, `"`, `\` and backquotes escaped).
function M.shell_path(p)
  local home, rest = "", p
  if p == "~" or p:sub(1, 2) == "~/" then
    home, rest = "$HOME", p:sub(2)
  end
  return '"' .. home .. rest:gsub('[%$"\\`]', "\\%0") .. '"'
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

function M.basename(path)
  return path:match("[^/]+$") or path
end

-- kitchen.run_sync with kitchen.config.path on the PATH: kitchenctl and
-- claude live there when the kitchen was started from Finder (minimal PATH).
function M.sh(spec)
  spec.sh = "export PATH=" .. (kitchen.config.path or "") .. ':"$PATH"; ' .. spec.sh
  return kitchen.run_sync(spec)
end

-- A failed job's reason, for logs and toasts: stderr only, one line, at most
-- 200 characters. Never stdout: the token syncs' jobs print secrets there.
function M.why(res)
  if res.timed_out then
    return "timed out"
  end
  local text = (res.stderr or ""):gsub("%s+", " "):match("^%s*(.-)%s*$")
  if text == "" then
    text = "exit " .. tostring(res.code)
  end
  return text:sub(1, 200)
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

-- The token syncs' schedule: a `pass` timer every config.interval_minutes
-- (default 5) that calls pass(manual) and returns its stats, a `run` command
-- for a pass now (manual = true), and a `new-hosts` poll that starts a pass
-- within 10 s of a devbox workspace opening, not at the next pass: a tab
-- opened there before the tokens would come up logged out. Each host counts
-- once per load (so a box that can't be reached doesn't start a pass every
-- tick), and a reload counts every host as new.
-- opts = { pass, desc, run_desc, first_delay }
function M.sync_schedule(opts)
  local manual = false -- the next pass was asked for with <plugin>.run
  local tried = {} -- host → true once a pass of this load has started with it
  -- The kitchen runs it on schedule and never starts one while another runs.
  local timer = kitchen.every("pass", math.max(1, tonumber(kitchen.config.interval_minutes) or 5) * 60, function()
    local was_manual = manual
    manual = false
    for _, host in ipairs((M.hosts(kitchen.workspaces()))) do
      tried[host] = true
    end
    return opts.pass(was_manual)
  end, { desc = opts.desc, first_delay = opts.first_delay })

  kitchen.every("new-hosts", 10, function()
    for _, host in ipairs((M.hosts(kitchen.workspaces()))) do
      if not tried[host] then
        timer:run() -- false while a pass runs; then the next tick asks again
        return { new_host = host }
      end
    end
    return {}
  end, { desc = "start a pass when a devbox workspace opens", first_delay = opts.first_delay + 15 }) -- after the first pass

  kitchen.command("run", function()
    manual = true
    if not timer:run() then
      manual = false
      kitchen.notify(kitchen.name .. ": a pass is already running")
    end
  end, { desc = opts.run_desc })
end

return M
