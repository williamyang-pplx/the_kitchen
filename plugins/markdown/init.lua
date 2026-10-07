-- markdown: ⌘⇧V in the viewer shows Markdown files rendered, and back.
-- Every open .md file is rendered up front (lua/markdown.lua) so the toggle
-- is instant. Each row keeps the source lines it came from: ⌘M on a row
-- comments on those lines (a paragraph: its whole range), and review
-- comments show under the row that holds their line.
local markdown = require("markdown")

local function render(ev)
  local text = ev.text()
  if not text then return end
  kitchen.render.set(ev.file, markdown.rows(text), { version = ev.version })
end

kitchen.on("file_opened", render)
kitchen.on("file_changed", render)

kitchen.command("toggle", function(ctx)
  kitchen.render.show(ctx.tab)
end, { desc = "show Markdown rendered / as text" })

kitchen.keymap("cmd+shift+v", "markdown.toggle", { when = "viewer" })
