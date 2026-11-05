# constitution.md

## 1. Repository and branches
- The project's home is the Rufin fork. `fork-master` is the project mainline (plays the SDD role of \"main\"); `main` only mirrors upstream and carries no project-only commits.
- Feature work follows SDD topology: `feature/ipod-page` is the base feature branch.
- Task branches are named `feature/ipod-page-task-B-XXX` (flat naming, no nested directory structure).
- constitution.md and spec.md are edited on the mainline only; constitution changes require explicit owner approval.
- **Exception for First Feature**: Initial bootstrap of `constitution.md`, `spec.md`, and resetting `plan.md`/`tasks.md` to templates on `fork-master` are permitted as a one-time setup.
- History is a log, never a draft: no amending or rebasing of merged commits — hashes referenced in tasks.md must stay valid. Post-merge fixes go through `fix-` branches.

## 2. Data safety (.itl)
- The production `iTunes Library.itl` is a live, iTunes-owned file. Every write to it goes through itl-sync's single atomic write path with rolling backups.
- The sentinel rule is non-negotiable: zeroed sentinel = library open in iTunes = refuse the write. No `--force` bypass. The /proc handle scan stays as the second layer.
- Format experiments and sync-only work happen only on copies; the iTunesACCEPT corpus is never written to.
- The .itl is the only source of truth; `iTunes Library.xml` is an export and is never read back as data.

## 3. Dependencies
- Third-party code that needs patching gets vendored (itl-rs precedent).
- **Project Dependencies**: `libpod`, `libusb`. System libraries (e.g. usbmuxd) must not break builds when absent — feature-gate or fallback.

## 4. Testing
- Tests run against env-gated snapshot corpora (ITL_TEST_PATH pattern); no test may touch production data.
- Features that write to a device or to the .itl format require VM/real-device acceptance before merging to the mainline.
- A task branch merges only with its verification criteria passing.

## 5. Docs
- NOTES.md stays the format knowledge store; SDD artifacts live in the repo root.
- Language: English.

## 6. Ownership
- Architecture and format decisions are owner calls, recorded in NOTES.md §11.
- Spec/plan/tasks changes go through the SDD approval gates.