# Changelog fragments

Every PR with a user-visible change adds one file here, named after its branch
(`changes/<branch>.md`), instead of editing `CHANGELOG.md`. Parallel PRs then
never touch the same lines.

A fragment holds [Keep a Changelog](https://keepachangelog.com/en/1.1.0/)
sections (`Added`, `Changed`, `Deprecated`, `Removed`, `Fixed`, `Security`):

```markdown
### Fixed
- **Short title (#12):** what changed, in words a player understands.
```

`scripts/release.py check` validates them (CI runs it); `scripts/release.py
notes` prints what the next release will say. The release PR folds every
fragment into `CHANGELOG.md` and deletes them (docs/PACKAGING.md, "Releases").
