-- Python navigation: gd / gr answered by a language server running in the
-- project root (nearest pyproject.toml / setup.py) on the file's host.
-- The project's .venv is used when there is one.

local servers = {
  ty = "ty server",
  basedpyright = "--from basedpyright basedpyright-langserver --stdio",
  pyright = "--from pyright pyright-langserver --stdio",
  pyrefly = "pyrefly lsp",
  jedi = "jedi-language-server",
}

local name = kitchen.config.server or "ty"
local args = servers[name]
local custom = kitchen.config.cmd or ""
if custom == "" and not args then
  error("unknown server " .. tostring(name) .. " (ty, basedpyright, pyright, pyrefly, jedi)")
end

-- Non-interactive ssh shells have a minimal PATH; uvx fetches the server
-- on first use and caches it. A binary in the project's .venv wins.
local function command()
  local env = 'export PATH="$HOME/.local/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"; '
    .. '[ -d .venv ] && export VIRTUAL_ENV="$PWD/.venv"; '
  if custom ~= "" then
    return env .. "exec " .. custom
  end
  local bin = args:match("^%-%-from %S+ (%S+)") or args:match("^(%S+)")
  local rest = args:gsub("^%-%-from %S+ ", ""):gsub("^%S+", "")
  return env .. "if [ -x .venv/bin/" .. bin .. " ]; then exec .venv/bin/" .. bin .. rest
    .. "; elif command -v " .. bin .. " >/dev/null; then exec " .. bin .. rest
    .. "; else exec uvx --quiet " .. args .. "; fi"
end

require("kitchen.lsp").define {
  name = custom ~= "" and "python-lsp" or name,
  filetypes = { "py", "pyi" },
  language_id = "python",
  root_markers = { "pyproject.toml", "setup.py", "setup.cfg", "pyrightconfig.json", "ty.toml" },
  cmd = command,
  diagnostics = kitchen.config.diagnostics == true,
  -- pyright-family servers ask for these; others ignore them.
  settings = {
    python = { analysis = { autoSearchPaths = true, diagnosticMode = "openFilesOnly" } },
    basedpyright = { analysis = { diagnosticMode = "openFilesOnly" } },
  },
}
