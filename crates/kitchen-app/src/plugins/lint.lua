-- kitchen.lint: declarative linters (like nvim-lint / SublimeLinter).
--
--   require("kitchen.lint").define {
--     sh  = function(ev) return "ruff check -" end,   -- shell script; gets the viewer's text on stdin
--     timeout = 30,                       -- seconds
--     parse = function(res, ev) return { {line=, col=, message=, severity=} } end,
--   }
--
-- Runs on the file's host (the devbox for devbox workspaces) in its
-- worktree, on every file_opened / file_changed of the manifest's
-- filetypes. A result for an older version of a file is dropped.
local M = {}

local function key(f)
  return f.host .. "\0" .. f.worktree .. "\0" .. f.path
end

function M.define(spec)
  assert(type(spec.parse) == "function", "kitchen.lint.define: parse(res, ev) is required")
  assert(spec.sh, "kitchen.lint.define: sh(ev) is required")
  local latest = {}

  local function lint(ev)
    local k = key(ev.file)
    latest[k] = ev.version
    local job = { host = ev.file.host, cwd = ev.file.worktree, timeout = spec.timeout, stdin = ev.text() or "", sh = spec.sh(ev) }
    kitchen.run(job, function(res)
      if latest[k] ~= ev.version then return end -- superseded by a newer version
      if res.timed_out then
        kitchen.log(kitchen.name .. ": timed out on " .. ev.file.path)
        return
      end
      local ok, items = pcall(spec.parse, res, ev)
      if not ok then
        kitchen.log("parse failed for " .. ev.file.path .. ": " .. tostring(items) .. "\nstdout: " .. res.stdout .. "\nstderr: " .. res.stderr)
        return
      end
      if items == nil then return end
      kitchen.diagnostics.set(ev.file, items, { version = ev.version })
    end)
  end

  kitchen.on("file_opened", lint)
  kitchen.on("file_changed", lint)
end

return M
