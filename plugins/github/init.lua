-- github: review a PR in the tab you're in. Paste its link into ⌘⇧P: the PR
-- head is checked out detached in <repo>-kitchen-pr-<N> (no tab, no agent)
-- and the tab's viewer shows its diff against the PR's base, with the PR's
-- review comments under their lines; ⌘M … ⌘Enter posts a comment (or a
-- reply, on a line that has one) to the PR. Each PR viewed gets a ⌘⇧P entry
-- (github.pr_<N>) to show it again; github.own_changes shows the tab's own
-- worktree again. ⌘⇧S lists the PRs in the tab's $KITCHEN_PRS_FILE, one
-- section per stack, then the PRs opened in the tab with ⌘⇧P; an open PR's
-- row has a check once its code owners approved it. Atop the viewer's file
-- list, the PR's reviewers and their state (a team's members, whom to ping, on
-- hover), the required checks that block its merge, and a row that shows
-- the PR's description, rendered as on GitHub (⌘⇧K from the first file).
-- ⌘⇧O opens the shown PR (in the ⌘⇧S menu, the row under the cursor) in
-- Chrome, keeping the kitchen in front.

local M = {}

-- Re-read a PR's comments when a file opens and they're older than this.
local REFRESH = 60
-- One compact JSON object per comment (nulls dropped).
local JQ = ".[] | {id, in_reply_to_id, path, line, original_line, start_line, side, subject_type,"
  .. " user: .user.login, body, created_at} | with_entries(select(.value != null)) | @json"

-- The PRs whose worktrees the viewer has shown, by "<host>:<worktree path>"
-- (a repo can be a workspace both here and on a devbox). A tab's own
-- worktree is in it while ⌘⇧S last found it live, with the PR's `branch`.
local prs = {} -- key → { slug, n, branch, threads, at, files = {path → file}, drafts = {path → items} }

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

