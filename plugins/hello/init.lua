-- hello: the smallest useful plugin. Copy this folder to
-- ~/.kitchen/plugins/<name>/ and edit; it reloads on save.

kitchen.command("where", function(ctx)
  local v = ctx.viewer or {}
  local here = v.path and (v.path .. ":" .. tostring(v.line)) or "(no file open)"
  -- Jobs run on the tab's host: the devbox for devbox workspaces.
  kitchen.run({ host = ctx.host, cwd = ctx.worktree, sh = "hostname; git rev-parse --abbrev-ref HEAD" }, function(res)
    local host, branch = res.stdout:match("([^\n]*)\n([^\n]*)")
    kitchen.notify(("%s on %s (%s)"):format(here, host or "?", branch or "?"))
  end)
end, { desc = "show file, host and branch" })

kitchen.keymap("cmd+shift+y", "hello.where")      -- anywhere
kitchen.keymap("viewer", "<leader>w", "hello.where") -- viewer normal mode: <leader>w
