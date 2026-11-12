-- Markdown → kitchen.render rows. The whole document is one row, drawn in
-- one pass by the viewer's Markdown renderer (egui_commonmark), so its
-- spacing, lists and code are the renderer's. YAML front matter, which
-- GitHub shows as a table and CommonMark would misread, stays a code row
-- per line above it. A comment on the document lands on its last line.

local M = {}

function M.rows(text)
  if text == "" then return {} end
  local lines = {}
  for l in (text .. "\n"):gmatch("([^\n]*)\n") do
    lines[#lines + 1] = (l:gsub("\r$", ""))
  end
  if text:sub(-1) == "\n" then lines[#lines] = nil end
  local out = {}
  local n = 1
  if lines[1] == "---" then
    for e = 2, #lines do
      if lines[e] == "---" or lines[e] == "..." then
        out[#out + 1] = { line = 1, text = "", block = "code" }
        for k = 2, e - 1 do
          out[#out + 1] = { line = k, text = lines[k], block = "code", lang = "yaml" }
        end
        out[#out + 1] = { line = e, text = "", block = "code" }
        n = e + 1
        break
      end
    end
  end
  if n <= #lines then
    out[#out + 1] = { line = n, end_line = #lines > n and #lines or nil, markdown = table.concat(lines, "\n", n, #lines) }
  end
  return out
end

return M
