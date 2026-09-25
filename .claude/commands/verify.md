---
description: Run the gate and fix what it reports
---

Run `scripts/agent/verify`. If it fails, fix the first failing leg using the remedy printed on its `✗` line,
then run it again. Repeat until it prints `✓ verify passed`, or stop and report if the failure is outside
what you were asked to change. Report which legs ran and which were skipped.
