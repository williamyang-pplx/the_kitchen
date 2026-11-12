-- markdown: ⌘⇧V in the viewer shows Markdown files rendered, and back.
-- Every open .md file is rendered up front (lua/markdown.lua) so the toggle
-- is instant: the whole document is one row the viewer's renderer draws in
-- one pass. ⌘M on it comments on the document's lines, and review comments
-- show under it; ⌘⇧V back to the text comments on single lines.
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
