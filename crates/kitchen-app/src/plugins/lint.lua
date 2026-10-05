-- kitchen.lint: declarative linters (like nvim-lint / SublimeLinter).
--
--   require("kitchen.lint").define {
--     name = "ruff",                      -- namespace (default: the plugin's name)
--     filetypes = { "py" },               -- default: the manifest's filetypes
--     cmd = function(ev) return { "ruff", "check", "-" } end,   -- or
--     sh  = function(ev) return "ruff check -" end,             -- shell script
--     stdin = true,                       -- pipe the viewer's text (default true)
--     timeout = 30,                       -- seconds
--     parse = function(res, ev) return { {line=, col=, message=, severity=} } end,
--   }
--
-- Runs on the file's host (the devbox for devbox workspaces) in its
-- worktree, on every file_opened / file_changed. A result for an older
-- version of a file is dropped.
local M = {}

local function key(f)
  return f.host .. "\0" .. f.worktree .. "\0" .. f.path
end

function M.define(spec)
  assert(type(spec.parse) == "function", "kitchen.lint.define: parse(res, ev) is required")
  assert(spec.cmd or spec.sh, "kitchen.lint.define: cmd(ev) or sh(ev) is required")
  local latest = {}
  local wanted = nil
  if spec.filetypes then
    wanted = {}
    for _, ft in ipairs(spec.filetypes) do wanted[ft] = true end
  end
  local ns = spec.name and spec.name ~= kitchen.name and spec.name or nil

  local function lint(ev)
    if wanted and not wanted[ev.filetype] then return end
    local k = key(ev.file)
    latest[k] = ev.version
    local job = { host = ev.file.host, cwd = ev.file.worktree, timeout = spec.timeout }
    if spec.stdin ~= false then job.stdin = ev.text() or "" end
    if spec.sh then job.sh = spec.sh(ev) else job.cmd = spec.cmd(ev) end
    kitchen.run(job, function(res)
      if latest[k] ~= ev.version then return end -- superseded by a newer version
      if res.timed_out then
        kitchen.log((spec.name or kitchen.name) .. ": timed out on " .. ev.file.path)
        return
      end
      local ok, items = pcall(spec.parse, res, ev)
      if not ok then
        kitchen.log("parse failed for " .. ev.file.path .. ": " .. tostring(items) .. "\nstdout: " .. res.stdout .. "\nstderr: " .. res.stderr)
        return
      end
      if items == nil then return end
      kitchen.diagnostics.set(ev.file, items, { version = ev.version, ns = ns })
    end)
  end

  kitchen.on("file_opened", lint)
  kitchen.on("file_changed", lint)
end

return M
