# Pod visual regression (Windows)

Build `cargo build --workspace --release` in this worktree using the CPU gate,
sccache and four-job limits. No shared target directory. Captures launch only
`--ui-fixture`; no daemon, user repository or real agent is contacted.

From the repository root:

```powershell
powershell -NoProfile -File tools/visual/capture.ps1
powershell -NoProfile -File tools/visual/capture.ps1 -Mode compare
```

The capture pass uses dark and light themes, Malgun Gothic, reduced motion,
a fresh isolated config per screen and a fixed 1440×960 **physical pixel** window
on the secondary monitor. `manifest.json` records actual HWND bounds and DPI.
Use the same monitor scale, font installation and renderer for comparisons.
The script closes each fixture app even on failure. UIA trees are saved with PNGs.
Snapshots are visual evidence, not proof of native keyboard or Narrator behavior.

Compare uses maximum absolute RGB channel distance per pixel (default tolerance 8).
Any pixel above tolerance fails the comparison; there is no percentage allowance
or ignored region. Magenta pixels in `*.diff.png` show failures, with a dimmed
actual image underneath. Missing images or differing dimensions also fail.

To update baselines after reviewing **every** changed image:

```powershell
powershell -NoProfile -File tools/visual/capture.ps1 -Output tools/visual/baseline
```

`-Scenes`, `-Themes` and `-SettleSeconds` (default 2) narrow a run or give slow
scenes time (the editor LSP scenes use 40). The `editor` scene (P3) opens the
files panel of `sample/task-000` on a generated fixture git repository with
language servers off, so it is deterministic; other `editor-*` scenes are QA
evidence, not baselines (`crates/pod-app/qa/editor/run-editor-qa.ps1`).

Commit PNGs, UIA trees and the manifest together with the source change. Never
accept a baseline merely to hide a failure. Keep QA captures under
`crates/pod-app/qa/d3/`. `-Width 1024 -Height 700` exercises a small physical window.
Changing that size is a separate matrix, not comparable to the default baseline.

`native.ps1 -AppPid PID -Keys 17,75` takes foreground focus with AttachThreadInput
and sends Ctrl+K through SendInput. Input is refused if another HWND owns focus.
`-Text` uses Unicode SendInput, which **does not verify IME composition**.
Only use this helper against an isolated fixture window while keyboard takeover
is appropriate. `contrast.py` audits default tokens against WCAG AA thresholds.

Additional background-only checks (no keyboard or pointer takeover):

```powershell
& tools/visual/capture.ps1 -HighContrast -Scenes @('settings','palette') -Output crates/pod-app/qa/d3/highcontrast
& tools/visual/mixed-dpi.ps1
& tools/visual/restore-panels.ps1
& tools/visual/compare-selftest.ps1
```

Mixed-DPI captures use one window moved secondary → primary → secondary,
measuring actual HWND DPI at every step and sizing it to 1024×700 logical units.
The restore check invokes the inspector through UIA without acquiring focus,
then restarts with the same isolated config and checks both named tabs.
These tests do not change any Windows settings.

The read-only fixture has 100 tasks and 300 terminals. Review and Gates capture
their disconnected/empty states, because the fixture intentionally has no daemon
or real worktree. Populated review comments, live terminal rendering, actual IME,
and full request lifecycles need the separate manual integration checklist in
`docs/10-ui-design.md`; these baselines must not be represented as that coverage.
