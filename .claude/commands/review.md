---
description: Review the working-tree diff with the three read-only reviewers in parallel
---

1. Collect the diff: `git diff HEAD` plus the contents of untracked files from `git status --porcelain`
   (untracked files never appear in a diff; read them).
2. In ONE message, launch the `portability-reviewer`, `safety-reviewer`, and `test-reviewer` subagents on that
   diff. Each is read-only, so running them concurrently is safe.
3. Merge their findings, most severe first, de-duplicating the same file:line. Keep each reviewer's COVERAGE
   line; if any coverage is partial, say so plainly.
4. Do not fix anything unless asked. Findings are proposals; the gate decides what ships.

$ARGUMENTS
