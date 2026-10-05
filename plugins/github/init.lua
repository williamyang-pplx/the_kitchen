-- github: open a PR from its link in a read-only tab, and review it.
--
-- A PR is its repo + number. The tab checks out the PR head (fetched from
-- origin's refs/pull/N/head) in its own worktree, and the viewer diffs it
-- against the PR's base branch, like GitHub's "Files changed". gh (on the
-- mac) supplies the title and base branch; without it the workspace's
-- default branch is the base.
--
-- Review comments live on GitHub: the viewer shows the PR's comment threads
-- under their lines, and ⌘M … ⌘Enter posts a comment (or a reply, on a line
-- that has one) straight to the PR.

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

-- Review comments ---------------------------------------------------------

-- Re-read a PR's comments when a file opens and they're older than this.
local REFRESH = 60
-- One compact JSON object per comment (nulls dropped).
local JQ = ".[] | {id, in_reply_to_id, path, line, original_line, start_line, side, subject_type,"
  .. " user: .user.login, body, created_at} | with_entries(select(.value != null)) | @json"

local function gh(args, cb)
  local quoted = {}
  for i, a in ipairs(args) do
    quoted[i] = q(a)
  end
  local sh = "export PATH=" .. (kitchen.config.path or "") .. ':"$PATH"; gh ' .. table.concat(quoted, " ")
  kitchen.run({ sh = sh, host = "local", timeout = 30 }, cb)
end

local function why(res)
  local text = (res.stderr or "") .. " " .. (res.stdout or "")
  return (text:gsub("%s+", " "):gsub("^ ", ""):gsub(" $", ""))
end

-- The PR a worktree belongs to: tabs opened from PR links are named pr-<N>.
function M.pr_number(worktree)
  local n = worktree:match("%-kitchen%-ro%-pr%-(%d+)$")
  return n and tonumber(n)
end

-- gh comments → threads (a root comment and its replies), oldest first.
function M.threads(list)
  table.sort(list, function(a, b)
    return a.created_at < b.created_at
  end)
  local by_id, roots = {}, {}
  for _, c in ipairs(list) do
    local parent = c.in_reply_to_id and by_id[c.in_reply_to_id]
    if parent then
      table.insert(parent.replies, c)
      by_id[c.id] = parent
    else
      local t = { root = c, replies = {} }
      by_id[c.id] = t
      table.insert(roots, t)
    end
  end
  return roots
end

-- Threads on `path` as kitchen.comments items.
function M.items(threads, path)
  local out = {}
  for _, t in ipairs(threads) do
    local c = t.root
    if c.path == path then
      local outdated = c.line == nil and c.subject_type ~= "file"
      local last = c.line or c.original_line
      local where = (c.start_line and last and ("L" .. c.start_line .. "–" .. last)) or (last and ("L" .. last)) or "file"
      local title = "@" .. (c.user or "?") .. " · " .. where .. " · " .. (c.created_at or ""):sub(1, 10)
      if outdated then
        title = title .. " · outdated"
      end
      if #t.replies > 0 then
        title = title .. " · " .. #t.replies .. (#t.replies == 1 and " reply" or " replies")
      end
      local body = { c.body or "" }
      for _, r in ipairs(t.replies) do
        table.insert(body, "↳ @" .. (r.user or "?") .. ": " .. (r.body or ""))
      end
      table.insert(out, {
        id = tostring(c.id),
        line = (not outdated) and c.line or nil,
        side = c.side == "LEFT" and "old" or "new",
        title = title,
        body = table.concat(body, "\n\n"),
        dim = outdated,
      })
    end
  end
  return out
end

-- The gh api arguments that post `ev` (a viewer comment) at commit `sha`.
-- `file_level`: as a comment on the whole file (for lines outside the diff).
function M.post_args(slug, n, ev, sha, file_level)
  local args = { "api", "-X", "POST", "repos/" .. slug .. "/pulls/" .. n .. "/comments", "-f", "commit_id=" .. sha, "-f", "path=" .. ev.path }
  local function add(...)
    for _, a in ipairs({ ... }) do
      table.insert(args, a)
    end
  end
  if file_level then
    local quote = (ev.quote or ""):match("[^\n]*")
    add("-f", "subject_type=file", "-f", "body=**L" .. ev.line .. "**: `" .. quote .. "`\n\n" .. ev.text)
    return args
  end
  local side = function(s)
    return s == "old" and "LEFT" or "RIGHT"
  end
  add("-f", "body=" .. ev.text, "-F", "line=" .. ev.line, "-f", "side=" .. side(ev.side))
  if ev.start_line and not (ev.start_line == ev.line and ev.start_side == ev.side) then
    add("-F", "start_line=" .. ev.start_line, "-f", "start_side=" .. side(ev.start_side))
  end
  return args
end

local prs = {} -- "slug#n" → { slug, n, threads, at, files = {path → file}, drafts = {path → items} }

local function pr_for(worktree)
  local n = M.pr_number(worktree)
  if not n then
    return
  end
  for _, w in ipairs(kitchen.workspaces()) do
    for _, t in ipairs(w.tabs or {}) do
      if t.worktree == worktree and w.github_slug then
        local key = w.github_slug .. "#" .. n
        prs[key] = prs[key] or { slug = w.github_slug, n = n, files = {}, drafts = {} }
        return prs[key]
      end
    end
  end
end

local function render(pr)
  for path, file in pairs(pr.files) do
    local items = pr.threads and M.items(pr.threads, path) or {}
    for _, d in ipairs(pr.drafts[path] or {}) do
      table.insert(items, d)
    end
    kitchen.comments.set(file, items)
  end
end

local function fetch(pr)
  if pr.fetching then
    pr.again = true
    return
  end
  pr.fetching = true
  gh({ "api", "--paginate", "repos/" .. pr.slug .. "/pulls/" .. pr.n .. "/comments", "--jq", JQ }, function(res)
    pr.fetching = false
    if res.code ~= 0 then
      kitchen.notify("couldn't read #" .. pr.n .. "'s comments: " .. why(res), "warn")
    else
      local list = {}
      for line in res.stdout:gmatch("[^\n]+") do
        local ok, c = pcall(kitchen.json.decode, line)
        if ok and type(c) == "table" then
          table.insert(list, c)
        end
      end
      pr.threads, pr.at = M.threads(list), os.time()
      render(pr)
    end
    if pr.again then
      pr.again = false
      fetch(pr)
    end
  end)
end

kitchen.on("file_opened", function(ev)
  local pr = pr_for(ev.file.worktree)
  if not pr then
    return
  end
  pr.files[ev.file.path] = ev.file
  render(pr)
  if not pr.threads or os.time() - pr.at >= REFRESH then
    fetch(pr)
  end
end)

kitchen.on("comment", function(ev)
  local pr = pr_for(ev.file.worktree)
  if not pr then
    kitchen.notify("comments are posted to GitHub PRs — open the PR from its link (⌘I) to comment on it", "warn")
    return
  end
  pr.files[ev.path] = ev.file
  local draft = { line = ev.line, side = ev.side, title = "posting to #" .. pr.n .. "…", body = ev.text, dim = true }
  pr.drafts[ev.path] = pr.drafts[ev.path] or {}
  table.insert(pr.drafts[ev.path], draft)
  render(pr)
  local function done(res)
    if res.code == 0 then
      for i, d in ipairs(pr.drafts[ev.path]) do
        if d == draft then
          table.remove(pr.drafts[ev.path], i)
          break
        end
      end
      kitchen.notify("comment posted to " .. pr.slug .. "#" .. pr.n)
      fetch(pr)
    else
      draft.title, draft.dim = "⚠ not posted: " .. why(res) .. " (⌘M to try again)", false
      kitchen.notify("comment not posted: " .. why(res), "error")
      render(pr)
    end
  end
  if ev.reply_to then
    gh({ "api", "-X", "POST", "repos/" .. pr.slug .. "/pulls/" .. pr.n .. "/comments/" .. ev.reply_to .. "/replies", "-f", "body=" .. ev.text }, done)
    return
  end
  -- Lines are relative to the commit the tab shows, which may be behind the PR.
  kitchen.run({ cmd = { "git", "rev-parse", "HEAD" }, host = ev.file.host, cwd = ev.file.worktree, timeout = 15 }, function(head)
    local sha = head.code == 0 and head.stdout:match("%x+")
    if not sha or not ev.line then
      done({ code = 1, stderr = "can't tell the tab's commit" })
      return
    end
    gh(M.post_args(pr.slug, pr.n, ev, sha), function(res)
      -- GitHub only takes line comments inside the diff's hunks.
      if res.code ~= 0 and why(res):find("422") then
        gh(M.post_args(pr.slug, pr.n, ev, sha, true), done)
      else
        done(res)
      end
    end)
  end)
end)

kitchen.command("refresh_comments", function(ctx)
  local pr = ctx.worktree and pr_for(ctx.worktree)
  if pr then
    fetch(pr)
  else
    kitchen.notify("not a PR tab", "warn")
  end
end, { desc = "re-read this PR's review comments from GitHub" })

return M
