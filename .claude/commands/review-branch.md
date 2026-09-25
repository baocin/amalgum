---
description: Review the whole branch as a PR would see it (three-dot diff vs origin/main)
---

Branch-level review, distinct from /review (working tree):

1. `git fetch origin main` so a stale base does not produce phantom findings.
2. Diff with THREE dots: `git diff origin/main...HEAD` (two dots would include other people's work on main).
   Also read untracked files from `git status --porcelain`; they appear in no diff.
3. Run `VERIFY_FULL=1 scripts/agent/verify` — never the scoped gate at branch level.
4. Launch `portability-reviewer`, `safety-reviewer`, and `test-reviewer` on the branch diff in one message.
5. Branch-only checks you do yourself:
   - Secrets anywhere in the branch's history: `scripts/check-secrets origin/main..HEAD` (a secret deleted in
     a later commit is still in the branch).
   - Merge artifacts: conflict markers, a file reverted by a later commit, debug code left mid-branch.
   - Scope creep: commits unrelated to the branch's purpose — name them; they belong in another PR.
   - `docs/STATUS.md` updated if the branch changes what works.
6. Report most severe first. State coverage explicitly: which paths were and were not reviewed.

$ARGUMENTS
