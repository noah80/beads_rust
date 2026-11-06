# VCS Integration (Git Baseline)

br never automatically runs version-control commands — all storage lives inside
`.beads/`. User-invoked reporting commands (`br changelog`, `br orphans`, and
commit-activity `br stats`) read git history when explicitly called but never
write to or modify the repository. This document covers the **git baseline**
workflow plus best-effort command equivalents for other VCS (hg, jj, Perforce,
Sapling). Verify details with your VCS docs if your environment differs.

## Core Principles

- **br never runs git automatically** — VCS operations are explicit and manual.
  Reporting commands (`changelog`, `orphans`, `stats`) read git history only
  when you invoke them.
- **Track `.beads/` in version control** — it is the collaboration surface.
- **Local-only artifacts stay local** — `.beads/.gitignore` excludes dbs, locks,
  and machine-specific files.

## Git Baseline Workflow

### Export before commit

```bash
br sync --flush-only
git add .beads/
git commit -m "Update issues"
```

### Pull and import after sync

```bash
git pull --rebase
br sync --import-only
```

## Common Git Commands

| Task | Git command |
|------|-------------|
| Stage `.beads/` changes | `git add .beads/` |
| Check status | `git status .beads/` |
| Inspect JSONL diff | `git diff .beads/issues.jsonl` |
| Resolve JSONL conflicts | `git add .beads/issues.jsonl` (after manual edit), then `br sync --import-only` |

## Conflict Handling (JSONL)

JSONL is line-based, so conflicts are typically straightforward:

```bash
git status .beads/issues.jsonl
vim .beads/issues.jsonl   # resolve per-line conflicts
git add .beads/issues.jsonl
br sync --import-only
```

For more detail, see the README FAQ on JSONL conflicts.

## Per-Issue Tree (Conflict-Free Merges)

A single sorted JSONL makes git report a conflict whenever two branches edit
or insert *neighbouring* lines, even when they touch different issues. Hosted
merges (GitHub, GitLab) ignore custom merge drivers, so stacked pull requests
hit this constantly. The opt-in per-issue tree avoids it: git tracks one file
per issue instead of the JSONL.

```
.beads/issues/beads_rust-abc12.jsonl   # exactly that issue's export line
.beads/issues/beads_rust-def34.jsonl
```

`issues.jsonl` stays the local file every sync safety mechanism works on, but
becomes untracked. br keeps the two in step:

- **after a command publishes the JSONL** (auto-flush, `br sync --flush-only`),
  it writes the changed rows into the tree and removes files of issues that
  left the export;
- **before a command reads issues** (and on `br sync`), when git changed the
  tree (pull, merge, checkout), it rebuilds `issues.jsonl` from the tree, and
  the normal staleness probe imports it as if a pulled JSONL had arrived.

New issues become new files and edits to different issues touch different
files, so only two branches changing the *same* issue still conflict. Resolve
such a conflict in that one file (keep one complete JSON line); br refuses to
rebuild from a file that still holds conflict markers.

### Enabling

```bash
# 1. Name the tree directory in .beads/metadata.json:
#    { "database": "beads.db", "jsonl_export": "issues.jsonl", "issue_tree": "issues" }
# 2. Ignore the local JSONL, its workflow-history companion, and the tree
#    bookkeeping:
printf 'issues.jsonl\nissues.history.jsonl\n.issue-tree-state.json\n.issue-tree.lock\n' >> .beads/.gitignore
# 3. Stop tracking both files (they stay on disk) and create the tree:
git rm --cached .beads/issues.jsonl .beads/issues.history.jsonl
br sync --flush-only        # any br command that reads issues also works
git add .beads/
git commit -m "beads: track issues as a per-issue tree"
```

The workflow-history companion (`issues.history.jsonl`, see
[SYNC_SAFETY.md](SYNC_SAFETY.md#portable-workflow-history)) is one file
rewritten on every export, so leaving it tracked would bring back a conflict
on nearly every merge. With the tree it stays local: issue rows (status,
labels, dependencies, comments, close reason) travel through the tree, but
audit events, gate verdicts, close metadata and capacity evidence stay in the
clone that recorded them. An explicit import of a peer's row then records a
coverage warning rather than fabricating that evidence.

Every clone needs a br build with this feature; an older br ignores
`issue_tree` and keeps working on its local JSONL without updating the tree.
A fresh clone has no `issues.jsonl`: the first br command rebuilds it from the
tree, and `br sync --import-only` then builds the database from it, as in any
fresh clone.

### When both sides changed

br records the state of both sides after each reconciliation in
`.beads/.issue-tree-state.json`. If the JSONL and the tree both changed since
then and hold different issues (for example, br was interrupted after an export
and a `git pull` followed), br changes neither and refuses with a sync
conflict. To keep the tree, move `.beads/issues.jsonl` aside and rerun; to keep
the JSONL, move the tree directory aside and rerun.

The MCP server (`br serve`) does not update the tree itself; the next CLI
command writes any export it published.

## Ignore Files (Equivalents)

These are the usual ignore mechanisms for each VCS. Verify in your environment.

| VCS | Ignore file |
|-----|-------------|
| Git | `.gitignore` |
| Mercurial | `.hgignore` |
| Jujutsu | `.gitignore` (git-compatible working copy) |
| Perforce | `.p4ignore` (or `P4IGNORE` env) |
| Sapling | `.gitignore` / `.hgignore` (Sapling honors git/hg ignore files) |

---

## Mercurial (hg)

| Task | Command |
|------|---------|
| Stage `.beads/` changes | `hg add .beads/` |
| Check status | `hg status .beads/` |
| Inspect JSONL diff | `hg diff .beads/issues.jsonl` |
| Resolve JSONL conflicts | edit file -> `hg resolve -m .beads/issues.jsonl` |

## Jujutsu (jj)

jj is snapshot-based; there is no explicit "add" step for tracked files.

| Task | Command |
|------|---------|
| Check status | `jj status .beads/` |
| Inspect JSONL diff | `jj diff .beads/issues.jsonl` |
| Record change | `jj commit -m "Update issues"` |

## Perforce (p4)

| Task | Command |
|------|---------|
| Open/add/edit `.beads/` | `p4 reconcile //.../.beads/...` |
| Check status | `p4 status //.../.beads/...` |
| Inspect JSONL diff | `p4 diff //.../.beads/issues.jsonl` |
| Resolve JSONL conflicts | edit file -> `p4 resolve //.../.beads/issues.jsonl` |

## Sapling (sl)

Sapling uses hg-like commands via the `sl` CLI.

| Task | Command |
|------|---------|
| Stage `.beads/` changes | `sl add .beads/` |
| Check status | `sl status .beads/` |
| Inspect JSONL diff | `sl diff .beads/issues.jsonl` |
| Resolve JSONL conflicts | edit file -> `sl resolve -m .beads/issues.jsonl` |
