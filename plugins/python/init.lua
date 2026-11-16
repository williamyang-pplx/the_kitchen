-- Python navigation: gd / gr answered by a language server on the file's
-- host. ty's project root follows ty's own rule: the nearest ty.toml, or a
-- pyproject.toml with [tool.ty] (agi: the worktree root), before any other
-- marker. ty is told where sub-projects' src/ dirs are and which environment
-- to use: a venv the kitchen builds from uv.lock, else the nearest .venv.

local lsp = require("kitchen.lsp")

local servers = {
  ty = "ty server",
  basedpyright = "--from basedpyright basedpyright-langserver --stdio",
  pyright = "--from pyright pyright-langserver --stdio",
  pyrefly = "pyrefly lsp",
  jedi = "jedi-language-server",
}

local VENV_KEEP = 3 -- kitchen-built venvs kept per repo and host, newest by use
local VENV_TIMEOUT = 30 * 60 -- s a `uv sync` may take; a lock dir older than this is stale
-- Non-interactive ssh shells have a minimal PATH.
local PATH = 'export PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"; '

local name = kitchen.config.server or "ty"
local args = servers[name]
local custom = kitchen.config.cmd or ""
if custom == "" and not args then
  error("unknown server " .. tostring(name) .. " (ty, basedpyright, pyright, pyrefly, jedi)")
end

local server -- the kitchen.lsp server, set by define below
local building = {} -- host .. venv dir -> true: a build started (or failed) in this load

local function q(s)
  return "'" .. tostring(s):gsub("'", "'\\''") .. "'"
end

-- uvx fetches the server on first use and caches it. A binary in the
-- project's .venv wins.
local function command()
  local env = PATH .. '[ -d .venv ] && export VIRTUAL_ENV="$PWD/.venv"; '
  if custom ~= "" then
    return env .. "exec " .. custom
  end
  local bin = args:match("^%-%-from %S+ (%S+)") or args:match("^(%S+)")
  local rest = args:gsub("^%-%-from %S+ ", ""):gsub("^%S+", "")
  return env .. "if [ -x .venv/bin/" .. bin .. " ]; then exec .venv/bin/" .. bin .. rest
    .. "; elif command -v " .. bin .. " >/dev/null; then exec " .. bin .. rest
    .. "; else exec uvx --quiet " .. args .. "; fi"
end

