# Delta: iPod page access point (B-012)

Why: the owner moved the iPod page entry point out of the sidebar — the
sidebar pages lead to different library sorts, while the iPod page is a
device page. The natural home is the top bar, next to the playback-output
("this device") button. `deltas/b012_plan.md` §4-6 implements it; this
delta records the spec change it requires.

## MODIFIED

### Requirement: R-01 Page Visibility

**Previously:** the iPod page is accessible in the sidebar.

**Now:** the iPod page is accessible from an "iPod" button in the top bar
— left-aligned in the header bar's start group, next to the playback
output ("this device") button, before the search field. The button is
always visible regardless of the configured library sources.

#### Scenario: the page opens from the top bar with no device connected

- Given the app is running and no iPod is connected
- When the user clicks the iPod button in the top bar
- Then the iPod page opens
- And it shows the "No iPod detected" view with a status indicator

S-01 itself is unchanged (it is access-mechanism-agnostic — "when the
page is accessed").
