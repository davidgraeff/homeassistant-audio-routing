# Components

One directory per feature, plus `ui/` for the pieces every feature uses. The
daemon's `src/` is organised the same way and for the same reason — the directory
should say what a file is for before you open it (see
`docs/module-layout-plan.md`).

| directory | what lives there |
|---|---|
| `align/` | Latency alignment: the page, the wizard and its steps, and the small widgets a run needs (checks, refusals, the run log, the hold timer, the signal verdict, the mic capture). By far the largest feature — 18 of 38 components. |
| `outputs/` | The Outputs page and its two help panels. |
| `sources/` | The Sources page and the RTP-sender firmware docs. |
| `groups/` | Music and announcement groups: both pages, their docs, and the two widgets they share (`GroupTitle`, `DeviceChip`). |
| `routing/` | The routing matrix graph and its help panel. |
| `system/` | Settings and Diagnostics — the two pages that are about the installation rather than about audio. |
| `ui/` | Cross-feature pieces with no domain of their own: `ConfirmDialog`, `Toasts`, `ThemeToggle`, `DelaySlider`, `VolumeControl`. |

## The two rules that keep this meaningful

**`ui/` is a leaf.** It imports from `../../lib/` and nothing else. A component
that needs to know what an output or a group *is* belongs in a feature directory,
not here — that is the test for whether something is a UI primitive.

**A page (`*Tab.svelte`) is mounted only by `App.svelte`.** Everything else is a
component a page composes. If a page ever needs another page, the shared part
wants extracting instead.

Cross-feature imports are fine and there are four: `outputs/OutputsTab` uses
`align/AlignHoldTimer` and `groups/GroupTitle`, and `groups/MusicGroupsTab` uses
`routing/FlowGraph`. Each is a domain widget being reused by a neighbour, which is
cheaper than duplicating it or flattening both into one directory.

## Layout

Component-to-component imports inside one directory stay `./Name.svelte`; across
directories they are `../<dir>/Name.svelte`. Everything reaches shared state and
the API through `../../lib/`.
