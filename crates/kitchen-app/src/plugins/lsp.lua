-- kitchen.lsp: a small Language Server Protocol client for plugins.
--
--   require("kitchen.lsp").define {
--     name = "ty", filetypes = { "py", "pyi" }, language_id = "python",
--     root_markers = { "pyproject.toml", ".git" },
--     cmd = function(root) return "exec uvx ty server" end,   -- sh script, or an argv table
--     settings = {},          -- answers to workspace/configuration, by section
--     init_options = {},      -- initializationOptions
--     diagnostics = false,    -- show the server's publishDiagnostics
--   }
--
-- One server per (host, project root), started on the host where the code
-- lives when the viewer opens a matching file. The server answers the
-- viewer's `gd` (definition) and `gr` (references).

local M = {}

local servers = {} -- every define()d server, in order
local provided = false

local json = kitchen.json

local function q(s)
  return "'" .. tostring(s):gsub("'", "'\\''") .. "'"
end

local function under(path, dir)
  return dir ~= nil and dir ~= "" and path:sub(1, #dir + 1) == dir .. "/"
end

local function dirname(p)
  return p:match("^(.*)/[^/]*$") or "/"
end

local function filetype_of(path)
  return (path:match("%.([%w_]+)$") or ""):lower()
end

local function uri_of(path)
  return "file://" .. path:gsub("[^%w/%-%._~]", function(c)
    return string.format("%%%02X", c:byte())
  end)
end

local function path_of(uri)
  local p = uri:gsub("^file://", "")
  return (p:gsub("%%(%x%x)", function(h)
    return string.char(tonumber(h, 16))
  end))
end

-- Text of 1-based line `n` (without the newline), or "".
local function line_of(text, n)
  local pos = 1
  for _ = 2, n do
    local nl = text:find("\n", pos, true)
    if not nl then
      return ""
    end
    pos = nl + 1
  end
  local nl = text:find("\n", pos, true)
  return (text:sub(pos, nl and nl - 1 or #text):gsub("\r$", ""))
end

-- Characters -> the server's position units (and back), for one line.
local function to_units(line, chars, enc)
  if enc == "utf-32" then
    return chars
  end
  local units, n = 0, 0
  for _, cp in utf8.codes(line, true) do
    if n >= chars then
      break
    end
    if enc == "utf-8" then
      units = units + #utf8.char(cp)
    else
      units = units + (cp > 0xFFFF and 2 or 1)
    end
    n = n + 1
  end
  return units + math.max(0, chars - n)
end

local function from_units(line, units, enc)
  if enc == "utf-32" then
    return units
  end
  local u, n = 0, 0
  for _, cp in utf8.codes(line, true) do
    if u >= units then
      break
    end
    u = u + (enc == "utf-8" and #utf8.char(cp) or (cp > 0xFFFF and 2 or 1))
    n = n + 1
  end
  return n + math.max(0, units - u)
end

-- ------------------------------------------------------------ transport

local function send(c, msg)
  msg.jsonrpc = "2.0"
  local body = json.encode(msg)
  c.proc:write("Content-Length: " .. #body .. "\r\n\r\n" .. body)
end

local function notify(c, method, params)
  if c.ready then
    send(c, { method = method, params = params })
  else
    table.insert(c.queue, { method = method, params = params })
  end
end

local function request(c, method, params, cb)
  c.next_id = c.next_id + 1
  local id = c.next_id
  c.pending[id] = cb
  local msg = { id = id, method = method, params = params }
  local sent = c.ready or method == "initialize"
  if sent then
    send(c, msg)
  else
    table.insert(c.queue, msg)
  end
  -- Log stamps are in ms: with "-> N result(s)" they time the server.
  if method == "textDocument/definition" or method == "textDocument/references" then
    kitchen.log(string.format("%s #%d %s", method:gsub("^textDocument/", ""), id, sent and "sent" or "queued until the server is ready"))
  end
end

local function answer(s, c, msg)
  local result = json.null
  if msg.method == "workspace/configuration" then
    result = {}
    for i, item in ipairs((msg.params or {}).items or {}) do
      local v = item.section and s.settings[item.section]
      result[i] = v == nil and json.null or v
    end
    if #result == 0 then
      result = json.null
    end
  end
  send(c, { id = msg.id, result = result })
end

local function on_notification(s, c, msg)
  local p = msg.params or {}
  if msg.method == "textDocument/publishDiagnostics" and s.diagnostics then
    local doc = s.docs[c.host .. "\0" .. path_of(p.uri or "")]
    if not doc then
      return
    end
    local items = {}
    for _, d in ipairs(p.diagnostics or {}) do
      local st, en = d.range.start, d.range["end"]
      local l1, l2 = line_of(doc.text, st.line + 1), line_of(doc.text, en.line + 1)
      table.insert(items, {
        line = st.line + 1,
        col = from_units(l1, st.character, c.encoding) + 1,
        end_line = en.line + 1,
        end_col = from_units(l2, en.character, c.encoding) + 1,
        severity = d.severity or 1,
        message = d.message or "",
        code = d.code and tostring(d.code) or nil,
      })
    end
    kitchen.diagnostics.set(doc.file, items, { version = doc.kversion, ns = s.name })
  elseif msg.method == "window/showMessage" and p.type == 1 then
    kitchen.log(s.name .. ": " .. tostring(p.message))
  end
end

local function on_data(s, c, data)
  c.buf = c.buf .. data
  while true do
    local hdr_end = c.buf:find("\r\n\r\n", 1, true)
    if not hdr_end then
      return
    end
    local len = tonumber(c.buf:sub(1, hdr_end):match("[Cc]ontent%-[Ll]ength:%s*(%d+)"))
    local start = hdr_end + 4
    if len and #c.buf < start + len - 1 then
      return
    end
    local body = len and c.buf:sub(start, start + len - 1)
    c.buf = c.buf:sub(start + (len or 0))
    if body then
      local ok, msg = pcall(json.decode, body)
      if not ok then
        kitchen.log(s.name .. ": bad message: " .. tostring(msg))
      elseif msg.method and msg.id ~= nil then
        answer(s, c, msg)
      elseif msg.method then
        on_notification(s, c, msg)
      elseif msg.id ~= nil then
        local cb = c.pending[msg.id]
        c.pending[msg.id] = nil
        if cb then
          cb(msg.result, msg.error)
        end
      end
    end
  end
end

-- ------------------------------------------------------------ clients

local function on_exit(s, c, code)
  if s.clients[c.key] == c then
    s.clients[c.key] = nil
  end
  for _, cb in pairs(c.pending) do
    cb(nil, { message = s.name .. " exited" })
  end
  c.pending = {}
  if os.time() - c.started < 30 then
    s.failures[c.key] = (s.failures[c.key] or 0) + 1
    local msg = string.format("%s exited (code %s) in %s — see `kitchenctl plugin logs %s`", s.name, tostring(code), c.root, kitchen.name)
    if s.failures[c.key] >= 3 then
      msg = msg .. "; not restarting it (reload the plugin to retry)"
    end
    kitchen.notify(msg, "error")
  end
end

local function start(s, host, root, realwt)
  local key = host .. "\0" .. root
  local c = s.clients[key]
  if c then
    return c
  end
  if (s.failures[key] or 0) >= 3 then
    return nil
  end
  c = { key = key, host = host, root = root, realwt = realwt, buf = "", next_id = 0, pending = {}, ready = false, queue = {}, open = {}, encoding = "utf-16", started = os.time() }
  local cmd = s.cmd(root)
  local spec = {
    host = host,
    cwd = root,
    on_stdout = function(d)
      on_data(s, c, d)
    end,
    on_exit = function(code)
      on_exit(s, c, code)
    end,
  }
  if type(cmd) == "table" then
    spec.cmd = cmd
  else
    spec.sh = cmd
  end
  local ok, proc = pcall(kitchen.spawn, spec)
  if not ok then
    s.failures[key] = 3
    kitchen.notify(s.name .. ": " .. tostring(proc), "error")
    return nil
  end
  c.proc = proc
  s.clients[key] = c
  kitchen.log(string.format("%s: starting in %s:%s", s.name, host, root))
  local folder = { { uri = uri_of(root), name = root:match("[^/]+$") or root } }
  request(c, "initialize", {
    processId = json.null,
    rootUri = uri_of(root),
    rootPath = root,
    workspaceFolders = folder,
    initializationOptions = s.init_options,
    capabilities = {
      general = { positionEncodings = { "utf-32", "utf-16" } },
      textDocument = {
        synchronization = { dynamicRegistration = false },
        definition = { linkSupport = true },
        references = {},
        publishDiagnostics = {},
      },
      workspace = { configuration = true, workspaceFolders = true },
    },
    clientInfo = { name = "the kitchen" },
  }, function(result, err)
    if err or not result then
      kitchen.log(s.name .. ": initialize failed: " .. json.encode(err or "no result"))
      return
    end
    c.caps = result.capabilities or {}
    c.encoding = c.caps.positionEncoding or "utf-16"
    c.ready = true
    send(c, { method = "initialized", params = {} })
    local queued = c.queue
    c.queue = {}
    for _, m in ipairs(queued) do
      send(c, m)
    end
    kitchen.log(string.format("%s: ready (%s), %d queued message(s) sent", s.name, c.root, #queued))
  end)
  return c
end

-- Project root of `file` (nearest dir with a marker, up to the worktree),
-- found on the file's host; cached per directory.
local function find_root(s, file, cb)
  local dir = dirname(file.abs)
  local key = file.host .. "\0" .. dir
  local known = s.roots[key]
  if known then
    return cb(known.root, known.realwt)
  end
  if s.waiting[key] then
    table.insert(s.waiting[key], cb)
    return
  end
  s.waiting[key] = { cb }
  local tests = {}
  for _, m in ipairs(s.root_markers) do
    table.insert(tests, '[ -e "$d"/' .. q(m) .. " ]")
  end
  local script = "d=" .. q(dir) .. "; wt=" .. q(file.worktree) .. [[
; while :; do
  if ]] .. table.concat(tests, " || ") .. [[; then break; fi
  if [ "$d" = "$wt" ] || [ "$d" = / ]; then d="$wt"; break; fi
  d=$(dirname "$d")
done
echo "$d"; (cd "$wt" 2>/dev/null && pwd -P) || echo "$wt"]]
  kitchen.run({ sh = script, host = file.host, timeout = 20 }, function(res)
    local root, realwt = res.stdout:match("^([^\n]+)\n([^\n]*)")
    if res.code ~= 0 or not root then
      kitchen.log(s.name .. ": finding the project root failed: " .. res.stderr)
      root, realwt = file.worktree, file.worktree
    end
    s.roots[key] = { root = root, realwt = realwt }
    kitchen.log(string.format("%s: project root of %s is %s", s.name, dir, root))
    local waiting = s.waiting[key]
    s.waiting[key] = nil
    for _, f in ipairs(waiting) do
      f(root, realwt)
    end
  end)
end

-- The client for `file`: its project's, or (outside the worktree, e.g. an
-- installed package we jumped into) the one that last answered on its host.
local function with_client(s, file, cb)
  if not under(file.abs, file.worktree) then
    local k = s.last[file.host]
    return cb(k and s.clients[k])
  end
  find_root(s, file, function(root, realwt)
    cb(start(s, file.host, root, realwt))
  end)
end

local function sync(s, c, key)
  local d = s.docs[key]
  if not d then
    return
  end
  local uri = uri_of(d.file.abs)
  local have = c.open[key]
  if not have then
    notify(c, "textDocument/didOpen", { textDocument = { uri = uri, languageId = s.language_id, version = d.v, text = d.text } })
  elseif have < d.v then
    notify(c, "textDocument/didChange", { textDocument = { uri = uri, version = d.v }, contentChanges = { { text = d.text } } })
  end
  c.open[key] = d.v
end

-- ------------------------------------------------------------ queries

local function to_loc(s, c, file, x)
  local uri = x.targetUri or x.uri
  local range = x.targetSelectionRange or x.range
  if not uri or not range then
    return nil
  end
  local abs = path_of(uri)
  local line = range.start.line + 1
  local ch = range.start.character
  local doc = s.docs[file.host .. "\0" .. abs]
  if doc and c.encoding ~= "utf-32" then
    ch = from_units(line_of(doc.text, line), ch, c.encoding)
  end
  local path = abs
  for _, wt in ipairs({ file.worktree, c.realwt }) do
    if under(abs, wt) then
      path = abs:sub(#wt + 2)
      break
    end
  end
  return { path = path, line = line, col = ch + 1 }
end

local function server_for(ft)
  for _, s in ipairs(servers) do
    for _, f in ipairs(s.filetypes) do
      if f == ft then
        return s
      end
    end
  end
end

local function query(method, ctx, reply)
  local v = ctx.viewer or {}
  local file = v.file
  local s = file and v.line and server_for(filetype_of(file.path))
  if not s then
    return reply(nil)
  end
  with_client(s, file, function(c)
    if not c then
      return reply(nil)
    end
    local key = file.host .. "\0" .. file.abs
    sync(s, c, key)
    local doc = s.docs[key]
    local line = doc and line_of(doc.text, v.line) or ""
    local params = {
      textDocument = { uri = uri_of(file.abs) },
      position = { line = v.line - 1, character = to_units(line, (v.col or 1) - 1, c.encoding) },
    }
    if method == "textDocument/references" then
      params.context = { includeDeclaration = true }
    end
    request(c, method, params, function(result, err)
      if err then
        kitchen.log(s.name .. ": " .. method .. ": " .. tostring(err.message or json.encode(err)))
        return reply(nil)
      end
      if result and (result.uri or result.targetUri) then
        result = { result }
      end
      local out, seen = {}, {}
      for _, x in ipairs(result or {}) do
        local l = to_loc(s, c, file, x)
        local id = l and (l.path .. ":" .. l.line .. ":" .. l.col)
        if l and not seen[id] then
          seen[id] = true
          table.insert(out, l)
        end
      end
      s.last[file.host] = c.key
      local first = out[1]
      kitchen.log(string.format("%s %s:%d:%d -> %d result(s)%s", (method:gsub("^textDocument/", "")), file.path, v.line, v.col or 0, #out,
        first and string.format(", first %s:%d:%d", first.path, first.line, first.col) or ""))
      -- No definition: let the viewer try its text search.
      if #out == 0 and method == "textDocument/definition" then
        return reply(nil)
      end
      reply(out)
    end)
  end)
end

local function on_file(ev)
  local s = server_for(ev.filetype)
  if not s then
    return
  end
  local text = ev.text()
  if not text then
    return
  end
  local key = ev.file.host .. "\0" .. ev.file.abs
  local d = s.docs[key]
  if d then
    if d.kversion == ev.version then
      return
    end
    d.text, d.v, d.kversion = text, d.v + 1, ev.version
  else
    s.docs[key] = { file = ev.file, text = text, v = 1, kversion = ev.version }
  end
  with_client(s, ev.file, function(c)
    if c then
      sync(s, c, key)
    end
  end)
end

function M.define(spec)
  assert(type(spec.name) == "string", "lsp.define needs name")
  assert(spec.cmd, "lsp.define needs cmd")
  assert(type(spec.filetypes) == "table" and #spec.filetypes > 0, "lsp.define needs filetypes")
  local cmd = spec.cmd
  local s = {
    name = spec.name,
    filetypes = spec.filetypes,
    language_id = spec.language_id or spec.filetypes[1],
    root_markers = spec.root_markers or { ".git" },
    cmd = type(cmd) == "function" and cmd or function()
      return cmd
    end,
    settings = spec.settings or {},
    init_options = spec.init_options or json.null,
    diagnostics = spec.diagnostics == true,
    clients = {},
    roots = {},
    waiting = {},
    docs = {},
    last = {},
    failures = {},
  }
  table.insert(servers, s)
  local fts = {}
  for _, sv in ipairs(servers) do
    for _, f in ipairs(sv.filetypes) do
      table.insert(fts, f)
    end
  end
  kitchen.provide("definition", function(ctx, reply)
    query("textDocument/definition", ctx, reply)
  end, { filetypes = fts })
  kitchen.provide("references", function(ctx, reply)
    query("textDocument/references", ctx, reply)
  end, { filetypes = fts })
  if not provided then
    provided = true
    kitchen.on("file_opened", on_file)
    kitchen.on("file_changed", on_file)
  end
  return s
end

-- For tests and plugins that want the plumbing.
M._line_of, M._to_units, M._from_units, M._uri_of, M._path_of = line_of, to_units, from_units, uri_of, path_of

return M