local q = require("kitchen.util").q

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
-- on the same host (rebuilt from the name, so it survives a plugin reload),
-- or a tab's worktree ⌘⇧S found live (forgotten on a reload until ⌘⇧S).
function M.pr_of(f)
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
-- head detached in <repo>-kitchen-pr-<n>, diffed against its base. An open
-- PR's base is its base branch (`dflt` if gh names none). A merged or closed
-- PR's is the commit GitHub froze as its base (`baseRefOid`), so it shows the
-- diff it had on GitHub: its parent in a stack may since have been squashed
-- into main, and its base branch deleted. With the commits GitHub reports
-- (`headRefOid`, `baseRefOid`) the fetch is skipped when they are already here.
function M.view(tab, n, info, dflt)
  local rev = "refs/kitchen/pr/" .. n
  local spec = { tab = tab, rev = rev, name = "kitchen-pr-" .. n, fetch = { "+refs/pull/" .. n .. "/head:" .. rev } }
  local base_have
  if info.state ~= "OPEN" and info.baseRefOid then
    table.insert(spec.fetch, info.baseRefOid)
    spec.base = info.baseRefOid
    base_have = { rev = info.baseRefOid, commit = info.baseRefOid }
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
  gh({ "pr", "view", tostring(n), "-R", slug, "--json", "title,baseRefName,baseRefOid,headRefOid,state" }, function(res)
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

local FIELDS = "number title body url state isDraft headRefName headRefOid baseRefName baseRefOid additions deletions reviewDecision"
  .. " author { login }"
  .. " reviewRequests(first: 20) { nodes { asCodeOwner requestedReviewer {"
  .. " ... on User { login name } ... on Team { slug members(first: 100) { nodes { login name } } } } } }"
  .. " latestOpinionatedReviews(first: 20) { nodes { state author { login } } }"

local function named(s)
  return s ~= "" and s or nil
end

-- The commit status (on a PR's last commit) that says whether its code
-- owners approved it; unset or "": GitHub's review decision says.
local OWNERS = named(kitchen.config.owners_status)

local function gql_string(s)
  return '"' .. (s:gsub('[\\"]', "\\%0")) .. '"'
end

-- Every check on the last commit, and whether branch protection requires it
-- for PR %d (M.gates).
local ROLLUP = "statusCheckRollup { contexts(first: 100) { nodes {"
  .. " ... on CheckRun { name status conclusion isRequired(pullRequestNumber: %d) }"
  .. " ... on StatusContext { context state description isRequired(pullRequestNumber: %d) } } } }"

-- One GraphQL query for PRs `ns` of `slug`, each aliased p<N>.
function M.query(slug, ns)
  local owner, name = slug:match("^([^/]+)/(.+)$")
  local status = OWNERS and "status { owners: context(name: " .. gql_string(OWNERS) .. ") { state } } " or ""
  local parts = {}
  for _, n in ipairs(ns) do
    local commit = status .. ROLLUP:format(n, n)
    table.insert(parts, ("p%d: pullRequest(number: %d) { %s commits(last: 1) { nodes { commit { %s } } } }"):format(n, n, FIELDS, commit))
  end
  return ('query { repository(owner: "%s", name: "%s") { %s } }'):format(owner, name, table.concat(parts, " "))
end

-- A check run's or commit status's state as a check's: passed (or skipped)
-- done, running waiting, failed needed.
local function gate_state(c)
  if c.status then -- a check run
    if c.status ~= "COMPLETED" then
      return "waiting"
    end
    return ({ SUCCESS = true, NEUTRAL = true, SKIPPED = true })[c.conclusion or ""] and "done" or "needed"
  end
  return c.state == "SUCCESS" and "done" or (c.state == "PENDING" or c.state == "EXPECTED") and "waiting" or "needed"
end

-- The checks that keep open PR `p` (a GraphQL node) from merging, for the
-- viewer's file list: the required ones that failed, then those still
-- running, each { state, label = its name, note = its description }. Once
-- they all passed, one done row says so. A repo that requires no check
-- counts every check as required.
function M.gates(p)
  local last = p.commits and p.commits.nodes and p.commits.nodes[1]
  local rollup = last and last.commit and last.commit.statusCheckRollup
  local all = rollup and rollup.contexts and rollup.contexts.nodes or {}
  local required = {}
  for _, c in ipairs(all) do
    if c.isRequired then
      table.insert(required, c)
    end
  end
  local kind = "required "
  if #required == 0 then
    required, kind = all, ""
  end
  local out = {}
  for _, state in ipairs({ "needed", "waiting" }) do
    for _, c in ipairs(required) do
      if gate_state(c) == state then
        table.insert(out, { state = state, label = c.name or c.context or "?", note = c.description or "" })
      end
    end
  end
  if #out == 0 and #required > 0 then
    local n = #required
    out[1] = { state = "done", label = (n == 1 and "the " .. kind .. "check" or "all " .. n .. " " .. kind .. "checks") .. " passed", note = "" }
  end
  return out
end

-- How a reviewer is shown: "Paul Fryzel" (their GitHub name), else "@paulfryzel".
local function person(u)
  return (u.name or "") ~= "" and u.name or "@" .. u.login
end

-- The reviewers of PR `p` (a GraphQL node), as GitHub's Reviewers box lists
-- them, for the viewer's file list: { state, name, status, members }. The
-- teams and people asked to review wait (a team with its members, the
-- author left out, so you know whom to ping), then who approved (done) and
-- who asked for changes (needed).
function M.reviewers(p)
  local author = p.author and p.author.login
  local out = {}
  for _, r in ipairs(p.reviewRequests and p.reviewRequests.nodes or {}) do
    local rv = r.requestedReviewer or {}
    local status = "requested" .. (r.asCodeOwner and " · code owner" or "")
    if rv.slug then
      local members = {}
      for _, u in ipairs(rv.members and rv.members.nodes or {}) do
        if u.login ~= author then
          table.insert(members, person(u))
        end
      end
      table.insert(out, { state = "waiting", name = rv.slug, team = true, status = status, members = members })
    elseif rv.login then
      table.insert(out, { state = "waiting", name = "@" .. rv.login, status = status, members = {} })
    end
  end
  for _, rv in ipairs(p.latestOpinionatedReviews and p.latestOpinionatedReviews.nodes or {}) do
    local state = ({ APPROVED = "done", CHANGES_REQUESTED = "needed" })[rv.state]
    if state and rv.author and rv.author.login then
      local status = state == "done" and "approved" or "changes requested"
      table.insert(out, { state = state, name = "@" .. rv.author.login, status = status, members = {} })
    end
  end
  return out
end

-- Whether open PR `p` (a GraphQL node) has its code owners' approval: the
-- owners status passed, else GitHub's review decision is APPROVED.
function M.approved(p)
  if p.state ~= "OPEN" then
    return false
  end
  local last = p.commits and p.commits.nodes and p.commits.nodes[1]
  local owners = last and last.commit and last.commit.status and last.commit.status.owners
  if owners then
    return owners.state == "SUCCESS"
  end
  return p.reviewDecision == "APPROVED"
end

-- One menu row for PR `p` (a GraphQL node) under `section`, with its state
-- (open, draft, merged, closed: the menu draws GitHub's icon for it), its
-- page (for ⌘⇧O), M.approved, M.reviewers, M.gates and M.description. The open PR whose head is `branch` (checked out in the tab's worktree) is live:
-- that worktree against its parent (remember_live maps it to the PR, for its
-- review comments and ⌘M); every other PR is M.view's kitchen-pr-<N> checkout.
function M.is_live(p, branch)
  return p.state == "OPEN" and p.headRefName == branch
end

local function row(tab, p, section, place, branch, dflt)
  local live = M.is_live(p, branch)
  local detail = { p.baseRefName .. " ← " .. p.headRefName, ("+%d −%d"):format(p.additions or 0, p.deletions or 0) }
  if place then
    table.insert(detail, 1, place)
  end
  if live then
    table.insert(detail, "live")
  end
  local state = p.state == "OPEN" and (p.isDraft and "draft" or "open") or (p.state or ""):lower()
  local view = live and { base = "origin/" .. p.baseRefName } or M.view(tab, p.number, p, dflt)
  view.tab = nil
  return {
    section = section,
    label = "#" .. p.number .. " " .. (p.title or ""),
    detail = table.concat(detail, " · "),
    state = state ~= "" and state or nil,
    url = p.url,
    approved = M.approved(p),
    reviewers = p.state == "OPEN" and M.reviewers(p) or {},
    gates = p.state == "OPEN" and M.gates(p) or {},
    description = M.description(p),
    view = view,
  }
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

-- Map the tab's worktree (`ev`, the views event) to its live PR among
-- `listed`, so ⌘M there posts to it and its comments show; drop a mapping
-- the worktree has left.
function M.remember_live(ev, slug, listed)
  if not (ev.host and ev.worktree) then
    return
  end
  local key = ev.host .. ":" .. ev.worktree
  local n
  for _, p in ipairs(listed) do
    if M.is_live(p, ev.branch) then
      n = p.number
      break
    end
  end
  local old = prs[key]
  if old and old.n == n and old.slug == slug then
    return
  end
  for _, file in pairs(old and old.files or {}) do
    kitchen.comments.clear(file)
  end
  prs[key] = n and { slug = slug, n = n, branch = ev.branch, files = {}, drafts = {} } or nil
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
    local listed = nodes(ns)
    M.remember_live(ev, slug, listed)
    done(M.entries(ev.tab, listed, nodes(rv), ev.branch, ws.default_branch or "main"))
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
  local pr = M.pr_of(ev.file)
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
  local pr = M.pr_of(ev.file)
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
  kitchen.run({ cmd = { "git", "rev-parse", "HEAD", "--abbrev-ref", "HEAD" }, host = ev.file.host, cwd = ev.file.worktree, timeout = 15 }, function(head)
    local sha, branch = (head.code == 0 and head.stdout or ""):match("^(%x+)%s+(%S+)")
    if not sha or not ev.line then
      done({ code = 1, stderr = "can't tell the tab's commit" })
      return
    end
    -- A tab's own worktree may have switched branch since ⌘⇧S saw it live.
    if pr.branch and branch ~= pr.branch then
      done({ code = 1, stderr = "the worktree is on " .. branch .. " now, not #" .. pr.n .. "'s " .. pr.branch .. "; reopen ⌘⇧S" })
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
  local pr = ctx.viewer and M.pr_of({ host = ctx.host, worktree = ctx.viewer.worktree })
  if pr then
    fetch(pr)
  else
    kitchen.notify("the viewer isn't showing a PR", "warn")
  end
end, { desc = "re-read the shown PR's review comments from GitHub" })

kitchen.command("own_changes", function(ctx)
  kitchen.view({ tab = ctx.tab })
end, { desc = "show this tab's own worktree in its viewer again" })

-- The PR description ------------------------------------------------------

-- The description page for PR `info` (a GraphQL node): title, a line of
-- facts, then the body. The file list's Description row shows it, rendered
-- as GitHub does in the workspace's repo (its #12, @al and SHAs linked).
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
  local body = (info.body or ""):gsub("\r\n", "\n")
  if not body:find("%S") then
    body = "_No description provided._"
  end
  local url = info.url and ("\n\n[" .. info.url .. "](" .. info.url .. ")") or ""
  return "# " .. (info.title or ("PR #" .. info.number)) .. "\n\n" .. table.concat(line, " · ") .. url .. "\n\n---\n\n" .. body .. "\n"
end

-- Opening a PR on GitHub (⌘⇧O) ---------------------------------------------

-- The PR the viewer shows: cb(slug, ref), `ref` its number when the viewer
-- shows its kitchen-pr-<N> checkout, else the branch checked out in the
-- viewer's worktree (gh finds its PR).
local function shown_pr(ctx, cb)
  local pr = M.pr_of({ host = ctx.host, worktree = ctx.viewer.worktree })
  if pr then
    return cb(pr.slug, pr.n)
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
  kitchen.run({ cmd = { "git", "symbolic-ref", "--short", "-q", "HEAD" }, host = ctx.host, cwd = ctx.viewer.worktree, timeout = 15 }, function(r)
    local branch = r.code == 0 and r.stdout:match("%S+")
    if not branch then
      return kitchen.notify("the viewer isn't showing a PR or a branch", "warn")
    end
    cb(slug, branch)
  end)
end

-- Opens argv[1] in a new tab of Chrome's front window and keeps the app that
-- was in front there. Chrome brings itself to the front whenever it loads a
-- URL, scripted or not, and can do so twice as the tab opens, so after the
-- first hand-back it keeps watching for another second.
local NEW_TAB = [[
on run argv
  tell application "System Events" to set frontApp to first process whose frontmost is true
  tell application "Google Chrome"
    if (count of windows) = 0 then make new window
    make new tab at end of tabs of window 1 with properties {URL:item 1 of argv}
  end tell
  tell application "System Events"
    set lost to 0
    repeat with i from 1 to 60
      if not (frontmost of frontApp) then
        set frontmost of frontApp to true
        if lost is 0 then set lost to i
      end if
      if lost > 0 and i - lost >= 20 then exit repeat
      delay 0.05
    end repeat
  end tell
end run]]

-- Open `url` in Chrome on this mac (devbox tabs too), keeping the kitchen in
-- front; else in the default browser.
local function browse(url)
  local sh = "osascript -e " .. q(NEW_TAB) .. " " .. q(url) .. " >/dev/null 2>&1 || open " .. q(url)
  kitchen.run({ sh = sh, host = "local", timeout = 15 }, function(res)
    if res.code ~= 0 then
      kitchen.notify("couldn't open " .. url .. ": " .. why(res), "error")
    end
  end)
end

-- ⌘⇧O: the PR on GitHub, in Chrome. In the ⌘⇧S menu, the row under the
-- cursor; else the PR the viewer shows.
kitchen.command("open_in_browser", function(ctx)
  local v = ctx.viewer
  if not v then
    return
  end
  if v.menu then
    if not v.menu.url then
      return kitchen.notify("that row isn't a PR", "warn")
    end
    return browse(v.menu.url)
  end
  shown_pr(ctx, function(slug, ref)
    if type(ref) == "number" then
      return browse("https://github.com/" .. slug .. "/pull/" .. ref)
    end
    gh({ "pr", "view", ref, "-R", slug, "--json", "url", "--jq", ".url" }, function(res)
      local url = res.code == 0 and res.stdout:match("%S+")
      if not url then
        return kitchen.notify("no PR for " .. ref .. ": " .. why(res), "warn")
      end
      browse(url)
    end)
  end)
end, { desc = "open the shown PR on GitHub in Chrome (in the ⌘⇧S menu: the row under the cursor)" })

kitchen.keymap("cmd+shift+o", "github.open_in_browser", { when = "viewer" })

return M
