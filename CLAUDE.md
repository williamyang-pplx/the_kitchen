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
