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
  synced to the latest `origin/<default>`. Files are not writable. Use it for
  code understanding and review only; never try to make it writable.
- Handles: ids, 1-based indexes (`2`, `workspace:2`, `tab:3`), or names.

## Common tasks

```bash
kitchenctl workspace list
kitchenctl tab new --edit --task "AI-123: add retries to the judge client"
kitchenctl tab new --edit --pr 1234          # work on an existing PR
kitchenctl tab new --readonly                # latest main, read-only
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

## PR review comments on your PR

```bash
kitchenctl pr threads --unresolved           # William's and others' threads
kitchenctl pr resolve <thread-id> --reply "Fixed in 3f2a1c: ..."
```

Resolve a thread as soon as you have addressed it (pushed the fix), so the
kitchen and GitHub show it as done. Only reply publicly where the
`address-pr-comments` skill allows it.

## Reviewing other people's PRs (auto-review runs)

When the kitchen starts you to review a PR, record each finding as a
**draft** — drafts are private and are published only after William approves
each one. Never post review comments to GitHub yourself (`gh pr review`,
`gh api .../comments`): that bypasses his approval.

```bash
kitchenctl review draft --run 7 --path src/a.rs --line 42 --severity bug \
  --body "This unwrap panics when the list is empty; use .first()?"
kitchenctl review draft --run 7 --path src/a.rs --body "File-level note"
kitchenctl review draft --run 7 --path src/a.rs --line 10 --side LEFT --body "..."   # deleted line
kitchenctl review done --run 7               # or: --failed "reason"
kitchenctl review drafts                     # what you have drafted
```

Lines are line numbers in the new file (`--side RIGHT`, the default) or in the
old file for deleted code (`--side LEFT`). Use `--start-line` for ranges.

## Output and errors

- `--json` gives machine-readable output for every command.
- Exit status: 0 ok, 1 the kitchen refused the request (message on stderr),
  2 usage error, 3 the kitchen is not reachable.
- `kitchenctl rpc <method> '<json>'` calls any API method directly.
- `kitchenctl <command> --help` documents every flag, with examples.
