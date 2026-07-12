## First launch on macOS

Allele is **ad-hoc signed but not yet notarised** (pre-alpha), so macOS Gatekeeper
shows a warning the first time you open it. This is expected — it's a one-time step
per install, not a sign that anything is wrong.

**Fastest — Terminal.** Clear the download quarantine, then open normally:

```sh
xattr -dr com.apple.quarantine /path/to/Allele.app
open /path/to/Allele.app
```

**Or via Finder.** Double-click **Allele**, dismiss the first warning, then open
**System Settings → Privacy & Security**, scroll to the bottom, and click
**Open Anyway** (you'll be asked to authenticate). macOS 15 Sequoia removed the old
right-click → Open shortcut, so this is now the GUI path.

Requires **macOS 14 or later on Apple silicon**. Notarised builds — double-click with
no prompt — are planned for the beta milestone.
