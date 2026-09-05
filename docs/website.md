# Project website

The project landing page lives in [website/index.html](../website/index.html).
It is a static site with no JavaScript dependencies or build step. Its copy
introduces the memory and work loop described in the [vision](vision.md),
and keeps [shipped alpha capabilities](shipped.md) separate from the
[roadmap](roadmap.md).

## Local preview

From the repository root, with Python 3 installed:

```bash
python -m http.server 8000 --bind 127.0.0.1 --directory website
```

Open `http://127.0.0.1:8000`. Stop the server with Ctrl+C. Any static file
server can serve the same directory. The site can also be opened directly
from disk; if clipboard access is unavailable, Copy selects the commands
for manual copying.

For hosting, publish the contents of `website/` as the static document root.
Relative asset paths also support deployment in a subdirectory. No service,
database, API keys, or Engram installation is needed to run the website.
Publication is a separate user decision.

## Design and interaction

The visual direction is a Leonardo da Vinci inspired inventor's notebook:
parchment, sepia ink, serif typography, geometric construction lines, and an
original imaginary memory apparatus. The hero illustration was generated
with the built-in image generation tool; the exact prompt is saved in
[memory-machine.prompt.txt](../website/assets/memory-machine.prompt.txt).
It is a new illustration, not a historical Leonardo work or an anatomical
reference. Fonts use local system families; the page makes no third-party
asset requests and contains no analytics.

CSS animates orbital construction lines, a terminal caret, the hero's
entrance, and scroll reveals. The footer motion control pauses animation
and persists the preference when browser storage is available. The OS
reduced-motion preference always takes precedence. Content stays visible
without JavaScript; the initial examples remain readable.

JavaScript adds mobile navigation, keyboard-operated work-cycle and
operating-system tabs, clipboard copying with a selection fallback, and
motion controls. The working notebook is explicitly illustrative; it does
not execute commands or connect to a real Engram store. Setup commands
explicitly initialize an advisory notebook; enforced control requires the
integration described in the [host checklist](host-checklist.md).

## Validation

Run `node --check website/app.js` and the repository's required quality gates.
Preview at desktop and mobile widths. Exercise all four work-cycle tabs,
both operating-system tabs, the copy action, mobile navigation, and the motion
control with mouse and keyboard. Check reduced motion and reload with
JavaScript disabled. Check for overflow, failed assets, and console errors.
