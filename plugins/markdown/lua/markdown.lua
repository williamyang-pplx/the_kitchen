-- Markdown → kitchen.render rows. Block structure line by line (headings,
-- paragraphs, lists, quotes, fenced and indented code, tables, rules, front
-- matter), then inline styles (code, bold, italic, strikethrough, links).
-- Every source line belongs to exactly one row, so a comment on a row lands
-- on the lines it came from.

local M = {}

local LINK = "#7aa2f7"
local DIM = "comment"
local HEADING_SIZE = { 1.6, 1.35, 1.18, 1.05, 1.0, 1.0 }

local function len(s)
  return utf8.len(s) or #s
end

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

-- Inline ------------------------------------------------------------------

local function copy(t, k, v)
  local o = {}
  for a, b in pairs(t) do o[a] = b end
  o[k] = v
  return o
end

local function is_space(c)
  return c == "" or c:match("^%s$") ~= nil
end

local function is_alnum(c)
  return c ~= "" and c:match("^%w$") ~= nil
end

-- `s` → segments { {text, style}, … }; style keys as in a render span.
local function parse_inline(s, style, out)
  local i, buf = 1, {}
  local function flush()
    if #buf > 0 then
      out[#out + 1] = { table.concat(buf), style }
      buf = {}
    end
  end
  while i <= #s do
    local c = s:sub(i, i)
    local rest = s:sub(i)
    local handled = false
    if c == "\\" and s:sub(i + 1, i + 1):match("^%p$") then
      buf[#buf + 1] = s:sub(i + 1, i + 1)
      i = i + 2
      handled = true
    elseif c == "`" then
      local ticks = rest:match("^`+")
      local close = s:find(ticks, i + #ticks, true)
      -- The closing run must be exactly as long.
      while close and s:sub(close + #ticks, close + #ticks) == "`" do
        close = s:find(ticks, close + #ticks + 1, true)
      end
      if close then
        flush()
        local code = s:sub(i + #ticks, close - 1)
        if code:match("^ .* $") and code:match("%S") then code = code:sub(2, -2) end
        out[#out + 1] = { code, copy(style, "code", true) }
        i = close + #ticks
        handled = true
      end
    elseif c == "!" and rest:match("^!%[") then
      local alt, url, n = rest:match("^!%[([^%]]*)%]%(([^%)]*)%)()")
      if alt then
        flush()
        out[#out + 1] = { "[image: " .. (alt ~= "" and alt or url) .. "]", copy(style, "group", DIM) }
        i = i + n - 1
        handled = true
      end
    elseif c == "[" then
      local label, _, n = rest:match("^(%b[])%(([^%)]*)%)()")
      if not label then label, n = rest:match("^(%b[])%[[^%]]*%]()") end
      if label and label ~= "[]" then
        flush()
        local link = copy(copy(style, "underline", true), "group", LINK)
        parse_inline(label:sub(2, -2), link, out)
        i = i + n - 1
        handled = true
      end
    elseif c == "<" then
      local url, n = rest:match("^<(%a[%w+.-]*://[^>%s]+)>()")
      if url then
        flush()
        out[#out + 1] = { url, copy(copy(style, "underline", true), "group", LINK) }
        i = i + n - 1
        handled = true
      end
    elseif c == "*" or c == "_" or c == "~" then
      local run = rest:match("^%" .. c .. "+")
      local d = c == "~" and (#run >= 2 and "~~" or nil) or (#run >= 2 and c .. c or c)
      local before, after = s:sub(i - 1, i - 1), s:sub(i + #d, i + #d)
      -- Opens when followed by text; `_` not inside a word.
      local opens = d and not is_space(after) and not (c == "_" and is_alnum(before))
      if opens then
        local from = i + #d
        local close
        while true do
          close = s:find(d, from, true)
          if not close then break end
          local prev, nxt = s:sub(close - 1, close - 1), s:sub(close + #d, close + #d)
          local ok = not is_space(prev) and close > i + #d and not (c == "_" and is_alnum(nxt))
          -- `*` closing `*` must not be half of a `**`.
          if ok and #d == 1 and (nxt == c or prev == c) then ok = false end
          if ok then break end
          from = close + 1
        end
        if close then
          flush()
          local key = (d == "~~" and "strike") or (#d == 2 and "bold") or "italic"
          parse_inline(s:sub(i + #d, close - 1), copy(style, key, true), out)
          i = close + #d
          handled = true
        end
      end
      if not handled then
        buf[#buf + 1] = run
        i = i + #run
        handled = true
      end
    end
    if not handled then
      buf[#buf + 1] = c
      i = i + 1
    end
  end
  flush()
  return out
end

-- Inline markdown → text and its spans (1-based character columns).
function M.inline(s)
  local segs = parse_inline(s, {}, {})
  local parts, spans, col = {}, {}, 1
  for _, seg in ipairs(segs) do
    local text, style = seg[1], seg[2]
    local n = len(text)
    parts[#parts + 1] = text
    if next(style) and n > 0 then
      local sp = { col = col, end_col = col + n }
      for k, v in pairs(style) do sp[k] = v end
      spans[#spans + 1] = sp
    end
    col = col + n
  end
  return table.concat(parts), spans
end

-- Blocks ------------------------------------------------------------------

local function heading(line)
  local hashes, text = line:match("^ ? ? ?(#+)%s+(.-)%s*$")
  if not hashes then
    hashes = line:match("^ ? ? ?(#+)$")
    text = ""
  end
  if not hashes or #hashes > 6 then return nil end
  text = text:gsub("%s+#+$", ""):gsub("^#+$", "")
  return #hashes, text
end

local function is_rule(line)
  local s = line:gsub("%s", "")
  return #s >= 3 and (s:match("^%-+$") or s:match("^%*+$") or s:match("^_+$")) ~= nil and line:match("^ ? ? ?[-*_]") ~= nil
end

local function fence(line)
  local indent, marks, info = line:match("^( ? ? ?)(```+)%s*([^`]*)$")
  if not marks then indent, marks, info = line:match("^( ? ? ?)(~~~+)%s*(.*)$") end
  if marks then return marks, info:match("^(%S*)") end
end

local function list_item(line)
  local lead, mark, gap, body = line:match("^(%s*)([-*+])(%s+)(.*)$")
  local bullet
  if mark then
    bullet = "•"
  else
    lead, mark, gap, body = line:match("^(%s*)(%d+[.)])(%s+)(.*)$")
    bullet = mark
  end
  if not mark then
    -- An empty item ("-" alone).
    lead, mark = line:match("^(%s*)([-*+])$")
    if not mark then return nil end
    bullet, gap, body = "•", " ", ""
  end
  local box, after = body:match("^%[([ xX])%]%s+(.*)$")
  if box then
    bullet = box == " " and "☐" or "☑"
    body = after
  end
  local width = #expand_tabs(lead)
  return { depth = width // 2, bullet = bullet, body = body, content_col = width + #mark + #gap }
end

local function table_cells(line)
  local s = line:gsub("^%s*|", ""):gsub("|%s*$", "")
  local cells = {}
  -- Split on pipes that aren't escaped or inside code.
  local cur, in_code, i = {}, false, 1
  while i <= #s do
    local c = s:sub(i, i)
    if c == "\\" and s:sub(i + 1, i + 1) == "|" then
      cur[#cur + 1] = "|"
      i = i + 2
    else
      if c == "`" then in_code = not in_code end
      if c == "|" and not in_code then
        cells[#cells + 1] = table.concat(cur):match("^%s*(.-)%s*$")
        cur = {}
      else
        cur[#cur + 1] = c
      end
      i = i + 1
    end
  end
  cells[#cells + 1] = table.concat(cur):match("^%s*(.-)%s*$")
  return cells
end

local function is_table_sep(line)
  return line:find("-", 1, true) ~= nil and line:match("^%s*|?%s*:?%-+:?%s*[|%s:%-]*$") ~= nil and line:find("|", 1, true) ~= nil
end

-- A table's lines as aligned monospace rows.
local function table_rows(lines, first, last, out)
  local grid, widths = {}, {}
  for n = first, last do
    if n ~= first + 1 then
      local cells = {}
      for j, cell in ipairs(table_cells(lines[n])) do
        local text, spans = M.inline(cell)
        cells[j] = { text = text, spans = spans }
        widths[j] = math.max(widths[j] or 0, len(text))
      end
      grid[n] = cells
    end
  end
  for n = first, last do
    local parts, spans, col = {}, {}, 1
    local function add(s)
      parts[#parts + 1] = s
      col = col + len(s)
    end
    if n == first + 1 then
      for j, w in ipairs(widths) do
        if j > 1 then add("─┼─") end
        add(string.rep("─", w))
      end
      spans[1] = { col = 1, end_col = col, group = DIM }
    else
      for j, w in ipairs(widths) do
        local cell = grid[n][j] or { text = "", spans = {} }
        if j > 1 then
          spans[#spans + 1] = { col = col + 1, end_col = col + 2, group = DIM }
          add(" │ ")
        end
        for _, sp in ipairs(cell.spans) do
          local s = copy(sp, "col", sp.col + col - 1)
          s.end_col = sp.end_col + col - 1
          spans[#spans + 1] = s
        end
        if n == first then spans[#spans + 1] = { col = col, end_col = col + len(cell.text), bold = true } end
        add(cell.text .. string.rep(" ", w - len(cell.text)))
      end
    end
    out[#out + 1] = { line = n, text = table.concat(parts), spans = spans, block = "code" }
  end
end

local function starts_block(line, nxt)
  return line:match("^%s*$") or heading(line) or is_rule(line) or fence(line) or list_item(line)
    or line:match("^ ? ? ?>") or (line:find("|", 1, true) and nxt and is_table_sep(nxt))
end

function M.rows(text)
  local lines = {}
  for l in (text .. "\n"):gmatch("([^\n]*)\n") do
    lines[#lines + 1] = (l:gsub("\r$", ""))
  end
  if text:sub(-1) == "\n" then lines[#lines] = nil end
  local out = {}
  local n = 1
  local in_list = false
  local function row(r)
    out[#out + 1] = r
    return r
  end

  -- YAML front matter.
  if lines[1] == "---" then
    for e = 2, #lines do
      if lines[e] == "---" or lines[e] == "..." then
        for k = 1, e do
          row({ line = k, text = expand_tabs(lines[k]), block = "code", group = DIM })
        end
        n = e + 1
        break
      end
    end
  end

  while n <= #lines do
    local line = lines[n]
    local nxt = lines[n + 1]
    local marks, lang = fence(line)
    if line:match("^%s*$") then
      -- Blank lines: one empty row for the run.
      local e = n
      while lines[e + 1] and lines[e + 1]:match("^%s*$") do e = e + 1 end
      -- A list goes on past a blank line if the next line is indented or an item.
      local after = lines[e + 1]
      in_list = in_list and after ~= nil and (after:match("^%s+%S") ~= nil or list_item(after) ~= nil)
      row({ line = n, end_line = e > n and e or nil, text = "" })
      n = e + 1
    elseif marks then
      row({ line = n, text = lang, block = "code", group = DIM })
      local e = n + 1
      while lines[e] and not (lines[e]:match("^%s*" .. marks:sub(1, 1):rep(#marks) .. "+%s*$")) do
        row({ line = e, text = expand_tabs(lines[e]), block = "code" })
        e = e + 1
      end
      if lines[e] then row({ line = e, text = "", block = "code" }) end
      n = e + 1
      in_list = false
    elseif heading(line) then
      local level, title = heading(line)
      local t, spans = M.inline(title)
      row({ line = n, text = t, spans = spans, size = HEADING_SIZE[level], bold = true, group = level >= 5 and "#a9b1d6" or nil })
      n = n + 1
      in_list = false
    elseif is_rule(line) then
      row({ line = n, text = "", block = "rule" })
      n = n + 1
      in_list = false
    elseif not in_list and (line:match("^    ") or line:match("^\t")) and (n == 1 or lines[n - 1]:match("^%s*$")) then
      -- Indented code, up to the next line that isn't indented (or blank).
      local e = n
      while lines[e + 1] and (lines[e + 1]:match("^    ") or lines[e + 1]:match("^\t") or (lines[e + 1]:match("^%s*$") and lines[e + 2] and lines[e + 2]:match("^    "))) do
        e = e + 1
      end
      for k = n, e do
        row({ line = k, text = expand_tabs(lines[k]):gsub("^    ", ""), block = "code" })
      end
      n = e + 1
    elseif line:match("^ ? ? ?>") then
      local depth = #line:match("^ ? ? ?([>%s]*)"):gsub("%s", "")
      local body = line:gsub("^ ? ? ?[>%s]*", "", 1)
      local h, title = heading(body)
      local t, spans = M.inline(title or body)
      row({ line = n, text = t, spans = spans, block = "quote", indent = depth - 1, bold = h ~= nil })
      n = n + 1
    elseif line:find("|", 1, true) and nxt and is_table_sep(nxt) then
      local e = n + 1
      while lines[e + 1] and lines[e + 1]:find("|", 1, true) and not lines[e + 1]:match("^%s*$") do e = e + 1 end
      table_rows(lines, n, e, out)
      n = e + 1
      in_list = false
    else
      local item = list_item(line)
      local body, prefix, depth = line, "", 0
      if item then
        in_list = true
        body, prefix, depth = item.body, item.bullet .. " ", item.depth
      elseif in_list then
        -- A paragraph inside a list item.
        local lead = #expand_tabs(line:match("^%s*"))
        depth = math.max(lead - 2, 0) // 2
        prefix = "  "
      end
      -- Join the paragraph's lines (a hard break, two trailing spaces or a
      -- backslash, ends the row).
      local parts = { (body:gsub("^%s+", "")) }
      local e = n
      local function hard(s) return s:match("  $") or s:match("\\$") end
      while lines[e + 1] and not hard(lines[e]) and not starts_block(lines[e + 1], lines[e + 2]) do
        -- Setext underline: this paragraph is a heading.
        if not item and (lines[e + 1]:match("^ ? ? ?=+%s*$") or lines[e + 1]:match("^ ? ? ?%-+%s*$")) then break end
        e = e + 1
        parts[#parts + 1] = (lines[e]:gsub("^%s+", ""))
      end
      local setext = not item and lines[e + 1] and (lines[e + 1]:match("^ ? ? ?(=)=*%s*$") or lines[e + 1]:match("^ ? ? ?(%-)%-*%s*$"))
      local joined = table.concat(parts, " "):gsub("%s*\\$", ""):gsub("%s+$", "")
      local t, spans = M.inline(joined)
      if setext then
        local level = setext == "=" and 1 or 2
        row({ line = n, end_line = e + 1, text = t, spans = spans, size = HEADING_SIZE[level], bold = true })
        n = e + 2
        in_list = false
      else
        local plen = len(prefix)
        for _, sp in ipairs(spans) do
          sp.col, sp.end_col = sp.col + plen, sp.end_col + plen
        end
        if item then table.insert(spans, 1, { col = 1, end_col = plen, group = item.bullet:match("%d") and "#a9b1d6" or LINK }) end
        row({ line = n, end_line = e > n and e or nil, text = prefix .. t, spans = spans, indent = depth })
        n = e + 1
      end
    end
  end
  return out
end

return M
