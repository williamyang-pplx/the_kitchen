-- todo: highlight marker words and list them with `\t`.
local words = kitchen.config.words or { "TODO", "FIXME" }
local found = {} -- path -> { {line, text} }

local function scan(ev)
  local text = ev.text()
  if not text then return end
  local hl, hits, n = {}, {}, 0
  for line in (text .. "\n"):gmatch("(.-)\n") do
    n = n + 1
    for _, w in ipairs(words) do
      local s = 1
      while true do
        local a, b = line:find(w, s, true)
        if not a then break end
        -- Columns are characters; these words are ASCII, but the prefix may not be.
        local col = utf8.len(line:sub(1, a - 1)) + 1
        table.insert(hl, { line = n, col = col, end_col = col + (b - a + 1), group = "#ff9e64" })
        table.insert(hits, { line = n, text = line:gsub("^%s+", "") })
        s = b + 1
      end
    end
  end
  found[ev.file.path] = hits
  kitchen.highlights.set(ev.file, hl, { version = ev.version })
end

kitchen.on("file_opened", scan)
kitchen.on("file_changed", scan)

kitchen.command("list", function(ctx)
  local hits = ctx.viewer and ctx.viewer.path and found[ctx.viewer.path]
  if not hits or #hits == 0 then
    kitchen.notify("no TODOs in this file")
    return
  end
  local lines = {}
  for i, h in ipairs(hits) do
    if i > 5 then table.insert(lines, "…") break end
    table.insert(lines, "L" .. h.line .. " " .. h.text)
  end
  kitchen.notify(#hits .. " marker(s): " .. table.concat(lines, " · "))
end, { desc = "list TODO/FIXME markers in this file" })

kitchen.keymap("viewer", "<leader>t", "todo.list")
