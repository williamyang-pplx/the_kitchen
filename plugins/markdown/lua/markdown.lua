-- Markdown → kitchen.render rows. The viewer draws prose with its Markdown
-- renderer (egui_commonmark), one block per row: a heading, a paragraph, a
-- table, a quote, a top-level list item with what it holds. Top-level fenced
-- and indented code, and front matter, stay one row per line (highlighted
-- by the fence's language), so a comment can pick out single lines. Every
-- source line belongs to exactly one row, so a comment on a row lands on the
-- lines it came from.

local M = {}

local DIM = "comment"

local function expand_tabs(s)
  if not s:find("\t", 1, true) then return s end
  local out, col = {}, 0
  for _, c in utf8.codes(s) do
    if c == 9 then
      local n = 4 - col % 4
      out[#out + 1] = string.rep(" ", n)
      col = col + n
    else
      out[#out + 1] = utf8.char(c)
      col = col + 1
    end
  end
  return table.concat(out)
end

local function is_blank(line)
  return line:match("^%s*$") ~= nil
end

-- The checks below are for unindented lines: an indented one belongs to
-- the block above it (a list item's content, a nested list).

local function fence(line)
  local marks, info = line:match("^(```+)%s*([^`]*)$")
  if not marks then marks, info = line:match("^(~~~+)%s*(.*)$") end
  if marks then return marks, info:match("^(%S*)") end
end

local function is_heading(line)
  local hashes = line:match("^(#+)%s") or line:match("^(#+)$")
  return hashes ~= nil and #hashes <= 6
end

local function is_item(line)
  return line:match("^[-*+]%s") ~= nil or line:match("^[-*+]$") ~= nil or line:match("^%d+[.)]%s") ~= nil
end

-- `[label]: url` (not a footnote): blocks render separately, so each block
-- that may use one gets them all.
local function is_link_def(line)
  return line:match("^ ? ? ?%[[^%]^][^%]]*%]:") ~= nil
end

function M.rows(text)
  local lines = {}
  for l in (text .. "\n"):gmatch("([^\n]*)\n") do
    lines[#lines + 1] = (l:gsub("\r$", ""))
  end
  if text:sub(-1) == "\n" then lines[#lines] = nil end
  local defs = {}
  for _, l in ipairs(lines) do
    if is_link_def(l) then defs[#defs + 1] = l end
  end
  local defs_text = #defs > 0 and "\n\n" .. table.concat(defs, "\n") or ""
  local out = {}
  local function row(r)
    out[#out + 1] = r
  end
  local function code(n, s, lang)
    row({ line = n, text = expand_tabs(s), block = "code", lang = lang ~= "" and lang or nil })
  end

  local n = 1
  -- YAML front matter.
  if lines[1] == "---" then
    for e = 2, #lines do
      if lines[e] == "---" or lines[e] == "..." then
        row({ line = 1, text = "", block = "code" })
        for k = 2, e - 1 do code(k, lines[k], "yaml") end
        row({ line = e, text = "", block = "code" })
        n = e + 1
        break
      end
    end
  end

  while n <= #lines do
    local line = lines[n]
    local marks, lang = fence(line)
    if is_blank(line) then
      -- Blank lines join the row above; after code they are a gap of their own.
      local e = n
      while lines[e + 1] and is_blank(lines[e + 1]) do e = e + 1 end
      local last = out[#out]
      if last and last.block ~= "code" then
        last.end_line = e
      else
        row({ line = n, end_line = e > n and e or nil, text = "" })
      end
      n = e + 1
    elseif marks then
      row({ line = n, text = lang, block = "code", group = DIM })
      local close = "^%s*" .. marks:sub(1, 1):rep(#marks) .. "+%s*$"
      local e = n + 1
      while lines[e] and not lines[e]:match(close) do
        code(e, lines[e], lang)
        e = e + 1
      end
      if lines[e] then row({ line = e, text = "", block = "code" }) end
      n = e + 1
    elseif line:match("^    ") or line:match("^\t") then
      -- Indented code, up to the next line that isn't indented (or blank).
      local e = n
      while lines[e + 1] and (lines[e + 1]:match("^    ") or lines[e + 1]:match("^\t") or (is_blank(lines[e + 1]) and lines[e + 2] and lines[e + 2]:match("^    "))) do
        e = e + 1
      end
      for k = n, e do
        row({ line = k, text = (expand_tabs(lines[k]):gsub("^    ", "")), block = "code" })
      end
      n = e + 1
    else
      -- A Markdown block: up to a blank line (in a list item: one followed
      -- by an unindented line), or an unindented heading, fence or list item.
      -- A heading is one line.
      local e, item = n, is_item(line)
      while not is_heading(line) and lines[e + 1] do
        local nxt = lines[e + 1]
        if is_blank(nxt) then
          local k = e + 1
          while lines[k] and is_blank(lines[k]) do k = k + 1 end
          if not item or not lines[k] or not lines[k]:match("^%s") then break end
          e = k
        elseif is_heading(nxt) or fence(nxt) or is_item(nxt) then
          break
        else
          e = e + 1
        end
      end
      local src = table.concat(lines, "\n", n, e)
      if defs_text ~= "" and src:find("]", 1, true) then src = src .. defs_text end
      row({ line = n, end_line = e > n and e or nil, markdown = src })
      n = e + 1
    end
  end
  return out
end

return M
