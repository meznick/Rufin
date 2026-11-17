# Delta: Dry Run scope (B-015)

Why: the owner decided (2026-11-17, during the B-015 plan review) that
Dry Run is a development diagnostic, not a product feature — release
builds ship no Dry Run checkbox and no pre-run review dialog; a
production Sync runs on the click. `deltas/b015_plan.md` implements it;
this delta records the spec change it requires.

## MODIFIED

### Requirement: R-04 Sync Operation

#### Scenario: S-08 Dry Run

**Previously:** Given the "Dry Run" checkbox is enabled, When a sync is
started, Then calculate the differences and show them to the user
without changing the device, consuming its playback statistics, or
writing the host `.itl`.

**Now:** Dry Run is available only in development builds
(`cfg!(debug_assertions)`). In dev builds the checkbox and the
pre-run review dialog exist exactly as S-08 describes — the
differences are calculated and shown without changing the device or
the host library. Release builds omit the checkbox and the review
dialog entirely: a Sync click computes the plan at that moment and
runs it, the click being the approval; the engine still re-verifies
the plan hash against the device before any write.

The nonmutation guarantee itself is unchanged — wherever Dry Run
exists, it changes nothing (the B-009 tree-hash proof stands).

Note: production writes remain gated by the `sync-writes` feature
until B-017 hardware acceptance; the release-build silent run is a
refusal until that flips, regardless of this delta.
