# kitchenctl — agent guide

You are running inside **the kitchen**, William's cmux-style workspace manager.
`kitchenctl` does from the shell everything William does from the keyboard.
It talks to the kitchen app over `~/.kitchen/kitchen.sock`; on a devbox that
socket is a relay the app keeps open over ssh, so the same commands work there.

## Where am I?

```bash
kitchenctl identify            # my workspace, tab, worktree, branch, plan file
kitchenctl --json identify     # same, as JSON
```

Environment in every kitchen tab: `KITCHEN_WORKSPACE_ID`, `KITCHEN_TAB_ID`,
`KITCHEN_PLAN_FILE`, `KITCHEN_DIFF_BASE`. Commands default to *your*
workspace and tab.

## Model

- **Workspace** — one repo on this mac or on a devbox (sidebar).
- **Tab** — its own git worktree `<repo>-<slug>`, on a new branch
  `william/<slug>` cut from freshly fetched `origin/<default>`, or on an
  existing branch. One PR per tab.
- Every tab has `$KITCHEN_DIFF_BASE` (`origin/<default>`): the tab's changes
  are `git diff $(git merge-base HEAD $KITCHEN_DIFF_BASE)`.
- William reviews a PR by pasting its link into ⌘⇧P (its head is checked out
  in `<repo>-kitchen-pr-<N>`); his comments (⌘M) are posted to GitHub
  as PR review comments. To address them, read them with
  `gh api repos/<owner>/<repo>/pulls/<N>/comments --paginate` (`path`,
  `line`, `side`, `body`, `in_reply_to_id` for replies).
- Handles: ids, 1-based indexes (`2`, `workspace:2`, `tab:3`), or names.

## Common tasks

```bash
kitchenctl workspace list
kitchenctl tab new --task "AI-123: add retries to the judge client"
kitchenctl tab new --branch william/x        # continue an existing branch
kitchenctl tab send --tab 2 --enter "make test"
kitchenctl tab read-screen --tab 2 --scrollback 200
kitchenctl tab send-key --tab 2 C-c
kitchenctl viewer open src/lib.rs --line 42 --diff
kitchenctl notify --title "tests green" --body "PR ready for review"
kitchenctl worktree list
```

## Messaging other tabs

A prompt that mentions a tab ("tab 2", "agi tab 3") comes with a
`<kitchen-context>` note saying which tab that is and how to reach its agent:
SendMessage to the ListAgents peer with the same tmux session. For a tab
ListAgents doesn't show (it runs on another machine), or to find a tab
yourself:

```bash
kitchenctl --json tab list                       # index, id, tmux_session per tab
kitchenctl tab send -t <tab id> --enter "<text>"
```

## Planning phase

Every PR starts with a plan. When you were started with a task:

1. Produce the plan with the `low-level-plan` skill. Write the full plan as
   markdown to `$KITCHEN_PLAN_FILE` (`.kitchen/plan.md` in the worktree; the
   `.kitchen/` directory is git-ignored).
2. Run `kitchenctl plan ready`. William reviews it in the plan viewer and
   leaves inline comments.
3. When comments arrive: `kitchenctl plan comments` lists them with id, line,
   quoted text and body. Revise the plan file (keep item IDs stable), then
   `kitchenctl plan resolve <id>` for each comment you addressed, then
   `kitchenctl plan ready` again.
4. Do not implement until William approves the plan.

## Plugins and viewer diagnostics

Plugins are Lua in `~/.kitchen/plugins/<name>/` (`plugin.toml` + `init.lua`;
full reference: `docs/plugins.md` in the kitchen repo). They reload when
their files change.

```
kitchenctl plugin list                      # status + commands
kitchenctl plugin run ruff.explain          # run a plugin command in the caller's tab
kitchenctl plugin logs ruff                 # print()/errors of a plugin
kitchenctl plugin timers                    # kitchen.every timers: runs, last fired, next due, stats
kitchenctl plugin reload [name]             # this mac only
```

Any program can push diagnostics into the viewer without writing a plugin
(namespace becomes `ext.<ns>`; host/worktree default to the calling tab):

```
kitchenctl decor set app/main.py --ns mytool --items '[{"line":3,"col":5,"severity":"error","message":"boom"}]'
kitchenctl decor clear app/main.py --ns mytool
```

Lines and columns are 1-based, columns in characters of the raw line,
`end_col` exclusive.

In the viewer, `gd` / `gr` (definition / references) come from a plugin's
language server when one covers the file (the bundled `python` plugin runs ty
in the project root on the file's host) and fall back to text search.

## Output and errors

- `--json` gives machine-readable output for every command.
- Exit status: 0 ok, 1 the kitchen refused the request (message on stderr),
  2 usage error, 3 the kitchen is not reachable.
- `kitchenctl rpc <method> '<json>'` calls any API method directly.
- `kitchenctl <command> --help` documents every flag, with examples.
