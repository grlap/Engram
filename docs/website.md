# Project website

The four-chapter project website lives in [website/index.html](../website/index.html).
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

The website is licensed under Apache-2.0, like the rest of Engram. Keep
[website/LICENSE.txt](../website/LICENSE.txt) in the published directory; it is an
identical copy of the repository's [LICENSE](../LICENSE), linked from the
third chapter. Copyright 2026 Engram contributors.

## Design and interaction

The visual direction is a Leonardo da Vinci inspired inventor's notebook:
four manuscript leaves, parchment, sepia ink, serif typography, and an
original imaginary memory apparatus. Eight detailed manuscript fragments fill
the margins with fine penwork, crosshatching, construction lines, and tiny
handwritten annotations embedded in the artwork. Each leaf has its own
studies: perception and armillary instruments, clockwork and mechanical
linkages, then writing tools and a seal press. The fourth leaf has two original
studies: an articulated mechanical hand writing with a quill, and an imagined
listening instrument that records a voice. They frame the handwritten agent notes.

The illustrations were generated with the built-in image generation tool.
The first six margin studies use the existing hero as their visual reference.
The fourth leaf's studies were generated independently in the same penwork
and parchment style. Exact
prompts are saved in [memory-machine.prompt.txt](../website/assets/memory-machine.prompt.txt)
and [manuscript-prompts.txt](../website/assets/manuscript-prompts.txt), with the
new pair in [manuscript-voices-prompts.txt](../website/assets/manuscript-voices-prompts.txt). These
are original imaginary illustrations, not historical Leonardo works or
technical references; the tiny manuscript lettering is decorative notation.
The margins are stored as WebP images at their original dimensions, with
later chapters loaded lazily. The studies are positioned relative to the
central content, keeping the whole composition together on ultrawide
screens. Their faint inner edges overlap the content area; the artwork
sits behind the text and controls. A shared ink filter and multiply blending
unify the hero and margin paper tones. Navigation also has a maximum width
to follow the centered composition. Meaningful captions use italic serif type. Fonts use local system
families; the page makes no third-party asset requests and contains no analytics.

Typography uses a shared, responsive scale: main copy is 17–20 px at the
default browser font size, controls and supporting prose are 16–18 px, code
is 14–16 px, and small labels are 12–14 px. The scale uses rem units so
browser text preferences carry through. Shorter screens allow the chapters
to grow while keeping the text readable. On small phones the source link
lives in the navigation menu, and the motion control keeps its accessible
text label alongside a visible icon.

The four chapters are **The idea**, **The mechanism**, **Your notebook**, and
**Agent notes**.
On desktop, each fills at least one viewport and native CSS scroll snapping
moves between manuscript leaves. A fixed contents bar and Roman-numeral page
links provide direct navigation; the footer reflects the visible chapter and
links to the next leaf, or back to the beginning. Ordinary anchors preserve
deep links and browser history. When all leaves fit a desktop viewport, a
vertical wheel gesture advances one leaf; its trailing momentum cannot skip
over another. Zoom and horizontal-wheel gestures remain native, as do touch
scrolling and keyboard page navigation. On narrow or short screens, or when
any leaf is taller than the viewport, native scrolling and growing page
heights keep longer content readable. Marginalia moves into two columns below
the main content on small screens.

The fourth leaf pairs an approved quote from Engram::Advisor, a project AI
agent reviewing the Phoenix pilot, with the explicitly fictional aside
“Would recommend to my next session.” — Anonymous, but recorded. Advisor’s
quote is a dated September 2026 pilot observation, including a rough edge,
not an independent customer endorsement. His report of avoided arguments
comes from pilot agents; he reviewed reports and source rather than executing
the pilot tasks himself. Keep that date and context if the memory revision
behavior changes. Quotes use large local handwriting fonts with a serif
fallback; a short pen-stroke flourish respects both motion controls.

CSS animates orbital construction lines, a terminal caret,
the page-turn cue, and scroll reveals. The footer motion control pauses animation
and persists the preference when browser storage is available. The OS
reduced-motion preference always takes precedence. Content stays visible
without JavaScript; the initial examples remain readable.

JavaScript tracks the visible leaf and adds mobile navigation, keyboard-operated work-cycle and
operating-system tabs, clipboard copying with a selection fallback, and
motion controls. The working notebook is explicitly illustrative; it does
not execute commands or connect to a real Engram store. Setup commands
explicitly initialize an advisory notebook; enforced control requires the
integration described in the [host checklist](host-checklist.md).

## Validation

Run `node --check website/app.js` and the repository's required quality gates.
Preview at desktop and mobile widths, including short landscape windows and
ultrawide displays. Check that the studies stay close to the central content
and that their faded edges keep text and controls readable.
Check the four leaves, wheel scrolling in both directions, keyboard page
navigation, chapter links, deep links, browser history, and active page
indicators. Exercise all four work-cycle tabs, both operating-system tabs,
the copy action, mobile navigation, and the motion control. Check reduced
motion and reload with JavaScript disabled. Check for horizontal overflow,
content obscured by the fixed navigation, failed assets, and console errors.
