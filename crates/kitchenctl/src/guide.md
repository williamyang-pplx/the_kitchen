# kitchenctl — agent guide

You are running inside **the kitchen**, William's cmux-style workspace manager.
`kitchenctl` does from the shell everything William does from the keyboard.
It talks to the kitchen app over `~/.kitchen/kitchen.sock`; on a devbox that
socket is a relay the app keeps open over ssh, so the same commands work there.

## Where am I?

```bash
kitchenctl identify            # my workspace, tab, worktree, branch, plan file
kitchenctl --json identify     # same, as JSON
echo "$KITCHEN_TAB_KIND"       # edit | readonly
```

Environment in every kitchen tab: `KITCHEN_WORKSPACE_ID`, `KITCHEN_TAB_ID`,
`KITCHEN_TAB_KIND`, `KITCHEN_PLAN_FILE` (edit tabs), `KITCHEN_READONLY=1`
(read-only tabs). Commands default to *your* workspace and tab.

## Model

- **Workspace** — one repo on this mac or on a devbox (sidebar).
- **Edit tab** — its own git worktree `<repo>-<slug>` on branch
  `william/<slug>`, cut from freshly fetched `origin/<default>`. One PR per tab.
- **Read-only tab** — the shared read-only worktree `<repo>-kitchen-readonly`,
  synced to the latest `origin/<default>`, or (opened from a PR link) the PR
  head in `<repo>-kitchen-ro-pr-<N>`. Files are not writable. Use it for
  code understanding and review only; never try to make it writable.
- Every tab has `$KITCHEN_DIFF_BASE`: the tab's changes (an edit tab's work,
  or a PR's diff) are `git diff $(git merge-base HEAD $KITCHEN_DIFF_BASE)`.
- William's review comments on a PR (⌘M in the viewer) are posted to GitHub
  as PR review comments. To address them, read them with
  `gh api repos/<owner>/<repo>/pulls/<N>/comments --paginate` (`path`,
  `line`, `side`, `body`, `in_reply_to_id` for replies).
- Handles: ids, 1-based indexes (`2`, `workspace:2`, `tab:3`), or names.

## Common tasks

```bash
kitchenctl workspace list
kitchenctl tab new --edit --task "AI-123: add retries to the judge client"
kitchenctl tab new --edit --branch william/x  # continue an existing branch
kitchenctl tab new --readonly                # latest main, read-only
kitchenctl tab open https://github.com/ppl-ai/air/pull/14211   # review a PR
kitchenctl tab send --tab 2 --enter "make test"
kitchenctl tab read-screen --tab 2 --scrollback 200
kitchenctl tab send-key --tab 2 C-c
kitchenctl viewer open src/lib.rs --line 42 --diff
kitchenctl notify --title "tests green" --body "PR ready for review"
kitchenctl worktree list
```

## Planning phase (edit tabs)

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
