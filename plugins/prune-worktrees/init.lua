-- prune-worktrees: every interval_minutes, remove the worktrees of PRs that
-- are done.
--
-- Two kinds go: edit worktrees whose branch's PRs are all merged/closed, and
-- PR review worktrees (<repo>-kitchen-ro-pr-<N>, opened by the github plugin)
-- whose PR #N is merged/closed. Neither may have an open tab.
--
-- Per workspace (on its own host): `kitchenctl worktree list` says which
-- worktrees are edit / review worktrees with no tab; gh (on the mac) says
-- whether their PRs are done and which commit GitHub has. An edit worktree
-- goes only when its HEAD is that commit. `kitchenctl worktree remove` (never
-- --force) re-checks the tab and refuses dirty worktrees at removal time.
-- Anything doubtful is kept and logged.

local M = {}

local function q(s)
  return "'" .. tostring(s):gsub("'", "'\\''") .. "'"
end

local function basename(path)
  return path:match("[^/]+$") or path
end

-- worktree.list entries that may be pruned, before asking GitHub. `tabs`: the
-- workspace's tabs from kitchen.workspaces(). Each returned entry gets
-- `kind = "edit" | "pr"`, and `number` for "pr". Tabs are also matched by
-- branch / worktree name, in case a tab spells its path differently from git.
function M.candidates(list, tabs)
  local branches, names = {}, {}
  for _, t in ipairs(tabs or {}) do
    if t.branch then
      branches[t.branch] = true
    end
    if t.worktree then
      names[basename(t.worktree)] = true
    end
  end
  local out = {}
  for _, wt in ipairs(list) do
    if wt.tab == nil and not names[basename(wt.path)] then
      local n = wt.path:match("%-kitchen%-ro%-pr%-(%d+)$")
      if wt.role == "edit" and wt.branch and not branches[wt.branch] then
        table.insert(out, { kind = "edit", path = wt.path, branch = wt.branch, head = wt.head })
      elseif wt.role == "readonly" and n then
        table.insert(out, { kind = "pr", path = wt.path, number = tonumber(n), head = wt.head })
      end
    end
  end
  return out
end

-- What to do with candidate `wt` given its PRs: the branch's PR list for
-- kind "edit", { PR #number } for kind "pr".
-- Returns { action = "remove" | "keep", why, pr, delete_branch }.
function M.verdict(wt, prs)
  if #prs == 0 then
    return { action = "keep", why = "no PR" }
  end
  local newest
  for _, pr in ipairs(prs) do
    if pr.state == "OPEN" then
      return { action = "keep", why = "PR #" .. pr.number .. " open", pr = pr }
    end
    if not newest or pr.number > newest.number then
      newest = pr
    end
  end
  local done = "PR #" .. newest.number .. " " .. newest.state:lower()
  if wt.kind == "pr" then
    -- A detached read-only checkout of the PR head: no branch, no local work.
    return { action = "remove", why = done, pr = newest, delete_branch = false }
  end
  if not newest.headRefOid or wt.head ~= newest.headRefOid then
    return { action = "keep", why = "local commits not on PR #" .. newest.number, pr = newest }
  end
  return { action = "remove", why = done, pr = newest, delete_branch = newest.state == "MERGED" }
end

-- Shell line that deletes `branch` only while its tip is still `oid` (the
-- merged PR's head, so the work is on GitHub). `-D` because `-d` refuses
-- squash-merged branches.
function M.delete_branch_sh(branch, oid)
  return '[ "$(git rev-parse --verify -q ' .. q("refs/heads/" .. branch) .. ')" = ' .. q(oid) .. " ] && git branch -D " .. q(branch)
end

-- run_sync with gh and kitchenctl on the PATH.
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

-- gh's PRs for candidate `wt`, or nil (and a reason) when gh failed.
local function prs_for(slug, wt)
  local cmd
  if wt.kind == "pr" then
    cmd = "gh pr view " .. wt.number .. " -R " .. q(slug) .. " --json number,state,url"
  else
    cmd = "gh pr list -R " .. q(slug) .. " --head " .. q(wt.branch) .. " --state all --json number,state,url,headRefOid --limit 10"
  end
  local res = sh({ host = "local", timeout = 20, sh = cmd })
  if res.code ~= 0 then
    return nil, "gh failed: " .. why(res)
  end
  local ok, v = pcall(kitchen.json.decode, res.stdout)
  if not ok or type(v) ~= "table" then
    return nil, "gh answered " .. res.stdout
  end
  if wt.kind == "pr" then
    return { v }
  end
  return v
end

-- One pass over workspace `ws`; returns the removed worktree paths and how many it kept.
local function prune_workspace(ws)
  local log = function(...)
    kitchen.log("[" .. ws.name .. "]", ...)
  end
  if not ws.github_slug then
    log("not a GitHub repo, skipped")
    return {}, 0
  end
  local res = sh({ host = "local", timeout = 60, sh = "kitchenctl --json worktree list -w " .. q(ws.id) })
  local ok, list = pcall(kitchen.json.decode, res.stdout)
  if res.code ~= 0 or not ok or type(list) ~= "table" then
    log("worktree list failed:", why(res))
    error("worktree list failed: " .. why(res))
  end
  local removed, kept = {}, 0
  for _, wt in ipairs(M.candidates(list, ws.tabs)) do
    local prs, err = prs_for(ws.github_slug, wt)
    local v = prs and M.verdict(wt, prs) or { action = "keep", why = err }
    if v.action == "remove" and wt.kind == "edit" then
      -- worktree.remove doesn't re-check HEAD: a commit made since the list must not be lost.
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
      local rm = sh({ host = "local", timeout = 900, sh = "kitchenctl --json worktree remove " .. q(wt.path) .. " -w " .. q(ws.id) })
      if rm.timed_out then
        log("gave up waiting on", wt.path, "(" .. v.why .. "); the removal may still finish, branch kept")
      elseif rm.code ~= 0 then
        log("kept", wt.path, "(" .. v.why .. ", but remove refused: " .. why(rm) .. ")")
        kept = kept + 1
      else
        log("removed", wt.path, "(" .. v.why .. ")")
        table.insert(removed, wt.path)
        if v.delete_branch then
          local del = kitchen.run_sync({ host = ws.host, cwd = ws.repo_path, sh = M.delete_branch_sh(wt.branch, v.pr.headRefOid) })
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
  local removed, totals, per_ws = {}, { removed = 0, kept = 0, failed_workspaces = 0 }, {}
  for _, ws in ipairs(kitchen.workspaces()) do
    local ok, got, kept = pcall(prune_workspace, ws)
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
  desc = "remove worktrees of merged/closed PRs",
  first_delay = 60,
})

kitchen.command("run", function()
  manual = true
  if not timer:run() then
    manual = false
    kitchen.notify("a prune pass is already running")
  end
end, { desc = "prune worktrees of merged/closed PRs now" })

return M