-- Facts about the project at the root, one per line: sub-projects' src/
-- dirs, [tool.pyright].extraPaths, the uv.lock hash, the repo's name, the
-- nearest .venv, $HOME, and a kitchen-built venv that is ready.
local function discover_script()
  return [=[
git ls-files -z -- '*pyproject.toml' 2>/dev/null | tr '\0' '\n' | while IFS= read -r f; do
  d=$(dirname "$f"); [ -d "$d/src" ] && echo "src $d/src"
done
if [ -f pyproject.toml ]; then
  awk '/^\[/ { sec = $0; grab = 0 }
    sec == "[tool.pyright]" && /^[[:space:]]*extraPaths[[:space:]]*=/ { grab = 1; sub(/^[^=]*=/, "") }
    grab { sub(/#.*/, ""); l = $0
      while (match(l, /"[^"]*"/)) { print "extra " substr(l, RSTART + 1, RLENGTH - 2); l = substr(l, RSTART + RLENGTH) }
      if ($0 ~ /\]/) grab = 0 }' pyproject.toml
fi
lock=
if [ -f uv.lock ]; then
  lock=$( (sha1sum uv.lock 2>/dev/null || shasum uv.lock) | cut -c1-12)
  echo "lock $lock"
fi
repo=$(basename "$(cd "$(git rev-parse --git-common-dir 2>/dev/null || echo .)/.." 2>/dev/null && pwd)")
echo "repo $repo"
d=$PWD
while :; do
  if [ -x "$d/.venv/bin/python" ]; then echo "venv $d/.venv"; break; fi
  if [ "$d" = / ]; then break; fi
  d=$(dirname "$d")
done
echo "home $HOME"
v="$HOME/.cache/kitchen/ty-venvs/$repo-$lock"
if [ -n "$lock" ] && [ -f "$v/.kitchen-ready" ]; then touch "$v"; echo "built $v"; fi
]=]
end

local function parse_facts(out)
  local facts = { src = {}, extra = {} }
  for line in out:gmatch("[^\n]+") do
    local key, value = line:match("^(%S+) (.+)$")
    if key == "src" or key == "extra" then
      table.insert(facts[key], value)
    elseif key then
      facts[key] = value
    end
  end
  return facts
end

local function venv_dir(facts)
  return facts.home .. "/.cache/kitchen/ty-venvs/" .. facts.repo .. "-" .. facts.lock
end

-- Build a venv from uv.lock in the background (once per lock and host per
-- plugin load); when it is ready, the server restarts to use it.
local function build_venv(ctx, facts)
  local dir = venv_dir(facts)
  local key = ctx.host .. "\0" .. dir
  if building[key] then
    return
  end
  building[key] = true
  local base = facts.home .. "/.cache/kitchen/ty-venvs"
  local script = PATH .. string.format([[
base=%s; dir=%s; lock="$dir.lock"
mkdir -p "$base"
if [ -d "$lock" ] && [ -n "$(find "$lock" -maxdepth 0 -mmin +%d 2>/dev/null)" ]; then rmdir "$lock"; fi
mkdir "$lock" 2>/dev/null || exit 3
trap 'rmdir "$lock" 2>/dev/null' EXIT
UV_PROJECT_ENVIRONMENT="$dir" uv sync --frozen --no-install-project --quiet && touch "$dir/.kitchen-ready"
st=$?
ls -dt "$base"/%s-* 2>/dev/null | grep -v '\.lock$' | grep -vxF "$dir" | tail -n +%d | while IFS= read -r old; do rm -rf "$old"; done
exit $st]], q(base), q(dir), VENV_TIMEOUT // 60, q(facts.repo), VENV_KEEP)
  kitchen.log(string.format("python: building the environment for %s in %s", facts.repo, dir))
  kitchen.run({ sh = script, host = ctx.host, cwd = ctx.root, timeout = VENV_TIMEOUT }, function(res)
    if res.code == 3 then
      return -- another build of this lock runs
    end
    -- 0 only once .kitchen-ready is written.
    if res.code == 0 then
      kitchen.log("python: environment ready in " .. dir)
      kitchen.notify(string.format("python: environment ready for %s; restarting %s", facts.repo, name))
      lsp.restart(server, ctx.host, ctx.root)
      return
    end
    kitchen.log(string.format("python: building %s failed (code %s): %s", dir, tostring(res.code), (res.stderr or ""):sub(-2000)))
    kitchen.notify(string.format("python: couldn't build the environment for %s; using %s — see `kitchenctl plugin logs python`",
      facts.repo, facts.venv or "no venv"), "error")
  end)
end

local function ty_settings(roots, python)
  local cfg = { environment = { root = roots, python = python } }
  return { settings = { ty = { configuration = cfg } }, init_options = { configuration = cfg } }
end

-- Per root, before ty starts: where first-party code lives (the root, every
-- sub-project's src/, pyright's extraPaths) and which environment to use.
local function prepare(ctx, done)
  if name ~= "ty" or custom ~= "" then
    return done({})
  end
  kitchen.run({ sh = discover_script(), host = ctx.host, cwd = ctx.root, timeout = 20 }, function(res)
    if res.code ~= 0 then
      kitchen.log("python: looking at the project failed: " .. (res.stderr or ""))
      return done(ty_settings({ ctx.root }, nil))
    end
    local facts = parse_facts(res.stdout)
    local roots, seen = {}, {}
    local function add(path)
      local abs = path:sub(1, 1) == "/" and path or (ctx.root .. "/" .. path:gsub("^%./", ""))
      abs = abs:gsub("/%.$", ""):gsub("/$", "")
      if not seen[abs] then
        seen[abs] = true
        table.insert(roots, abs)
      end
    end
    add(ctx.root)
    for _, p in ipairs(facts.src) do
      add(p)
    end
    for _, p in ipairs(facts.extra) do
      add(p)
    end
    local python = facts.built or facts.venv
    if not facts.built and facts.lock and facts.home and facts.repo and kitchen.config.build_venv ~= false then
      build_venv(ctx, facts)
    end
    kitchen.log(string.format("python: %s: %d import root(s), environment %s", ctx.root, #roots, python or "none"))
    done(ty_settings(roots, python))
  end)
end

server = lsp.define {
  name = custom ~= "" and "python-lsp" or name,
  filetypes = { "py", "pyi" },
  language_id = "python",
  -- ty's rule first (its project), then the markers of any Python project.
  root_markers = {
    { "ty.toml", { file = "pyproject.toml", contains = "[tool.ty" } },
    { "pyproject.toml", "setup.py", "setup.cfg", "pyrightconfig.json" },
  },
  cmd = command,
  prepare = prepare,
  diagnostics = kitchen.config.diagnostics == true,
  max_rss_mb = kitchen.config.max_rss_mb,
  idle_minutes = kitchen.config.idle_minutes,
  -- pyright-family servers ask for these; others ignore them.
  settings = {
    python = { analysis = { autoSearchPaths = true, diagnosticMode = "openFilesOnly" } },
    basedpyright = { analysis = { diagnosticMode = "openFilesOnly" } },
  },
}
