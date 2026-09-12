---
name: maintainer-sweep
description: "Run Kaku maintainer follow-up for live GitHub issues and pull requests: triage open items, connect them to code or commits, verify fixes, push main safely, wait for GitHub Actions, then post concise replies and close items when appropriate."
when_to_use: "latest issues, latest PRs, triage issues, triage PRs, close issue, reply issue, reply PR, GitHub follow-up, push then close, maintainer sweep, 看看最新 issue, 看看最新 PR, 回复 issue, 关闭 issue, 处理 PR"
---

# Kaku Maintainer Sweep

Use this skill for maintenance work that spans GitHub issues, pull requests, local fixes, `main` CI, and public follow-up. It is not the notarized release flow, and it must not contain private signing credentials or local machine setup.

## Workflow

1. **Refresh live state**
   - Run `gh issue list --state open --limit 20` and `gh pr list --state open --limit 20`.
   - For each actionable item, read `gh issue view <n>` or `gh pr view <n>` and confirm title, author, state, body, and recent comments.
   - If the task asks for the latest state, refresh the lists again before final decisions.

2. **Map reports to code**
   - Find the latest release tag with `git tag --list 'V*' --sort=-version:refname | head -1`; without the `V*` filter the rolling `nightly` tag sorts first.
   - Compare relevant changes with `git log <tag>..HEAD --oneline`, `git show`, and targeted `rg`.
   - Do not treat a closed issue as proof. Identify the fix mechanism, the commit, or the remaining gap.

3. **Fix and verify**
   - Keep fixes scoped. For unrelated issues, prefer one commit per issue or behavior.
   - For shell integration changes, run the affected `assets/shell-integration/tests/*_smoke.sh` script, then the full list that `.github/workflows/checks.yml` runs under `Shell integration smoke tests` when feasible.
   - For AI provider, transport, or config changes, run targeted Cargo tests before repository-level checks.
   - For release-adjacent work, prefer `git diff --check`, `make fmt-check`, `make check`, `make test`, and `make app` unless the maintainer narrows the gate.

4. **Push safely**
   - Run `git fetch origin main`, then verify `origin/main` still matches the expected base before `git push origin main`.
   - If `origin/main` moved, stop and review `origin/main..HEAD`.
   - After pushing, locate the new run with `gh run list --branch main --limit 5` and watch it with `gh run watch <id> --exit-status`.

5. **Follow up publicly**
   - Post fixed/closed replies only after the relevant GitHub Actions run on `main` is green.
   - Confirm each item identity again before posting.
   - Match the reporter's language, including Japanese and Korean, unless the maintainer explicitly requests another language.
   - Start with `@login`, one short thanks, the concrete fix or reason, and the next release, nightly, or verification step.
   - Use Nightly as a test path only after verifying it was rebuilt for the fix. `nightly` is a rolling GitHub prerelease produced by `./scripts/nightly.sh`; a push to `main` alone does not refresh the DMG.
   - For merged contributor PRs, leave at most one short thanks comment after merge or Nightly availability, and avoid duplicating bot or deployment noise.
   - If the current request authorizes closure, complete the root `AGENTS.md` closure pipeline and close with `--reason completed`; otherwise propose closure and wait for authorization.
   - Close PRs without merging only when the fix is already covered on `main`, the direction is no longer needed, the patch is unsafe, the work is duplicate, or the maintainer explicitly rejects it.
   - If an accepted contributor fix lands through a maintainer commit, acknowledge the contribution and explain the delivered fix in the PR comment without commit hashes or internal attribution bookkeeping.

## Final Report

Report the pushed branch and commit, the CI run URL and conclusion, any verified Nightly URL when used, each issue/PR decision, and whether any open issues or PRs remain.
