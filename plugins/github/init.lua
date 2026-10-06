-- github: open a PR from its link in a tab on the PR's branch, and review it.
--
-- A PR is its repo + number. The tab checks out the PR's branch in its own
-- worktree, <repo>-kitchen-pr-<N>, and the viewer diffs it against the PR's
-- base branch, like GitHub's "Files changed". Opening the link again (or
-- `git pull`) brings in new pushes. gh (on the mac) supplies the branch,
-- title and base; when the branch can't be used (gh failed, a fork, a merged
-- PR) the tab checks out the PR head (origin's refs/pull/N/head) detached,
-- diffed against the workspace's default branch if gh failed.
--
-- Any tab whose branch has an open PR is that PR's tab too: its viewer shows
-- the PR's comment threads under their lines, and ⌘M … ⌘Enter posts a
-- comment (or a reply, on a line that has one) straight to the PR. \p in the
-- viewer (github.toggle_pr_diff) switches the tab's diff between the default
-- branch and the PR's base, so one tab can review a stack of PRs: check out
-- each branch in turn.

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

-- The PR head and the default branch, fetched while gh answers.
function M.prefetch(ws, n)
  local dflt = ws.default_branch or "main"
  return { "+refs/pull/" .. n .. "/head:refs/kitchen/pr/" .. n, "+refs/heads/" .. dflt .. ":refs/remotes/origin/" .. dflt }
end

-- The tab for PR `n` of `ws`, given gh's answer (`nil` when gh failed).
-- `prefetched`: M.prefetch already ran, so the PR head and the default
-- branch need no fetch.
function M.spec(ws, n, info, prefetched)
  local dflt = ws.default_branch or "main"
  local base = (info and info.baseRefName) or dflt
  local spec = { workspace = ws.id, name = "kitchen-pr-" .. n, title = "PR #" .. n }
  if info and info.title and info.title ~= "" then
    spec.title = "#" .. n .. " " .. info.title
  end
  local fetch = {}
  if info and info.state == "OPEN" and info.headRefName and not info.isCrossRepository then
    spec.branch = info.headRefName
    table.insert(fetch, "+refs/heads/" .. info.headRefName .. ":refs/remotes/origin/" .. info.headRefName)
  else
    spec.rev = "refs/kitchen/pr/" .. n
    if not prefetched then
      table.insert(fetch, "+refs/pull/" .. n .. "/head:" .. spec.rev)
    end
  end
  if info and info.state == "MERGED" and info.mergeCommit and info.mergeCommit.oid then
    -- The base branch has moved past the PR (and may be gone): diff against
    -- the base as it was just before the merge.
    table.insert(fetch, info.mergeCommit.oid)
    spec.diff_base = info.mergeCommit.oid .. "^1"
  else
    if not (prefetched and base == dflt) then
      table.insert(fetch, "+refs/heads/" .. base .. ":refs/remotes/origin/" .. base)
    end
    spec.diff_base = "origin/" .. base
  end
  -- (nil, not {}: an empty table would reach tab_open as an object)
  spec.fetch = #fetch > 0 and fetch or nil
  return spec
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
  -- Ask gh (on the mac) and fetch the PR on the workspace's host at once;
  -- the tab opens when both are done.
  local pending, info, prefetched = 2, nil, false
  local function done()
    pending = pending - 1
    if pending == 0 then
      kitchen.tab_open(M.spec(ws, n, info, prefetched))
    end
  end
  local sh = "export PATH=" .. (kitchen.config.path or "") .. ':"$PATH"; gh pr view ' .. n .. " -R " .. q(slug)
    .. " --json title,baseRefName,state,mergeCommit,headRefName,isCrossRepository"
  kitchen.run({ sh = sh, host = "local", timeout = 20 }, function(res)
    if res.code == 0 then
      local ok, v = pcall(kitchen.json.decode, res.stdout)
      info = ok and v or nil
    end
    if not info then
      local why = (res.stderr ~= "" and res.stderr or res.stdout):gsub("%s+$", "")
      kitchen.notify("gh pr view failed (" .. why .. ") — diffing against " .. (ws.default_branch or "main"), "warn")
    end
    done()
  end)
  local fetch = { "git", "fetch", "--quiet", "--no-tags", "--no-write-fetch-head", "origin" }
  for _, r in ipairs(M.prefetch(ws, n)) do
    table.insert(fetch, r)
  end
  kitchen.run({ cmd = fetch, host = ws.host, cwd = ws.repo_path, timeout = 60 }, function(res)
    prefetched = res.code == 0
    done()
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

-- The PR of a tab opened from a PR link in its own worktree,
-- <repo>-kitchen-pr-<N> (tab slugs never start with "kitchen"). It may be
-- detached, so its branch doesn't say.
function M.pr_number(worktree)
  local n = worktree:match("%-kitchen%-pr%-(%d+)$")
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

local prs = {} -- "slug#n" → { slug, n, base, threads, at, files = {abs → file}, drafts = {path → items} }
-- Re-ask git for a tab's branch when a file opens and it's older than this.
local BRANCH_TTL = 10
local trees = {} -- worktree → { branch, at, key = the PR whose comments its files show }
local branch_prs = {} -- "slug:branch" → { n (false: no open PR), base, at }

local function tab_of(worktree)
  for _, w in ipairs(kitchen.workspaces()) do
    for _, t in ipairs(w.tabs or {}) do
      if t.worktree == worktree then
        return w, t
      end
    end
  end
end

-- `worktree`'s files show PR `key`'s comments (nil: none); drop the ones
-- they showed for another PR before its tab switched branches.
local function show(worktree, key)
  local tree = trees[worktree] or { at = 0 }
  trees[worktree] = tree
  local old = tree.key and tree.key ~= key and prs[tree.key]
  for abs, file in pairs(old and old.files or {}) do
    if file.worktree == worktree then
      old.files[abs] = nil
      kitchen.comments.clear(file)
    end
  end
  tree.key = key
end

-- cb(pr, ws, tab), or cb(nil, why): the PR `worktree`'s tab is on. A tab
-- opened from a PR link in its own worktree is on that PR; any other tab is
-- on its branch's open PR. git and gh answers are reused for a while unless
-- `fresh.branch` / `fresh.pr`.
local function pr_for(worktree, fresh, cb)
  local ws, tab = tab_of(worktree)
  if not (ws and ws.github_slug) then
    return cb(nil, "this tab's workspace isn't a GitHub repo")
  end
  local function found(n, base)
    local key = ws.github_slug .. "#" .. n
    show(worktree, key)
    prs[key] = prs[key] or { slug = ws.github_slug, n = n, files = {}, drafts = {} }
    prs[key].base = base or prs[key].base
    cb(prs[key], ws, tab)
  end
  local function none(why_not)
    show(worktree, nil)
    cb(nil, why_not)
  end
  -- `sel`: a PR number or a branch; `cache`: where a branch's answer goes.
  local function ask(sel, cache)
    gh({ "pr", "view", tostring(sel), "-R", ws.github_slug, "--json", "number,baseRefName,state" }, function(res)
      local ok, info = pcall(kitchen.json.decode, res.code == 0 and res.stdout or "")
      if not (ok and type(info) == "table") then
        if not why(res):find("no pull requests found") then
          return cb(nil, "gh pr view failed: " .. why(res))
        end
        info = {}
      end
      local open = info.number and (info.state == "OPEN" or not cache)
      if cache then
        branch_prs[cache] = { n = open and info.number, base = info.baseRefName, at = os.time() }
      end
      if open then
        found(info.number, info.baseRefName)
      else
        none("no open PR for branch " .. sel)
      end
    end)
  end
  local n = M.pr_number(worktree)
  if n then
    if fresh.pr then
      return ask(n)
    end
    return found(n)
  end
  local function on_branch(branch)
    if not branch then
      return none("the tab isn't on a branch")
    end
    local key = ws.github_slug .. ":" .. branch
    local known = branch_prs[key]
    if fresh.pr or not known or (not known.n and os.time() - known.at >= REFRESH) then
      return ask(branch, key)
    end
    if known.n then
      found(known.n, known.base)
    else
      none("no open PR for branch " .. branch)
    end
  end
  local tree = trees[worktree]
  if tree and not fresh.branch and os.time() - tree.at < BRANCH_TTL then
    return on_branch(tree.branch)
  end
  kitchen.run({ cmd = { "git", "symbolic-ref", "--quiet", "--short", "HEAD" }, host = ws.host, cwd = worktree, timeout = 15 }, function(res)
    -- (exit 1: detached)
    if res.code ~= 0 and res.code ~= 1 then
      return cb(nil, "can't tell the tab's branch: " .. why(res))
    end
    tree = trees[worktree] or {}
    trees[worktree] = tree
    tree.branch, tree.at = res.code == 0 and res.stdout:match("[^\n]+") or nil, os.time()
    on_branch(tree.branch)
  end)
end

local function render(pr)
  for _, file in pairs(pr.files) do
    local items = pr.threads and M.items(pr.threads, file.path) or {}
    for _, d in ipairs(pr.drafts[file.path] or {}) do
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
  pr_for(ev.file.worktree, {}, function(pr)
    if not pr then
      return
    end
    pr.files[ev.file.abs] = ev.file
    render(pr)
    if not pr.threads or os.time() - pr.at >= REFRESH then
      fetch(pr)
    end
  end)
end)

kitchen.on("comment", function(ev)
  -- The branch now, not as of the last file opened: the tab may have moved.
  pr_for(ev.file.worktree, { branch = true }, function(pr, why_not)
    if not pr then
      kitchen.notify("comment not posted: " .. why_not .. " (comments go to the PR of the tab's branch)", "warn")
      return
    end
    pr.files[ev.file.abs] = ev.file
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
end)

kitchen.command("refresh_comments", function(ctx)
  pr_for(ctx.worktree or "", { branch = true, pr = true }, function(pr, why_not)
    if pr then
      fetch(pr)
    else
      kitchen.notify(why_not, "warn")
    end
  end)
end, { desc = "re-read the review comments of this tab's PR from GitHub" })

-- A tab diffs against origin/<default> (all of its branch, stacked PRs
-- below it included) until this switches it to its PR's base branch (just
-- the PR, as on GitHub), and back.
kitchen.command("toggle_pr_diff", function(ctx)
  local ws, tab = tab_of(ctx.worktree or "")
  if not tab then
    return kitchen.notify("not in a tab", "warn")
  end
  local dflt = ws.default_branch or "main"
  if tab.diff_base and tab.diff_base ~= "origin/" .. dflt then
    kitchen.set_diff_base(tab.id, nil)
    return kitchen.notify("diffing against origin/" .. dflt)
  end
  pr_for(tab.worktree, { branch = true, pr = true }, function(pr, why_not)
    if not pr then
      return kitchen.notify(why_not, "warn")
    end
    if pr.base == dflt then
      return kitchen.notify("#" .. pr.n .. " is against " .. dflt .. ": the tab already shows just its diff")
    end
    local ref = "refs/remotes/origin/" .. pr.base
    local fetch_base = { "git", "fetch", "--quiet", "--no-tags", "--no-write-fetch-head", "origin", "+refs/heads/" .. pr.base .. ":" .. ref }
    kitchen.run({ cmd = fetch_base, host = ws.host, cwd = tab.worktree, timeout = 60 }, function(res)
      if res.code ~= 0 then
        return kitchen.notify("couldn't fetch " .. pr.base .. ": " .. why(res), "error")
      end
      kitchen.set_diff_base(tab.id, "origin/" .. pr.base)
      kitchen.notify("diffing against #" .. pr.n .. "'s base, " .. pr.base)
    end)
  end)
end, { desc = "diff this tab against its PR's base branch, or back against the default branch" })
kitchen.keymap("viewer", "<leader>p", "github.toggle_pr_diff")

return M
