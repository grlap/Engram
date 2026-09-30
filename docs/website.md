# Project website

The project website lives in [website/index.html](../website/index.html).
It is one continuous page in two worlds: four manuscript chapters for the
humans, then four machine signals for the agents — scrolling past the
manuscript crosses into the machine's own future, and the fixed header and
footer flip themes with it.
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

Manuscript typography uses a shared, responsive scale: main copy is 17–20 px at the
default browser font size, controls and supporting prose are 16–18 px, code
is 14–16 px, and small labels are 12–14 px. The scale uses rem units so
browser text preferences carry through. Shorter screens allow the chapters
to grow while keeping the text readable. On small phones the source link
lives in the navigation menu, and the motion control keeps its accessible
text label alongside a visible icon.

In both themes, the header uses a menu button at widths up to 1250px. At
1360px and below, the footer fits eight page links between 44px motion and
next-page controls; the motion label stays accessible but is visually hidden,
and the page cue and numeric page count are hidden.

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

## The machine section

After the fourth manuscript leaf, four machine signals — **Boot**,
**Vocabulary**, **Wake up**, and **Honest limits** — speak to the product's
primary users, coding agents, in a warm machine voice ("You will end. Your
work won't."): the fourteen words as the agent's working vocabulary, recovery
after context compaction, and the product's stated boundaries (identity is
asserted, turn gating depends on host integration, Engram never calls a model, the demos execute
nothing).

The machine section uses black ceramic and chrome, cyan and magenta optical
lighting, bold filled and outlined capitals, circuit traces and console
instrumentation. Plate V is an original synthetic intelligence portrait,
generated with the built-in image tool and stored as a lazy-loaded 1254-pixel
WebP. Its exact prompt is in
[synthetic-agent.prompt.txt](../website/assets/synthetic-agent.prompt.txt).
The portrait contains no text; captions and decorative HUD labels remain HTML.
The compact machine HTML annotations and captions intentionally use a smaller
0.5–0.6875rem scale (8–11px at the default browser font size), distinct from
the manuscript labels. These rem sizes still follow browser text preferences;
larger text can make a signal grow beyond one viewport.
The fourteen words form a command matrix lit by group, recovery uses a
circular instrument, and limits use a hazard-striped panel beside the
connection snippet. Recovery copy distinguishes retained notes from authority
and requires inspection of claim ownership and expiry before resuming.
Machine styles live in a separate
[machine.css](../website/machine.css) loaded after `styles.css`; it
redefines the manuscript's colour variables under `.folio-machine`. While a
machine folio is active the script sets
`data-theme="machine"` on the root so the fixed header, footer, and page
index flip with the content. Manuscript leaf content keeps its original styling;
shared navigation lives in `styles.css` and follows the responsive layout described above. The same motion contract
applies: the footer control pauses animation, the OS reduced-motion
preference wins, and content stays visible without JavaScript. All console
demos are explicitly illustrative, and the connect snippet initializes
nothing by itself.

## Validation

Run `node --check website/app.js` and the repository's required quality gates.
Preview at desktop and mobile widths, including short landscape windows and
ultrawide displays. Check that the studies stay close to the central content
and that their faded edges keep text and controls readable.
Check all eight folios, wheel scrolling in both directions, keyboard page
navigation, chapter links, deep links, browser history, and active page
indicators. Exercise all four work-cycle tabs, both operating-system tabs,
both copy actions (setup and connect), mobile navigation, and the motion
control. Check reduced
motion and reload with JavaScript disabled. Check for horizontal overflow,
content obscured by the fixed navigation, failed assets, and console errors.
At the manuscript/machine boundary, check that the header, footer, and page
index flip themes as the boundary folio crosses the viewport midpoint in both
directions, that the core's float, scan line, rings, gauge arc, and status
pulses stop with the motion control, that the boot-log lines reveal in order,
and that every signal fits a 900px-tall window at desktop widths of 1001px
and wider, with default text size, without the title block covering content.
Narrower screens and enlarged text use growing folios and native scrolling.
The manuscript folios print; the machine section is a screen feature and is not printed.
