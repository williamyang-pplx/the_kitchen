-- ruff: lint Python files in the viewer with ruff.
--
-- Runs on the file's host, in its worktree (so pyproject.toml / ruff.toml
-- apply), on the text the viewer shows. Finds ruff in the project venv,
-- then on PATH, then via uvx.
local lint = require("kitchen.lint")
local cfg = kitchen.config

local function q(s)
  return "'" .. s:gsub("'", "'\\''") .. "'"
end

-- Last diagnostics per file path, for `\e` (explain the rule under the cursor).
local last = {}

lint.define {
  timeout = 40,
  sh = function(ev)
    local args = "check --output-format=json --no-fix --exit-zero --stdin-filename " .. q(ev.file.path)
    if cfg.select and cfg.select ~= "" then
      args = args .. " --extend-select " .. q(cfg.select)
    end
    return table.concat({
      'export PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"',
      'if [ -x .venv/bin/ruff ]; then R=.venv/bin/ruff',
      'elif command -v ruff >/dev/null 2>&1; then R=ruff',
      'elif command -v uvx >/dev/null 2>&1; then R="uvx ruff"',
      'else echo "ruff not found on $(hostname)" >&2; exit 127; fi',
      '$R ' .. args .. ' -',
    }, "\n")
  end,
  parse = function(res, ev)
    if res.code == 127 then
      kitchen.notify("ruff is not installed on " .. ev.file.host .. " (pip/uv install ruff, or add it to the venv)", "warn")
      return nil
    end
    if res.stdout == "" then
      error("no output (exit " .. res.code .. "): " .. res.stderr)
    end
    local items = kitchen.json.decode(res.stdout)
    local out = {}
    for _, d in ipairs(items) do
      local code = d.code
      local severity = "warning"
      if code == nil or code:match("^E9") or code:match("^F82") or code:match("^F63") then
        severity = "error" -- syntax errors, undefined names, invalid comparisons
      end
      table.insert(out, {
        line = d.location.row,
        col = d.location.column,
        end_line = d.end_location and d.end_location.row,
        end_col = d.end_location and d.end_location.column,
        severity = severity,
        message = d.message,
        code = code,
      })
    end
    last[ev.file.path] = { file = ev.file, items = out }
    return out
  end,
}

kitchen.command("explain", function(ctx)
  local v = ctx.viewer or {}
  local entry = v.path and last[v.path]
  if not entry then
    kitchen.notify("ruff: no diagnostics for this file")
    return
  end
  for _, d in ipairs(entry.items) do
    if d.line == v.line and d.code then
      kitchen.run({ host = entry.file.host, cwd = entry.file.worktree, sh = 'export PATH="$HOME/.local/bin:$PATH"; ruff rule ' .. q(d.code) .. ' 2>/dev/null || uvx ruff rule ' .. q(d.code) }, function(res)
        local summary = res.stdout:match("^[^\n]*\n[^\n]*\n([^\n]+)") or res.stdout:match("^([^\n]+)") or d.message
        kitchen.notify(d.code .. ": " .. summary)
        kitchen.log(res.stdout)
      end)
      return
    end
  end
  kitchen.notify("ruff: nothing on this line")
end, { desc = "explain the ruff rule on the cursor line" })

kitchen.keymap("viewer", "<leader>e", "ruff.explain")
