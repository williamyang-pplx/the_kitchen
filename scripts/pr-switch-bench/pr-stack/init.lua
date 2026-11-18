-- The PR-switch bench's stand-in for the github plugin: the tab's PR menu is
-- the stack in $PR_STACK_FILE ([{n, branch, base, head, base_oid}], from
-- make-repo.py), served after PR_STACK_DELAY seconds (GitHub's GraphQL
-- round trip), each row as the github plugin builds it: the PR's head
-- fetched into refs/kitchen/pr/<n>, diffed against its base branch, with the
-- commits GitHub reports as `have`.
local function menu(ev)
  local file, delay = os.getenv("PR_STACK_FILE"), os.getenv("PR_STACK_DELAY") or "0.6"
  kitchen.run({ sh = "sleep " .. delay .. "; cat " .. file }, function(res)
    local ok, stack = pcall(kitchen.json.decode, res.stdout or "")
    if not ok or type(stack) ~= "table" then
      kitchen.views({ tab = ev.tab, entries = {} })
      return
    end
    local entries = {}
    for i, p in ipairs(stack) do
      local rev = "refs/kitchen/pr/" .. p.n
      local base = "refs/remotes/origin/" .. p.base
      table.insert(entries, {
        section = "main ← stack",
        label = "#" .. p.n .. " stack part " .. i,
        detail = i .. "/" .. #stack .. " · " .. p.base .. " ← " .. p.branch,
        state = "open",
        view = {
          rev = rev,
          name = "kitchen-pr-" .. p.n,
          fetch = { "+refs/pull/" .. p.n .. "/head:" .. rev, "+refs/heads/" .. p.base .. ":" .. base },
          base = "origin/" .. p.base,
          have = { { rev = rev, commit = p.head }, { rev = base, commit = p.base_oid } },
        },
      })
    end
    kitchen.views({ tab = ev.tab, entries = entries })
  end)
end
kitchen.on("views", menu)
