-- github: review a PR in the tab you're in. Paste its link into ⌘⇧P: the PR
-- head is checked out detached in <repo>-kitchen-pr-<N> (no tab, no agent)
-- and the tab's viewer shows its diff against the PR's base, with the PR's
-- review comments under their lines; ⌘M … ⌘Enter posts a comment (or a
-- reply, on a line that has one) to the PR. Each PR viewed gets a ⌘⇧P entry
-- (github.pr_<N>) to show it again; github.own_changes shows the tab's own
-- worktree again. ⌘⇧S lists the PRs in the tab's $KITCHEN_PRS_FILE, one
-- section per stack, then the PRs opened in the tab with ⌘⇧P. ⌘⇧D shows the
-- shown PR's description, rendered as on GitHub, and goes back.

local M = {}

-- Re-read a PR's comments when a file opens and they're older than this.
local REFRESH = 60
-- One compact JSON object per comment (nulls dropped).
local JQ = ".[] | {id, in_reply_to_id, path, line, original_line, start_line, side, subject_type,"
  .. " user: .user.login, body, created_at} | with_entries(select(.value != null)) | @json"

-- The PRs whose worktrees the viewer has shown, by "<host>:<worktree path>"
-- (a repo can be a workspace both here and on a devbox).
local prs = {} -- key → { slug, n, threads, at, files = {path → file}, drafts = {path → items} }

-- PRs opened with ⌘⇧P, by tab id: { [tab] = { n, … } } (in memory; a reload forgets them).
local viewed = {}

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
-- merged PR, the base just before the merge; `dflt` if gh names none). With
-- the commits GitHub reports (`headRefOid`, `baseRefOid`) the fetch is
-- skipped when they are already here.
function M.view(tab, n, info, dflt)
  local rev = "refs/kitchen/pr/" .. n
  local spec = { tab = tab, rev = rev, name = "kitchen-pr-" .. n, fetch = { "+refs/pull/" .. n .. "/head:" .. rev } }
  local base_have
  if info.state == "MERGED" and info.mergeCommit and info.mergeCommit.oid then
    table.insert(spec.fetch, info.mergeCommit.oid)
    spec.base = info.mergeCommit.oid .. "^1"
    base_have = { rev = info.mergeCommit.oid, commit = info.mergeCommit.oid }
  else
    local b = info.baseRefName or dflt
    table.insert(spec.fetch, "+refs/heads/" .. b .. ":refs/remotes/origin/" .. b)
    spec.base = "origin/" .. b
    base_have = info.baseRefName and info.baseRefOid and { rev = "refs/remotes/origin/" .. b, commit = info.baseRefOid }
  end
  if info.headRefOid and base_have then
    spec.have = { { rev = rev, commit = info.headRefOid }, base_have }
  end
  return spec
end

local open

