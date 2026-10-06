-- github: review a PR in the tab you're in. Paste its link into ⌘⇧P: the PR
-- head is checked out detached in <repo>-kitchen-pr-<N> (no tab, no agent)
-- and the tab's viewer shows its diff against the PR's base, with the PR's
-- review comments under their lines; ⌘M … ⌘Enter posts a comment (or a
-- reply, on a line that has one) to the PR. Each PR viewed gets a ⌘⇧P entry
-- (github.pr_<N>) to show it again; github.own_changes shows the tab's own
-- worktree again.

local M = {}

-- Re-read a PR's comments when a file opens and they're older than this.
local REFRESH = 60
-- One compact JSON object per comment (nulls dropped).
local JQ = ".[] | {id, in_reply_to_id, path, line, original_line, start_line, side, subject_type,"
  .. " user: .user.login, body, created_at} | with_entries(select(.value != null)) | @json"

-- The PRs whose worktrees the viewer has shown, by "<host>:<worktree path>"
-- (a repo can be a workspace both here and on a devbox).
local prs = {} -- key → { slug, n, threads, at, files = {path → file}, drafts = {path → items} }

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

local function q(s)
  return "'" .. tostring(s):gsub("'", "'\\''") .. "'"
end

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

local function basename(path)
  return (path:gsub("/+$", ""):match("[^/]+$"))
end

-- The PR whose view `f` ({host, worktree}: a viewer file or ctx.viewer) is
-- in: its worktree is <repo>-kitchen-pr-<N> beside a GitHub workspace's repo
-- on the same host. Rebuilt from the name, so it survives a plugin reload.
local function pr_of(f)
  local key = f.host .. ":" .. f.worktree
  if prs[key] then
    return prs[key]
  end
  local repo, n = basename(f.worktree):match("^(.+)%-kitchen%-pr%-(%d+)$")
  for _, w in ipairs(repo and kitchen.workspaces() or {}) do
    if w.host == f.host and w.github_slug and basename(w.repo_path) == repo then
      prs[key] = { slug = w.github_slug, n = tonumber(n), files = {}, drafts = {} }
      return prs[key]
    end
  end
end

-- The kitchen.view spec that shows PR `n` (gh's `info`) in tab `tab`: its
-- head detached in <repo>-kitchen-pr-<n>, diffed against its base (for a
-- merged PR, the base just before the merge; `dflt` if gh names none).
function M.view(tab, n, info, dflt)
  local rev = "refs/kitchen/pr/" .. n
  local spec = { tab = tab, rev = rev, name = "kitchen-pr-" .. n, fetch = { "+refs/pull/" .. n .. "/head:" .. rev } }
  if info.state == "MERGED" and info.mergeCommit and info.mergeCommit.oid then
    table.insert(spec.fetch, info.mergeCommit.oid)
    spec.base = info.mergeCommit.oid .. "^1"
  else
    local b = info.baseRefName or dflt
    table.insert(spec.fetch, "+refs/heads/" .. b .. ":refs/remotes/origin/" .. b)
    spec.base = "origin/" .. b
  end
  return spec
end

local open

-- Show PR `slug#n` in tab `tab` of workspace `ws`, and add it to ⌘⇧P.
local function show(ws, tab, slug, n)
  kitchen.notify("opening " .. slug .. "#" .. n .. "…")
  gh({ "pr", "view", tostring(n), "-R", slug, "--json", "title,baseRefName,state,mergeCommit" }, function(res)
    local ok, info = pcall(kitchen.json.decode, res.code == 0 and res.stdout or "")
    if not (ok and type(info) == "table") then
      return kitchen.notify("couldn't read " .. slug .. "#" .. n .. ": " .. why(res), "error")
    end
    kitchen.view(M.view(tab, n, info, ws.default_branch or "main"))
    -- (picked from another tab, `open` checks that tab's repo again)
    local title = (info.title or "") ~= "" and info.title or ("PR #" .. n)
    kitchen.command("pr_" .. n, function(ctx)
      open({ text = slug .. "#" .. n, workspace = ctx.workspace, tab = ctx.tab })
    end, { desc = slug .. "#" .. n .. " " .. title })
  end)
end

-- A link typed into ⌘⇧P: ev = { text, workspace, tab, … } (the tab it was typed in).
function open(ev)
  local slug, n = M.parse(ev.text)
  if not slug then
    if ev.text:find("github%.com/") then
      kitchen.notify("not a pull request link: " .. ev.text, "warn")
    end
    return
  end
  for _, w in ipairs(kitchen.workspaces()) do
    if w.id == ev.workspace then
      if not (w.github_slug and w.github_slug:lower() == slug:lower()) then
        return kitchen.notify(slug .. "#" .. n .. " is in another repo: open it from a tab of a " .. slug .. " workspace", "error")
      end
      return show(w, ev.tab, slug, n)
    end
  end
end
kitchen.on("open", open)

-- Review comments ---------------------------------------------------------

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
  local pr = pr_of(ev.file)
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
  local pr = pr_of(ev.file)
  if not pr then
    kitchen.notify("comments go to GitHub PRs: paste a PR link into ⌘⇧P to view one", "warn")
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
  local pr = ctx.viewer and pr_of({ host = ctx.host, worktree = ctx.viewer.worktree })
  if pr then
    fetch(pr)
  else
    kitchen.notify("the viewer isn't showing a PR", "warn")
  end
end, { desc = "re-read the shown PR's review comments from GitHub" })

kitchen.command("own_changes", function(ctx)
  kitchen.view({ tab = ctx.tab })
end, { desc = "show this tab's own worktree in its viewer again" })

return M
