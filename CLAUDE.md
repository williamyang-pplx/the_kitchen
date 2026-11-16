# the kitchen: notes for agents

## Git workflow: branch first, then merge into main

Several agents may work in this repo at the same time. Don't commit
straight to `main`:

1. Before you change anything, create a branch from an up-to-date `main`,
   e.g. `git fetch origin && git switch -c <topic> origin/main`. Use a
   separate worktree if another agent is using this checkout.
2. Commit your work on that branch.
3. When it's done and tested, merge it into `main`. First bring in what
   landed meanwhile (`git fetch origin && git rebase origin/main`, or merge
   `origin/main` into the branch), resolve conflicts, and re-run
   `cargo test --workspace`.
4. Push `main` (or the branch, if asked) only when William asks.

Never reset, force-push or rewrite `main`, and never discard uncommitted
changes you didn't make: they may belong to another agent.

## Testing UI changes: run the kitchen hidden

To check a UI change, start a test kitchen built from your checkout with
`KITCHEN_HIDDEN=1`. Drive it with `kitchenctl` and look at
`kitchenctl debug screenshot` PNGs. William sees nothing. Give it its own
`KITCHEN_STATE_DIR` and `KITCHEN_SOCKET_PATH` and a throwaway clone, or you
drive his live kitchen and make worktrees in his checkout. See
[docs/testing.md](docs/testing.md) for the rules and a recipe.
