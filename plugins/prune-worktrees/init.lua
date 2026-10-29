-- prune-worktrees: every interval_minutes, remove the worktrees nobody uses.
--
-- A worktree goes when it has no tab (in any workspace on its repo), no tmux
-- pane (kitchen's or yours) has its cwd in it, and nothing in it was touched
-- for idle_hours: no commit or checkout (the last entry of its HEAD reflog),
-- no edit to a modified, staged or untracked file. Uncommitted changes go
-- with it (`worktree remove --force`).
--
-- Commits are never lost: the branch stays, unless its tip is already on
-- origin/<default> (then it's deleted too), and a detached HEAD that no ref
-- contains keeps its worktree. The main checkout and spares are never
-- touched. `kitchenctl worktree remove` re-checks the tab at removal time.

local M = {}

local SOCKET = "kitchen" -- the kitchen's tmux socket (kitchen-core tmux::SOCKET_NAME)

local function q(s)
  return "'" .. tostring(s):gsub("'", "'\\''") .. "'"
end

local function basename(path)
  return path:match("[^/]+$") or path
end

-- True when `path` is `dir` or inside it. Both drop a leading /private
-- first: a pane's cwd is getcwd()'s spelling (/private/tmp), git's may not be.
local function within(path, dir)
  path, dir = path:gsub("^/private/", "/"), dir:gsub("^/private/", "/")
  return path == dir or path:sub(1, #dir + 1) == dir .. "/"
end

-- worktree.list entries that may be collected. `tabs`: the tabs of every
-- workspace on the repo (kitchen.workspaces()), also matched by branch /
-- worktree name in case a tab spells its path differently from git.
-- `panes`: cwd of every tmux pane on the workspace's host. Returns { path, branch (or nil), head } entries.
function M.candidates(list, tabs, panes)
  local branches, names = {}, {}
  for _, t in ipairs(tabs or {}) do
    if t.branch then
      branches[t.branch] = true
    end
    if t.worktree then
      names[basename(t.worktree)] = true
    end
  end
  local function in_pane(path)
    for _, p in ipairs(panes or {}) do
      if within(p, path) then
        return true
      end
    end
    return false
  end
  local out = {}
  -- (role "main" / "spare": the main checkout and the spare pool, never collected)
  for _, wt in ipairs(list) do
    local tabbed = wt.tab ~= nil or names[basename(wt.path)] or (wt.branch and branches[wt.branch])
    if wt.role == "edit" and not tabbed and not in_pane(wt.path) then
      table.insert(out, { path = wt.path, branch = wt.branch, head = wt.head })
    end
  end
  return out
end

-- Shell, run in the worktree on its host, that prints three lines:
--   touched 0|1    the last HEAD reflog entry, or the mtime of a modified,
--                  staged or untracked file, is within `minutes`
--   contained 0|1  on a branch, or (detached) some ref contains HEAD
--   on_default 0|1 HEAD is an ancestor of `default_ref`
-- The reflog counts by the time in its last entry, not the file's mtime:
-- `git gc` (which a fetch may start) rewrites every worktree's reflog.
-- It fails (non-zero, before any output) when a git listing fails, so a
-- broken probe keeps the worktree instead of reading as "not touched".
function M.probe_sh(minutes, default_ref)
  minutes = math.floor(minutes)
  return table.concat({
    "set -e",
    "l=$(mktemp); trap 'rm -f \"$l\"' EXIT",
    -- an assignment, so set -e stops here outside a repo
    "h=$(git rev-parse --git-path logs/HEAD)",
    -- "<old> <new> <name> <<email>> <time> <tz>\t<message>"; no reflog: 0
    [[r=$(tail -n 1 "$h" 2>/dev/null | sed -n 's/^[^>]*> \([0-9][0-9]*\) .*/\1/p')]],
    "[ $((${r:-0} + " .. minutes * 60 .. ')) -gt "$(date +%s)" ] && t=1 || t=',
    [[git ls-files -z -m -o --exclude-standard > "$l"]],
    [[git diff --cached -z --name-only >> "$l"]],
    -- paths before primaries (BSD find insists); deleted files just error;
    -- no paths, no find (GNU's would look at ".")
    [[[ -n "$t" ] || t=$(xargs -0 sh -c '[ $# = 0 ] || find "$@" -maxdepth 0 -mmin -]] .. minutes .. [[ -print 2>/dev/null; true' _ < "$l" | head -n1)]],
    [[if [ -n "$t" ]; then echo "touched 1"; else echo "touched 0"; fi]],
    -- for-each-ref --contains walks every ref, so only when detached
    [[if git symbolic-ref -q HEAD >/dev/null || [ -n "$(git for-each-ref --contains HEAD --count=1)" ]; then echo "contained 1"; else echo "contained 0"; fi]],
    "if git merge-base --is-ancestor HEAD " .. q(default_ref) .. ' 2>/dev/null; then echo "on_default 1"; else echo "on_default 0"; fi',
  }, "\n")
end

-- { touched, contained, on_default } (booleans) from probe_sh's output, or
-- nil when a line is missing (the probe failed: keep the worktree).
function M.parse_probe(stdout)
  local p = {}
  for _, key in ipairs({ "touched", "contained", "on_default" }) do
    local v = (stdout or ""):match(key .. " ([01])")
    if not v then
      return nil
    end
    p[key] = v == "1"
  end
  return p
end

-- What to do with candidate `wt` given its probe (nil: it failed) and the
-- workspace's default branch. Returns { action = "remove" | "keep", why, delete_branch }.
function M.verdict(wt, probe, default_branch)
  if not probe then
    return { action = "keep", why = "probe failed" }
  end
  if probe.touched then
    return { action = "keep", why = "touched recently" }
  end
  if not wt.branch and not probe.contained then
    return { action = "keep", why = "detached commits on no ref" }
  end
  local delete = wt.branch ~= nil and wt.branch ~= default_branch and probe.on_default
  return { action = "remove", why = "idle", delete_branch = delete }
end

-- Shell line that deletes `branch` only while its tip is still `oid` (the
-- removed worktree's HEAD, already on the default branch). `-D` because `-d`
-- checks against the main checkout's HEAD, not origin/<default>.
function M.delete_branch_sh(branch, oid)
  return '[ "$(git rev-parse --verify -q ' .. q("refs/heads/" .. branch) .. ')" = ' .. q(oid) .. " ] && git branch -D " .. q(branch)
end

-- run_sync with kitchenctl on the PATH.
local function sh(spec)
  spec.sh = "export PATH=" .. (kitchen.config.path or "") .. ':"$PATH"; ' .. spec.sh
  return kitchen.run_sync(spec)
end

local function why(res)
  if res.timed_out then
    return "timed out"
  end
  local text = (res.stderr ~= "" and res.stderr or res.stdout or ""):gsub("%s+", " ")
  return text
end

-- cwd of every tmux pane on `host`, on the kitchen's socket and the default
-- one. A server that isn't running answers nothing; that's fine.
local function panes_on(host)
  local fmt = " list-panes -a -F '#{pane_current_path}' 2>/dev/null; "
  local res = kitchen.run_sync({ host = host, timeout = 30, sh = "tmux -L " .. SOCKET .. fmt .. "tmux" .. fmt .. "true" })
  if res.code ~= 0 then
    -- (ssh failed: without the panes every worktree would look unused)
    error("tmux panes: " .. why(res))
  end
  local panes = {}
  for line in res.stdout:gmatch("[^\n]+") do
    table.insert(panes, line)
  end
  return panes
end

-- One pass over workspace `ws`; `tabs`: those of every workspace on its repo
-- (worktree.list only knows ws's own). Returns the removed worktree paths and
-- how many it kept.
local function prune_workspace(ws, tabs, minutes)
  local log = function(...)
    kitchen.log("[" .. ws.name .. "]", ...)
  end
  local res = sh({ host = "local", timeout = 60, sh = "kitchenctl --json worktree list -w " .. q(ws.id) })
  local ok, list = pcall(kitchen.json.decode, res.stdout)
  if res.code ~= 0 or not ok or type(list) ~= "table" then
    log("worktree list failed:", why(res))
    error("worktree list failed: " .. why(res))
  end
  local panes = panes_on(ws.host)
  local default = ws.default_branch or "main"
  local removed, kept = {}, 0
  for _, wt in ipairs(M.candidates(list, tabs, panes)) do
    local probe = kitchen.run_sync({ host = ws.host, cwd = wt.path, timeout = 120, sh = M.probe_sh(minutes, "refs/remotes/origin/" .. default) })
    local v = M.verdict(wt, probe.code == 0 and M.parse_probe(probe.stdout) or nil, default)
    if v.action == "remove" then
      -- A tab being opened checks a worktree out before its tab exists.
      local head = kitchen.run_sync({ host = ws.host, cmd = { "git", "-C", wt.path, "rev-parse", "HEAD" } })
      if head.code ~= 0 or head.stdout:gsub("%s+", "") ~= wt.head then
        v = { action = "keep", why = "HEAD moved" }
      end
    end
    if v.action == "keep" then
      log("kept", wt.path, "(" .. v.why .. ")")
      kept = kept + 1
    else
      -- Deleting a big checkout on a devbox takes minutes. A timeout only
      -- kills kitchenctl: the app carries on, so the outcome is unknown.
      local rm = sh({ host = "local", timeout = 900, sh = "kitchenctl --json worktree remove --force " .. q(wt.path) .. " -w " .. q(ws.id) })
      if rm.timed_out then
        log("gave up waiting on", wt.path, "(" .. v.why .. "); the removal may still finish, branch kept")
      elseif rm.code ~= 0 then
        log("kept", wt.path, "(" .. v.why .. ", but remove refused: " .. why(rm) .. ")")
        kept = kept + 1
      else
        log("removed", wt.path, "(" .. v.why .. ")")
        table.insert(removed, wt.path)
        if v.delete_branch then
          local del = kitchen.run_sync({ host = ws.host, cwd = ws.repo_path, sh = M.delete_branch_sh(wt.branch, wt.head) })
          log(del.code == 0 and "deleted branch" or "kept branch", wt.branch, del.code == 0 and "" or why(del))
        end
      end
    end
  end
  return removed, kept
end

local manual = false -- the next pass was asked for with prune-worktrees.run

-- A pass over every workspace. Returns the stats the dashboard shows: the
-- totals, and per workspace.
local function pass()
  local was_manual = manual
  manual = false
  local minutes = math.max(60, (tonumber(kitchen.config.idle_hours) or 6) * 60)
  local removed, totals, per_ws = {}, { removed = 0, kept = 0, failed_workspaces = 0 }, {}
  local workspaces = kitchen.workspaces()
  -- Several workspaces can share a repo: a tab in any of them keeps its worktree.
  local tabs = {}
  for _, ws in ipairs(workspaces) do
    local key = ws.host .. ":" .. ws.repo_path
    tabs[key] = tabs[key] or {}
    for _, t in ipairs(ws.tabs or {}) do
      table.insert(tabs[key], t)
    end
  end
  for _, ws in ipairs(workspaces) do
    local ok, got, kept = pcall(prune_workspace, ws, tabs[ws.host .. ":" .. ws.repo_path], minutes)
    if ok then
      for _, p in ipairs(got) do
        table.insert(removed, basename(p))
      end
      per_ws[ws.id] = { removed = #got, kept = kept }
      totals.removed, totals.kept = totals.removed + #got, totals.kept + kept
    else
      kitchen.log("[" .. ws.name .. "] failed:", got)
      per_ws[ws.id] = { failed = tostring(got):sub(1, 120) }
      totals.failed_workspaces = totals.failed_workspaces + 1
    end
  end
  if #removed > 0 then
    kitchen.notify("pruned " .. #removed .. " worktree(s): " .. table.concat(removed, ", "))
  elseif was_manual then
    kitchen.notify("nothing to prune (kitchenctl plugin logs prune-worktrees says why)")
  end
  return totals, per_ws
end

-- The kitchen runs it on schedule and never starts one while another is going.
local timer = kitchen.every("pass", math.max(1, tonumber(kitchen.config.interval_minutes) or 60) * 60, pass, {
  desc = "remove worktrees idle for idle_hours (kept: no tab or pane, but used recently or not safe to remove)",
  first_delay = 60,
})

kitchen.command("run", function()
  manual = true
  if not timer:run() then
    manual = false
    kitchen.notify("a prune pass is already running")
  end
end, { desc = "remove idle worktrees now" })

return M
