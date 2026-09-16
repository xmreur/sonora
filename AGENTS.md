# Agent instructions

## Branches — REQUIRED

NEVER edit directly on `main`. ALL edits happen on a typed branch:

- Bug fix: `fix/<short-topic>` — e.g. `fix/seek-snap-back`, `fix/mut-cache-race`
- New feature: `feat/<short-topic>` — e.g. `feat/mpris-support`, `feat/shuffle-queue`
- Refactor: `refactor/<short-topic>` — e.g. `refactor/sidecar-polling`

Rules:

- Create the branch BEFORE the first edit (`git checkout -b <type>/<topic>`).
- One branch = one change. Unrelated fix spotted mid-work → new branch, not scope creep.
- kebab-case topic, 2–4 words. No bare names (`fix/stuff`), no ticket numbers without context.
- Already on `main` with uncommitted edits? `git stash -u`, create branch, `git stash pop`.
- Open the PR from that branch against `main`; branch name MUST match its content at merge time.