-- Show PR `slug#n` in tab `tab` of workspace `ws`, and add it to ⌘⇧P.
local function show(ws, tab, slug, n)
  kitchen.notify("opening " .. slug .. "#" .. n .. "…")
  viewed[tab] = viewed[tab] or {}
  if not M.contains(viewed[tab], n) then
    table.insert(viewed[tab], n)
  end
  gh({ "pr", "view", tostring(n), "-R", slug, "--json", "title,baseRefName,baseRefOid,headRefOid,state,mergeCommit" }, function(res)
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

-- The PR menu (⌘⇧S) ----------------------------------------------------

function M.contains(list, x)
  for _, y in ipairs(list) do
    if y == x then
      return true
    end
  end
  return false
end

-- PR numbers in `lines` (the tab's PR list) that belong to `slug`: "123",
-- "#123", a PR link or "o/r#123". Others are skipped; first occurrence wins.
function M.list(lines, slug)
  local out = {}
  for _, line in ipairs(lines or {}) do
    local n = tonumber(line:match("^%s*#?(%d+)%s*$"))
    if not n then
      local s, m = M.parse(line)
      n = s and s:lower() == slug:lower() and m or nil
    end
    if n and not M.contains(out, n) then
      table.insert(out, n)
    end
  end
  return out
end

local FIELDS = "number title state headRefName headRefOid baseRefName baseRefOid additions deletions mergeCommit { oid }"

-- One GraphQL query for PRs `ns` of `slug`, each aliased p<N>.
function M.query(slug, ns)
  local owner, name = slug:match("^([^/]+)/(.+)$")
  local parts = {}
  for _, n in ipairs(ns) do
    table.insert(parts, ("p%d: pullRequest(number: %d) { %s }"):format(n, n, FIELDS))
  end
  return ('query { repository(owner: "%s", name: "%s") { %s } }'):format(owner, name, table.concat(parts, " "))
end

-- One menu row for PR `p` (a GraphQL node) under `section`. The open PR whose
-- head is `branch` (checked out in the tab's worktree) is live: that worktree
-- against its parent; every other PR is M.view's kitchen-pr-<N> checkout, so
-- its review comments and ⌘M work there.
local function row(tab, p, section, place, branch, dflt)
  local live = p.state == "OPEN" and p.headRefName == branch
  local detail = { p.baseRefName .. " ← " .. p.headRefName, ("+%d −%d"):format(p.additions or 0, p.deletions or 0) }
  if place then
    table.insert(detail, 1, place)
  end
  if live then
    table.insert(detail, "live")
  elseif p.state ~= "OPEN" then
    table.insert(detail, p.state:lower())
  end
  local view = live and { base = "origin/" .. p.baseRefName } or M.view(tab, p.number, p, dflt)
  view.tab = nil
  return { section = section, label = "#" .. p.number .. " " .. (p.title or ""), detail = table.concat(detail, " · "), view = view }
end

-- Menu entries: the stacks among `listed` (GraphQL nodes, in list order),
-- then `reviewing`. A stack is the PRs linked base → head, bottom first,
-- children in list order, under "<bottom base> ← <bottom head>"; stacks come
-- in the order of their first PR in the list. A cycle is cut where the walk
-- meets a PR it placed.
function M.entries(tab, listed, reviewing, branch, dflt)
  local heads, children, order = {}, {}, {}
  for i, p in ipairs(listed) do
    heads[p.headRefName] = true
    children[p.baseRefName] = children[p.baseRefName] or {}
    table.insert(children[p.baseRefName], p)
    order[p.number] = i
  end
  local placed, stacks = {}, {}
  local function walk(root)
    local stack = {}
    local function go(p)
      if placed[p.number] then
        return
      end
      placed[p.number] = true
      table.insert(stack, p)
      for _, c in ipairs(children[p.headRefName] or {}) do
        go(c)
      end
    end
    go(root)
    table.insert(stacks, stack)
  end
  for _, p in ipairs(listed) do
    if not heads[p.baseRefName] then
      walk(p)
    end
  end
  for _, p in ipairs(listed) do
    if not placed[p.number] then
      walk(p) -- a cycle: no PR of it is a root
    end
  end
  local function first(stack)
    local min = math.huge
    for _, p in ipairs(stack) do
      min = math.min(min, order[p.number])
    end
    return min
  end
  table.sort(stacks, function(a, b)
    return first(a) < first(b)
  end)
  local out = {}
  for _, stack in ipairs(stacks) do
    local section = stack[1].baseRefName .. " ← " .. stack[1].headRefName
    for i, p in ipairs(stack) do
      table.insert(out, row(tab, p, section, i .. "/" .. #stack, branch, dflt))
    end
  end
  for _, p in ipairs(reviewing) do
    table.insert(out, row(tab, p, "Reviewing", nil, nil, dflt))
  end
  return out
end

-- ⌘⇧S: the tab's PR list, plus its ⌘⇧P PRs, into kitchen.views. Every path
-- ends in kitchen.views, so the menu stops saying "loading PRs…".
local function menu(ev)
  local function done(entries)
    kitchen.views({ tab = ev.tab, entries = entries })
  end
  local ws
  for _, w in ipairs(kitchen.workspaces()) do
    if w.id == ev.workspace then
      ws = w
    end
  end
  if not (ws and ws.github_slug) then
    return done({})
  end
  local slug = ws.github_slug
  local ns = M.list(ev.prs, slug)
  local rv = {}
  for _, n in ipairs(viewed[ev.tab] or {}) do
    if not M.contains(ns, n) then
      table.insert(rv, n)
    end
  end
  local all = {}
  for _, list in ipairs({ ns, rv }) do
    for _, n in ipairs(list) do
      table.insert(all, n)
    end
  end
  if #all == 0 then
    return done({})
  end
  gh({ "api", "graphql", "-f", "query=" .. M.query(slug, all) }, function(res)
    -- gh exits 1 when one alias fails (a wrong number) but still prints the rest.
    local ok, body = pcall(kitchen.json.decode, res.stdout or "")
    local repo = ok and type(body) == "table" and type(body.data) == "table" and body.data.repository
    if type(repo) ~= "table" then
      kitchen.notify("couldn't read the tab's PRs: " .. why(res), "warn")
      return done({})
    end
    local function nodes(list)
      local out = {}
      for _, n in ipairs(list) do
        local p = repo["p" .. n]
        if type(p) == "table" then
          table.insert(out, p)
        end
      end
      return out
    end
    done(M.entries(ev.tab, nodes(ns), nodes(rv), ev.branch, ws.default_branch or "main"))
  end)
end
kitchen.on("views", menu)

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

-- Threads on `path` as kitchen.comments items: the root as the card, each
-- reply under its own header, and `slug` for the #12 / @al / SHA links.
function M.items(threads, path, slug)
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
      local replies = {}
      for _, r in ipairs(t.replies) do
        table.insert(replies, { title = "@" .. (r.user or "?") .. " · " .. (r.created_at or ""):sub(1, 10), body = r.body or "" })
      end
      table.insert(out, {
        id = tostring(c.id),
        line = (not outdated) and c.line or nil,
        side = c.side == "LEFT" and "old" or "new",
        title = title,
        body = c.body or "",
        replies = replies,
        repo = slug,
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
    local items = pr.threads and M.items(pr.threads, path, pr.slug) or {}
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
  local draft = { line = ev.line, side = ev.side, title = "posting to #" .. pr.n .. "…", body = ev.text, repo = pr.slug, dim = true }
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

-- The PR description (⌘⇧D) ------------------------------------------------

-- Where the viewer was before ⌘⇧D showed a description, by tab: { path, line, dir }.
local before = {}

-- The description file of PR `n` in a worktree (`.kitchen/` is git-excluded).
local function description_path(n)
  return ".kitchen/pr-" .. n .. ".md"
end

-- The description page for PR `info` (gh's JSON): title, a line of facts,
-- then the body as GitHub shows it (its #12, @al and SHAs linked to the
-- PR's repo, which its URL names).
function M.description(info)
  local state = info.isDraft and "draft" or (info.state or ""):lower()
  local facts = {
    "**#" .. info.number .. "**",
    info.author and info.author.login and ("@" .. info.author.login) or nil,
    state ~= "" and state or nil,
    info.baseRefName and info.headRefName and ("`" .. info.baseRefName .. "` ← `" .. info.headRefName .. "`") or nil,
    info.additions and ("+" .. info.additions .. " −" .. (info.deletions or 0)) or nil,
  }
  local line = {}
  for i = 1, 5 do
    if facts[i] then
      table.insert(line, facts[i])
    end
  end
  local body = kitchen.markdown.github(info.body or "", { repo = M.parse(info.url or "") })
  if not body:find("%S") then
    body = "_No description provided._"
  end
  local url = info.url and ("\n\n[" .. info.url .. "](" .. info.url .. ")") or ""
  return "# " .. (info.title or ("PR #" .. info.number)) .. "\n\n" .. table.concat(line, " · ") .. url .. "\n\n---\n\n" .. body .. "\n"
end

-- Write PR `ref`'s description into `worktree` on `host` and show it rendered.
local function show_description(ctx, slug, ref)
  local fields = "number,title,body,author,state,isDraft,baseRefName,headRefName,additions,deletions,url"
  gh({ "pr", "view", tostring(ref), "-R", slug, "--json", fields }, function(res)
    local ok, info = pcall(kitchen.json.decode, res.code == 0 and res.stdout or "")
    if not (ok and type(info) == "table" and info.number) then
      return kitchen.notify("couldn't read the PR's description: " .. why(res), "warn")
    end
    local rel = description_path(info.number)
    local abs = ctx.viewer.worktree .. "/" .. rel
    local sh = "mkdir -p " .. q(ctx.viewer.worktree .. "/.kitchen") .. " && cat > " .. q(abs)
    kitchen.run({ sh = sh, host = ctx.host, stdin = M.description(info), timeout = 15 }, function(w)
      if w.code ~= 0 then
        return kitchen.notify("couldn't write the description: " .. why(w), "error")
      end
      before[ctx.tab] = { path = ctx.viewer.path, line = ctx.viewer.line, dir = ctx.viewer.dir, diff = ctx.viewer.diff }
      -- Plain, not diffed: the file is in no commit, so a diff shows it all as added.
      kitchen.open(abs, nil, { diff = false })
      kitchen.render.show(ctx.tab, true)
    end)
  end)
end

-- ⌘⇧D: the shown PR's description, rendered; again, back to where the
-- viewer was. The PR is the one checked out (kitchen-pr-<N>), else the one
-- for the branch checked out in the tab's worktree.
kitchen.command("description", function(ctx)
  local v = ctx.viewer
  if not v then
    return
  end
  if v.path and v.path:match("^%.kitchen/pr%-%d+%.md$") then
    local b = before[ctx.tab] or {}
    before[ctx.tab] = nil
    kitchen.render.show(ctx.tab, false)
    local opts = { diff = b.diff }
    if b.path then
      kitchen.open(v.worktree .. "/" .. b.path, b.line, opts)
    else
      kitchen.open(v.worktree .. "/" .. ((b.dir or "") ~= "" and b.dir .. "/" or ""), nil, opts)
    end
    return
  end
  local pr = pr_of({ host = ctx.host, worktree = v.worktree })
  if pr then
    return show_description(ctx, pr.slug, pr.n)
  end
  local slug
  for _, w in ipairs(kitchen.workspaces()) do
    if w.id == ctx.workspace then
      slug = w.github_slug
    end
  end
  if not slug then
    return kitchen.notify("this workspace isn't a GitHub repo", "warn")
  end
  kitchen.run({ cmd = { "git", "symbolic-ref", "--short", "-q", "HEAD" }, host = ctx.host, cwd = v.worktree, timeout = 15 }, function(r)
    local branch = r.code == 0 and r.stdout:match("%S+")
    if not branch then
      return kitchen.notify("the viewer isn't showing a PR or a branch", "warn")
    end
    show_description(ctx, slug, branch)
  end)
end, { desc = "show the PR's description rendered, as on GitHub (again: back)" })

kitchen.keymap("cmd+shift+d", "github.description", { when = "viewer" })

return M
