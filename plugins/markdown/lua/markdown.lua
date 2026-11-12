-- Markdown → kitchen.render rows: the whole document as one row, rendered
-- in one pass as GitHub renders it. YAML front matter, which GitHub shows
-- as a table and CommonMark would misread, becomes a `yaml` code block.
-- A comment on the document lands on its last line.

local M = {}

function M.rows(text)
  if text == "" then return {} end
  local lines = {}
  for l in (text .. "\n"):gmatch("([^\n]*)\n") do
    lines[#lines + 1] = (l:gsub("\r$", ""))
  end
  if text:sub(-1) == "\n" then lines[#lines] = nil end
  local n = #lines
  local markdown = table.concat(lines, "\n")
  if lines[1] == "---" then
    for e = 2, n do
      if lines[e] == "---" or lines[e] == "..." then
        markdown = "```yaml\n" .. table.concat(lines, "\n", 2, e - 1) .. "\n```\n" .. table.concat(lines, "\n", e + 1, n)
        break
      end
    end
  end
  return { { line = 1, end_line = n > 1 and n or nil, markdown = markdown } }
end

return M
