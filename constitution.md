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
- **iPod Backend**: libgpod (C, LGPL-2.1-or-later) is required for the device-side database and artwork layer, behind a small safe Rust FFI wrapper. Rufin owns sync policy, deltas, progress, recovery, and host-library updates through `itl-sync`. Copying files alone is not a sync backend.
- Discovery starts with mounted volumes; missing device capabilities may require USB/SCSI helpers and additional permissions. Do not assume a mount point supplies every prerequisite for writing. Link-time versus runtime loading remains undecided; builds without the backend must remain possible, but iPod sync must report unavailable rather than silently fall back to file copying. The supported non-iOS devices do not require usbmuxd or an iOS Trust flow.
- Device writes require application-level multi-file safety: backups, write ordering, interrupted-sync recovery, exclusion of competing writers, and safe flushing/ejection. libgpod's start/stop-sync calls provide no transaction or crash protection on regular iPods. Persist pending imported statistics before a device write can consume them; retries must not lose or double-import plays/skips. libgpod must never write the host `.itl`.

## 4. Testing
- Tests run against env-gated snapshot corpora (ITL_TEST_PATH pattern); no test may touch production data.
- Features that write to a device or to the .itl format require VM/real-device acceptance before merging to the mainline.
- A task branch merges only with its verification criteria passing.

## 5. Docs
- Format research stays in the companion `itl-player` repository's `NOTES.md`; this fork has no local `NOTES.md`. SDD artifacts live in the repo root; feature research and decisions live with their delta documents and must use repository-relative links, not session-local plan paths.
- Language: English.

## 6. Ownership
- Architecture and format decisions are owner calls, recorded in the owning repository's documentation (format decisions in `itl-player/NOTES.md` §11; Rufin feature decisions with the feature artifacts).
- Spec/plan/tasks changes go through the SDD approval gates.
