# CLAUDE.md

## Workspace hygiene

- **Clean up agent worktrees.** When fanning work out to agents with worktree isolation, remove each worktree once its branch is merged and verified: `git worktree remove --force .claude/worktrees/<name>`, `git branch -D <branch>`, then `git worktree prune`. Never remove a worktree that is locked (a live agent) or has unmerged commits or uncommitted changes.
- **Don't set `CARGO_TARGET_DIR`.** mbx (mr-boxington) owns target storage and the shared build cache (kache), so a manual override would fight it. If a worktree still grows its own `target/` (multiple GB each in practice), plain `cargo` is bypassing mbx: run `mbx doctor`, and `mbx setup` if it warns that the Cargo shim isn't installed. Delete stray `target/` dirs.
