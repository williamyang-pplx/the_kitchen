-- github: open a PR from its link in a read-only tab.
--
-- A PR is its repo + number. The tab checks out the PR head (fetched from
-- origin's refs/pull/N/head) in its own worktree, and the viewer diffs it
-- against the PR's base branch, like GitHub's "Files changed". gh (on the
-- mac) supplies the title and base branch; without it the workspace's
-- default branch is the base.

local M = {}

-- "https://github.com/o/r/pull/12/changes", "github.com/o/r/pull/12", "o/r#12"
function M.parse(text)
  local owner, repo, n = text:match("github%.com/([%w_.-]+)/([%w_.-]+)/pull/(%d+)")
  if not owner then
    owner, repo, n = text:match("^%s*([%w_.-]+)/([%w_.-]+)#(%d+)%s*$")
  end
  if owner then
    return owner .. "/" .. repo, tonumber(n)
  end
end

local function workspace_for(slug, workspaces)
  local found
  for _, w in ipairs(workspaces) do
    if w.github_slug and w.github_slug:lower() == slug:lower() then
      if w.selected then
        return w
      end
      found = found or w
    end
  end
  return found
end

local function q(s)
  return "'" .. tostring(s):gsub("'", "'\\''") .. "'"
end

-- The tab for PR `n` of `ws`, given gh's answer (`nil` when gh failed).
function M.spec(ws, n, info)
  local base = (info and info.baseRefName) or ws.default_branch or "main"
  local head_ref = "refs/kitchen/pr/" .. n
  local fetch = { "+refs/pull/" .. n .. "/head:" .. head_ref }
  local diff_base
  if info and info.state == "MERGED" and info.mergeCommit and info.mergeCommit.oid then
    -- The base branch has moved past the PR (and may be gone): diff against
    -- the base as it was just before the merge.
    table.insert(fetch, info.mergeCommit.oid)
    diff_base = info.mergeCommit.oid .. "^1"
  else
    table.insert(fetch, "+refs/heads/" .. base .. ":refs/remotes/origin/" .. base)
    diff_base = "origin/" .. base
  end
  local title = "PR #" .. n
  if info and info.title and info.title ~= "" then
    title = "#" .. n .. " " .. info.title
  end
  return { workspace = ws.id, name = "pr-" .. n, rev = head_ref, fetch = fetch, diff_base = diff_base, title = title }
end

kitchen.on("open", function(ev)
  local slug, n = M.parse(ev.text)
  if not slug then
    if ev.text:find("github%.com/") then
      kitchen.notify("not a pull request link: " .. ev.text, "warn")
    end
    return
  end
  local ws = workspace_for(slug, ev.workspaces)
  if not ws then
    kitchen.notify("no workspace for " .. slug .. " — add one with ⌘N", "error")
    return
  end
  kitchen.notify("opening " .. slug .. "#" .. n .. "…")
  local sh = "export PATH=" .. (kitchen.config.path or "") .. ':"$PATH"; gh pr view ' .. n .. " -R " .. q(slug)
    .. " --json title,baseRefName,state,mergeCommit"
  kitchen.run({ sh = sh, host = "local", timeout = 20 }, function(res)
    local info
    if res.code == 0 then
      local ok, v = pcall(kitchen.json.decode, res.stdout)
      info = ok and v or nil
    end
    if not info then
      local why = (res.stderr ~= "" and res.stderr or res.stdout):gsub("%s+$", "")
      kitchen.notify("gh pr view failed (" .. why .. ") — diffing against " .. (ws.default_branch or "main"), "warn")
    end
    kitchen.tab_open(M.spec(ws, n, info))
  end)
end)

return M
