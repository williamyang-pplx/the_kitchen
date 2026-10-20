-- download: pull files from a devbox down to this mac.
--
-- A session on the devbox copies what it wants into ~/<outbox>/ and runs
-- `kitchenctl plugin run download.pull`; that runs here, on the mac, and
-- rsyncs the outbox into <dest>/<host>/, removing each file from the devbox
-- once it has arrived. So the devbox side knows a file made it when it's gone
-- from the outbox, and a failed pull leaves it there for the next one.

local M = {}

local SSH = "ssh -o BatchMode=yes -o ConnectTimeout=15"

local function q(s)
  return "'" .. tostring(s):gsub("'", "'\\''") .. "'"
end

-- A config path → a shell word: a leading ~ becomes "$HOME", the rest is
-- double-quoted with `$`, `"`, `\` and backquotes escaped.
function M.shell_path(p)
  local home, rest = "", p
  if p == "~" or p:sub(1, 2) == "~/" then
    home, rest = "$HOME", p:sub(2)
  end
  return '"' .. home .. rest:gsub('[%$"\\`]', "\\%0") .. '"'
end

-- Shell, run on the mac, that pulls host:<outbox>/ into <dest>/<host>/ and
-- then drops the outbox's emptied folders on the devbox.
function M.pull_sh(host, dest, outbox)
  local into = M.shell_path(dest .. "/" .. host)
  return table.concat({
    "set -e",
    "mkdir -p " .. into,
    ("rsync -av --remove-source-files -e %s %s %s/"):format(q(SSH), q(host .. ":" .. outbox .. "/"), into),
    ("%s %s %s"):format(SSH, q(host), q("find ~/" .. q(outbox) .. " -mindepth 1 -type d -empty -delete")),
  }, "\n")
end

-- The files rsync -v listed: the lines between "Transfer starting" and the
-- blank line before its totals, minus folders (they end in /).
function M.pulled(stdout)
  local files, listing = {}, false
  for line in (stdout .. "\n"):gmatch("([^\n]*)\n") do
    if line:match("^Transfer starting") then
      listing = true
    elseif listing and line == "" then
      break
    elseif listing and line:sub(-1) ~= "/" then
      table.insert(files, line)
    end
  end
  return files
end

local busy = {}

kitchen.command("pull", function(ctx)
  local host, cfg = ctx.host, kitchen.config
  if not host or host == "local" then
    kitchen.notify("download: this tab is already on the mac", "warn")
    return
  end
  if busy[host] then
    kitchen.notify("download: already pulling from " .. host, "warn")
    return
  end
  busy[host] = true
  local into = cfg.dest .. "/" .. host
  kitchen.run({ sh = M.pull_sh(host, cfg.dest, cfg.outbox), timeout = cfg.timeout }, function(res)
    busy[host] = nil
    if res.code ~= 0 then
      kitchen.log(res.stderr)
      kitchen.notify(("download from %s failed: %s"):format(host, res.timed_out and "timed out" or res.stderr), "error")
      return
    end
    local files = M.pulled(res.stdout)
    if #files == 0 then
      kitchen.notify(("download: %s's ~/%s is empty"):format(host, cfg.outbox), "warn")
      return
    end
    local shown = #files == 1 and (into .. "/" .. files[1]) or into
    kitchen.notify(#files == 1 and ("downloaded " .. shown) or ("downloaded %d files to %s"):format(#files, into))
    if cfg.reveal then
      kitchen.run({ sh = (#files == 1 and "open -R " or "open ") .. M.shell_path(shown) })
    end
  end)
end, { desc = "pull the devbox's ~/outbox down to ~/Downloads/devbox/<host>", remote = true })

return M
