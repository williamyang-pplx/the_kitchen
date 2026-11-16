-- kitchen.lsp: a small Language Server Protocol client for plugins.
--
--   local server = require("kitchen.lsp").define {
--     name = "ty", filetypes = { "py", "pyi" }, language_id = "python",
--     root_markers = { "pyproject.toml", ".git" },    -- or tiers, first match wins:
--     --   { { "ty.toml", { file = "pyproject.toml", contains = "[tool.ty" } }, { "pyproject.toml" } }
--     cmd = function(root) return "exec uvx ty server" end,   -- sh script, or an argv table
--     prepare = function(ctx, done) done { settings = {}, init_options = {} } end,
--                             -- per root, before its server starts: ctx {host, root, worktree}
--     settings = {},          -- answers to workspace/configuration, by section
--     init_options = {},      -- initializationOptions
--     diagnostics = false,    -- show the server's publishDiagnostics
--     max_rss_mb = 6144,      -- restart a server using more memory (0: no cap)
--     idle_minutes = 15,      -- stop a server nobody asked for that long (0: never)
--   }
--
-- One server per (host, project root), started on the host where the code
-- lives when the viewer opens a matching file. The server answers the
-- viewer's `gd` (definition) and `gr` (references). A request replaces the
-- older one of its method. While servers run, a sweep every few seconds
-- cancels requests nobody waits for any more, restarts servers over their
-- memory cap, stops idle ones, and keeps a few per host. It also tells
-- servers which files changed on disk (an agent's edit, a checkout): it polls
-- git status and mtimes, because the viewer only watches the file it shows.

local M = {}

local servers = {} -- every define()d server, in order
local provided = false
local ticking = false
local tick -- the sweep, below

local TICK_SECS = 5 -- between sweeps
local REQUEST_TIMEOUT = 20 -- s; the viewer stops waiting then too
local MAX_SERVERS = 3 -- per host; starting one more stops the least recently used
local KILL_GRACE_SECS = 2 -- between `exit` and the kill, for servers we stop
local HEALTHY_AFTER_SECS = 30 -- uptime after which an exit no longer counts as a crash
local MAX_OPEN_DOCS = 32 -- documents kept open per server; older ones get didClose
local MAX_CHANGED_FILES = 2000 -- more changed files than this at once: restart the server
local POLL_COST = 10 -- poll at most every POLL_COST × the last poll's duration (git status is slow on big repos)
-- Changes to these restart the server (its project or environment changed).
local RESTART_ON = { ["uv.lock"] = true, ["ty.toml"] = true, ["pyproject.toml"] = true }
local NAV = { ["textDocument/definition"] = true, ["textDocument/references"] = true }

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

-- Stop waiting for request `id`: the server is told, the caller gets nil.
local function cancel(c, id, why)
  local p = c.pending[id]
  if not p then
    return
  end
  c.pending[id] = nil
  if c.inflight[p.method] == id then
    c.inflight[p.method] = nil
  end
  local queued = false
  for i, m in ipairs(c.queue) do
    if m.id == id then
      table.remove(c.queue, i)
      queued = true
      break
    end
  end
  if not queued and c.ready then
    send(c, { method = "$/cancelRequest", params = { id = id } })
  end
  kitchen.log(string.format("%s #%d cancelled: %s", (p.method:gsub("^textDocument/", "")), id, why))
  p.cb(nil, { message = "cancelled: " .. why, cancelled = true })
end

-- A gd / gr is being answered.
local function busy(c)
  for _, p in pairs(c.pending) do
    if NAV[p.method] then
      return true
    end
  end
  return false
end

local function request(c, method, params, cb)
  c.next_id = c.next_id + 1
  local id = c.next_id
  if NAV[method] and c.inflight[method] then
    cancel(c, c.inflight[method], "superseded")
  end
  c.pending[id] = { cb = cb, method = method, sent = os.time() }
  if NAV[method] then
    c.inflight[method] = id
  end
  local msg = { id = id, method = method, params = params }
  local sent = c.ready or method == "initialize"
  if sent then
    send(c, msg)
  else
    table.insert(c.queue, msg)
  end
  -- Log stamps are in ms: with "-> N result(s)" they time the server.
  if NAV[method] then
    kitchen.log(string.format("%s #%d %s", method:gsub("^textDocument/", ""), id, sent and "sent" or "queued until the server is ready"))
  end
  return id
end

local function answer(s, c, msg)
  local result = json.null
  if msg.method == "workspace/configuration" then
    result = {}
    for i, item in ipairs((msg.params or {}).items or {}) do
      -- What `prepare` gave this root wins over the server-wide settings.
      local v = item.section and ((c.settings or {})[item.section] or s.settings[item.section])
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
        local p = c.pending[msg.id]
        c.pending[msg.id] = nil
        if p then
          if c.inflight[p.method] == msg.id then
            c.inflight[p.method] = nil
          end
          p.cb(msg.result, msg.error)
        end
      end
    end
  end
end

-- ------------------------------------------------------------ clients

-- The server's stderr, whole lines: the pid line `wrap_cmd` prints, then logs.
local function on_stderr(s, c, chunk)
  c.errbuf = c.errbuf .. chunk
  while true do
    local nl = c.errbuf:find("\n", 1, true)
    if not nl then
      break
    end
    local line = c.errbuf:sub(1, nl - 1):gsub("\r$", "")
    c.errbuf = c.errbuf:sub(nl + 1)
    local pid = line:match("^kitchen%-lsp%-pid (%d+)$")
    if pid and not c.pid then
      c.pid = pid
    elseif line ~= "" then
      kitchen.log("[" .. s.name .. "] " .. line)
    end
  end
  if #c.errbuf > 65536 then
    kitchen.log("[" .. s.name .. "] " .. c.errbuf)
    c.errbuf = ""
  end
end

-- Stop a server on purpose: its requests are cancelled, it is told to exit,
-- and the sweep kills it if it is still there after KILL_GRACE_SECS.
local function stop_client(s, c, why)
  if c.stopping then
    return
  end
  c.stopping = why
  if s.clients[c.key] == c then
    s.clients[c.key] = nil
  end
  local ids = {}
  for id in pairs(c.pending) do
    table.insert(ids, id)
  end
  for _, id in ipairs(ids) do
    cancel(c, id, why)
  end
  if c.ready then
    send(c, { method = "exit" })
  end
  c.proc:close()
  c.kill_at = os.time() + KILL_GRACE_SECS
  table.insert(s.stopped, c)
  kitchen.log(string.format("%s: stopped in %s:%s (%s)", s.name, c.host, c.root, why))
end

local function on_exit(s, c, code)
  if s.clients[c.key] == c then
    s.clients[c.key] = nil
  end
  local pending = c.pending
  c.pending, c.inflight, c.exited = {}, {}, true
  for _, p in pairs(pending) do
    p.cb(nil, { message = s.name .. " exited" })
  end
  local up = os.time() - c.started
  kitchen.log(string.format("%s exited (code %s) after %ds in %s:%s", s.name, tostring(code), up, c.host, c.root))
  if c.stopping then
    return
  end
  local logs = string.format(" — see `kitchenctl plugin logs %s`", kitchen.name)
  if up >= HEALTHY_AFTER_SECS then
    s.failures[c.key] = 0
    kitchen.notify(string.format("%s exited (code %s) in %s; it restarts on the next gd/gr%s", s.name, tostring(code), c.root, logs), "error")
    return
  end
  s.failures[c.key] = (s.failures[c.key] or 0) + 1
  if s.failures[c.key] >= 3 then
    kitchen.notify(string.format("%s disabled in %s after 3 crashes; reload the plugin to retry%s", s.name, c.root, logs), "error")
  else
    kitchen.notify(string.format("%s exited (code %s) in %s%s", s.name, tostring(code), c.root, logs), "error")
  end
end

-- The server command, printing its pid first (the shell then execs into the
-- server, so that is the server's pid) for the memory check.
local function wrap_cmd(cmd)
  local script = cmd
  if type(cmd) == "table" then
    local words = {}
    for _, w in ipairs(cmd) do
      table.insert(words, q(w))
    end
    script = "exec " .. table.concat(words, " ")
  end
  return "echo kitchen-lsp-pid $$ >&2; " .. script
end

local function ensure_ticking()
  if ticking then
    return
  end
  ticking = true
  kitchen.every("lsp", TICK_SECS, function()
    tick()
  end, { desc = "language servers: request timeouts, memory, idle servers", first_delay = TICK_SECS })
end

local function start(s, host, root, realwt, prep)
  prep = prep or {}
  local key = host .. "\0" .. root
  local c = s.clients[key]
  if c then
    return c
  end
  if (s.failures[key] or 0) >= 3 then
    return nil
  end
  -- A few servers per host: starting one more stops the least recently used.
  local count, lru = 0, nil
  for _, o in pairs(s.clients) do
    if o.host == host then
      count = count + 1
      if not lru or o.last_used < lru.last_used then
        lru = o
      end
    end
  end
  if lru and count >= s.max_servers then
    stop_client(s, lru, "more than " .. s.max_servers .. " servers on " .. host)
  end
  c = {
    key = key, host = host, root = root, realwt = realwt, buf = "", errbuf = "", next_id = 0,
    pending = {}, inflight = {}, ready = false, queue = {}, open = {}, open_order = {}, encoding = "utf-16",
    started = os.time(), last_used = os.time(), polled_at = 0, poll_every = TICK_SECS,
    settings = prep.settings,
  }
  local spec = {
    host = host,
    cwd = root,
    sh = wrap_cmd(s.cmd(root)),
    on_stdout = function(d)
      on_data(s, c, d)
    end,
    on_stderr = function(d)
      on_stderr(s, c, d)
    end,
    on_exit = function(code)
      on_exit(s, c, code)
    end,
  }
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
    initializationOptions = prep.init_options or s.init_options,
    capabilities = {
      general = { positionEncodings = { "utf-32", "utf-16" } },
      textDocument = {
        synchronization = { dynamicRegistration = false },
        definition = { linkSupport = true },
        references = {},
        publishDiagnostics = {},
      },
      workspace = { configuration = true, workspaceFolders = true, didChangeWatchedFiles = { dynamicRegistration = true } },
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
  ensure_ticking()
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
  -- Each tier walks up from the file to the worktree; the first tier with a
  -- match wins, so a marker that names the project beats a nearer one that
  -- only names a package.
  local script = "d0=" .. q(dir) .. "; wt=" .. q(file.worktree) .. "; root=\n"
  for _, tier in ipairs(s.root_tiers) do
    local tests = {}
    for _, m in ipairs(tier) do
      if type(m) == "table" then
        table.insert(tests, '{ [ -f "$d"/' .. q(m.file) .. " ] && grep -qF -- " .. q(m.contains) .. ' "$d"/' .. q(m.file) .. "; }")
      else
        table.insert(tests, '[ -e "$d"/' .. q(m) .. " ]")
      end
    end
    script = script .. [[if [ -z "$root" ]; then d=$d0; while :; do
  if ]] .. table.concat(tests, " || ") .. [[; then root=$d; break; fi
  if [ "$d" = "$wt" ] || [ "$d" = / ]; then break; fi
  d=$(dirname "$d")
done; fi
]]
  end
  script = script .. [[[ -n "$root" ] || root=$wt
echo "$root"; (cd "$wt" 2>/dev/null && pwd -P) || echo "$wt"]]
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

-- The plugin's `prepare` result for (host, root), asked once and cached.
local function prepare_root(s, host, root, realwt, cb)
  if not s.prepare then
    return cb({})
  end
  local key = host .. "\0" .. root
  if s.prepared[key] then
    return cb(s.prepared[key])
  end
  if s.preparing[key] then
    table.insert(s.preparing[key], cb)
    return
  end
  s.preparing[key] = { cb }
  local function done(result)
    if not s.preparing[key] then
      return -- called twice
    end
    s.prepared[key] = result or {}
    local waiting = s.preparing[key]
    s.preparing[key] = nil
    for _, f in ipairs(waiting) do
      f(s.prepared[key])
    end
  end
  local ok, err = pcall(s.prepare, { host = host, root = root, worktree = realwt }, done)
  if not ok then
    kitchen.log(s.name .. ": prepare failed: " .. tostring(err))
    done({})
  end
end

-- The client for `file`: its project's, or (outside the worktree, e.g. an
-- installed package we jumped into) the one that last answered on its host.
local function with_client(s, file, cb)
  if not under(file.abs, file.worktree) then
    local k = s.last[file.host]
    return cb(k and s.clients[k])
  end
  find_root(s, file, function(root, realwt)
    prepare_root(s, file.host, root, realwt, function(prep)
      cb(start(s, file.host, root, realwt, prep))
    end)
  end)
end

local function sync(s, c, key)
  local d = s.docs[key]
  -- A replayed file may have changed on disk since: the server reads it itself.
  if not d or d.stale then
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
  for i, k in ipairs(c.open_order) do
    if k == key then
      table.remove(c.open_order, i)
      break
    end
  end
  table.insert(c.open_order, key)
  if #c.open_order > MAX_OPEN_DOCS then
    local old = table.remove(c.open_order, 1)
    local od = s.docs[old]
    if od then
      notify(c, "textDocument/didClose", { textDocument = { uri = uri_of(od.file.abs) } })
    end
    c.open[old] = nil
  end
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
    c.last_used = os.time()
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
        if not err.cancelled then
          kitchen.log(s.name .. ": " .. method .. ": " .. tostring(err.message or json.encode(err)))
        end
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
  if ev.replay == true then
    -- Re-sent after a reload, maybe stale: kept for column maths and
    -- diagnostics, never opened on a server, and no server is started.
    if not d then
      s.docs[key] = { file = ev.file, text = text, v = 1, kversion = ev.version, stale = true }
    end
    return
  end
  if d then
    if d.kversion == ev.version and not d.stale then
      return
    end
    d.text, d.v, d.kversion, d.stale = text, d.v + 1, ev.version, nil
  else
    s.docs[key] = { file = ev.file, text = text, v = 1, kversion = ev.version }
  end
  with_client(s, ev.file, function(c)
    if c then
      c.last_used = os.time()
      sync(s, c, key)
    end
  end)
end

-- What git says is dirty under the root (with mtimes), the HEAD, and, when
-- HEAD moved since `old_head`, the files that move touched. `recheck`: paths
-- dirty last time, stat'ed again (a revert or a commit makes them vanish).
local SNAPSHOT_SCRIPT = [[cd %s 2>/dev/null || exit 0
head=$(git rev-parse HEAD 2>/dev/null) || { echo nogit; exit 0; }
echo "head $head"
mtime() { stat -c %%Y -- "$1" 2>/dev/null || stat -f %%m -- "$1" 2>/dev/null || echo gone; }
git -c core.quotePath=false status --porcelain=v1 -uall -- '*.py' '*.pyi' pyproject.toml ty.toml uv.lock 2>/dev/null |
while IFS= read -r l; do
  p=${l#???}; p=${p##* -> }
  printf 's %%s %%s %%s
' "$(mtime "$p")" "$(printf %%s "$l" | cut -c1-2 | tr ' ' _)" "$p"
done
for p in %s; do printf 'x %%s %%s
' "$(mtime "$p")" "$p"; done
old=%s
if [ -n "$old" ] && [ "$old" != "$head" ]; then
  git -c core.quotePath=false diff --name-only "$old" "$head" -- '*.py' '*.pyi' pyproject.toml ty.toml uv.lock 2>/dev/null | sed 's/^/d /'
fi]]

local function send_changes(s, c, changes)
  if #changes == 0 then
    return
  end
  for _, ch in ipairs(changes) do
    if RESTART_ON[ch.path] then
      kitchen.log(string.format("%s: %s changed in %s; restarting", s.name, ch.path, c.root))
      return M.restart(s, c.host, c.root)
    end
  end
  if #changes > MAX_CHANGED_FILES then
    kitchen.log(string.format("%s: %d files changed in %s; restarting", s.name, #changes, c.root))
    return M.restart(s, c.host, c.root)
  end
  local list = {}
  for _, ch in ipairs(changes) do
    table.insert(list, { uri = uri_of(c.root .. "/" .. ch.path), type = ch.type })
  end
  notify(c, "workspace/didChangeWatchedFiles", { changes = list })
  kitchen.log(string.format("%s: %d file(s) changed on disk in %s", s.name, #list, c.root))
end

local function poll_changes(s, c)
  local now = os.time()
  if not c.ready or c.polling or c.nogit or c.stopping or now - c.polled_at < c.poll_every then
    return
  end
  c.polling, c.polled_at = true, now
  local snap = c.snapshot
  local recheck = {}
  for path in pairs(snap and snap.files or {}) do
    table.insert(recheck, q(path))
  end
  local script = string.format(SNAPSHOT_SCRIPT, q(c.root), table.concat(recheck, " "), q(snap and snap.head or ""))
  local started = os.time()
  kitchen.run({ sh = script, host = c.host, timeout = 30 }, function(res)
    c.polling = false
    local took = os.time() - started
    c.poll_every = math.max(TICK_SECS, took * POLL_COST)
    if c.stopping or s.clients[c.key] ~= c or res.code ~= 0 then
      return
    end
    if res.stdout:match("^nogit") then
      c.nogit = true
      return
    end
    local new = { head = res.stdout:match("^head (%S+)"), files = {} }
    local again, moved = {}, {}
    for line in res.stdout:gmatch("[^\n]+") do
      local m, xy, path = line:match("^s (%S+) (%S+) (.+)$")
      if m then
        new.files[path] = { mtime = m, untracked = xy == "??" }
      end
      local xm, xpath = line:match("^x (%S+) (.+)$")
      if xm then
        again[xpath] = xm
      end
      local dpath = line:match("^d (.+)$")
      if dpath then
        table.insert(moved, dpath)
      end
    end
    c.snapshot = new
    if not snap then
      kitchen.log(string.format("%s: watching %s (%d dirty file(s))", s.name, c.root, (function()
        local n = 0
        for _ in pairs(new.files) do
          n = n + 1
        end
        return n
      end)()))
      return
    end
    local changes, seen = {}, {}
    local function add(path, type)
      if not seen[path] then
        seen[path] = true
        table.insert(changes, { path = path, type = type })
      end
    end
    for path, f in pairs(new.files) do
      local was = snap.files[path]
      if f.mtime == "gone" then
        if not was or was.mtime ~= "gone" then
          add(path, 3)
        end
      elseif not was then
        add(path, f.untracked and 1 or 2)
      elseif was.mtime ~= f.mtime then
        add(path, 2)
      end
    end
    for path, was in pairs(snap.files) do
      if not new.files[path] then
        local m = again[path]
        if m == "gone" then
          if was.mtime ~= "gone" then
            add(path, 3)
          end
        else
          add(path, 2) -- reverted or committed: the content changed back
        end
      end
    end
    for _, path in ipairs(moved) do
      add(path, 2)
    end
    send_changes(s, c, changes)
  end)
end

-- Each live server's memory: its process tree's RSS, one `ps` per host.
local RSS_SCRIPT = [[ps -A -o pid=,ppid=,rss= | awk -v roots="%s" '
{ parent[$1] = $2; rss[$1] = $3 }
END {
  n = split(roots, r, " ")
  for (i = 1; i <= n; i++) {
    total = 0
    for (p in rss) {
      q = p
      for (d = 0; d < 64 && q != "" && q != "0"; d++) {
        if (q == r[i]) { total += rss[p]; break }
        q = parent[q]
      }
    }
    print r[i], total
  }
}']]

local function check_rss(s)
  if s.max_rss_mb <= 0 then
    return
  end
  local by_host = {}
  for _, c in pairs(s.clients) do
    if c.pid then
      by_host[c.host] = by_host[c.host] or {}
      by_host[c.host][c.pid] = c
    end
  end
  for host, clients in pairs(by_host) do
    if not s.rss_busy[host] then
      s.rss_busy[host] = true
      local pids = {}
      for pid in pairs(clients) do
        table.insert(pids, pid)
      end
      kitchen.run({ sh = string.format(RSS_SCRIPT, table.concat(pids, " ")), host = host, timeout = 20 }, function(res)
        s.rss_busy[host] = nil
        for pid, kib in res.stdout:gmatch("(%d+) (%d+)") do
          local c = clients[pid]
          local mb = tonumber(kib) / 1024
          if c and not c.stopping and mb > s.max_rss_mb then
            kitchen.notify(string.format("%s used %.1f GB in %s; restarted it (max_rss_mb is %d)", s.name, mb / 1024, c.root, s.max_rss_mb), "error")
            stop_client(s, c, string.format("memory: %d MB", math.floor(mb)))
          end
        end
      end)
    end
  end
end

tick = function()
  local now = os.time()
  for _, s in ipairs(servers) do
    for _, c in pairs(s.clients) do
      local late = {}
      for id, p in pairs(c.pending) do
        if NAV[p.method] and now - p.sent >= s.request_timeout then
          table.insert(late, id)
        end
      end
      for _, id in ipairs(late) do
        cancel(c, id, "no answer in " .. s.request_timeout .. "s")
      end
      if c.restart_when_idle and not busy(c) then
        M.restart(s, c.host, c.root)
      elseif s.idle_minutes > 0 and now - c.last_used >= s.idle_minutes * 60 then
        stop_client(s, c, "idle")
      else
        poll_changes(s, c)
      end
    end
    check_rss(s)
    local still = {}
    for _, c in ipairs(s.stopped) do
      if not c.exited then
        if now >= c.kill_at then
          c.proc:kill()
        end
        table.insert(still, c)
      end
    end
    s.stopped = still
  end
end

-- Stop the server at (host, root) and start it again for a document under the
-- root (its project, environment or many files changed).
function M.restart(s, host, root)
  local key = host .. "\0" .. root
  local c = s.clients[key]
  if c and busy(c) then
    -- Never urgent: let the server answer what it works on, the sweep
    -- restarts it once nothing is pending (or the request times out).
    if not c.restart_when_idle then
      c.restart_when_idle = true
      kitchen.log(string.format("%s: restarting %s once its gd/gr is answered", s.name, root))
    end
    return
  end
  if c then
    stop_client(s, c, "restart")
  end
  -- Ask `prepare` again: its settings may be what changed.
  s.prepared[key] = nil
  s.failures[key] = nil
  for key, d in pairs(s.docs) do
    if d.file.host == host and not d.stale and (d.file.abs == root or under(d.file.abs, root)) then
      with_client(s, d.file, function(nc)
        if nc then
          sync(s, nc, key)
        end
      end)
      return
    end
  end
end

-- The plugin unloads: cancel what servers still work on and tell them to
-- exit; the host then closes their stdin and kills what is left.
local function on_stop()
  for _, s in ipairs(servers) do
    for _, c in pairs(s.clients) do
      local ids = {}
      for id in pairs(c.pending) do
        table.insert(ids, id)
      end
      for _, id in ipairs(ids) do
        cancel(c, id, "unload")
      end
      if c.ready then
        send(c, { method = "exit" })
      end
    end
  end
end

-- Root markers as tiers: a flat list of markers is one tier (any marker,
-- nearest directory); a list of lists is tiers in priority order. A marker
-- is a file name or { file = name, contains = text }.
local function tiers_of(markers)
  local tiered = false
  for _, m in ipairs(markers) do
    if type(m) == "table" and m.file == nil then
      tiered = true
    end
  end
  if not tiered then
    return { markers }
  end
  local tiers = {}
  for _, m in ipairs(markers) do
    table.insert(tiers, (type(m) == "table" and m.file == nil) and m or { m })
  end
  return tiers
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
    root_tiers = tiers_of(spec.root_markers or { ".git" }),
    prepare = spec.prepare,
    prepared = {},
    preparing = {},
    cmd = type(cmd) == "function" and cmd or function()
      return cmd
    end,
    settings = spec.settings or {},
    init_options = spec.init_options or json.null,
    diagnostics = spec.diagnostics == true,
    max_rss_mb = tonumber(spec.max_rss_mb) or 6144,
    idle_minutes = tonumber(spec.idle_minutes) or 15,
    request_timeout = tonumber(spec.request_timeout) or REQUEST_TIMEOUT,
    max_servers = tonumber(spec.max_servers) or MAX_SERVERS,
    stopped = {},
    rss_busy = {},
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
    kitchen.on("stop", on_stop)
  end
  return s
end

-- For tests and plugins that want the plumbing.
M._line_of, M._to_units, M._from_units, M._uri_of, M._path_of = line_of, to_units, from_units, uri_of, path_of

return M
