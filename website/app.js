const root = document.documentElement;
const motionPreference = window.matchMedia("(prefers-reduced-motion: reduce)");
const motionToggle = document.querySelector("#motion-toggle");
let manuallyPaused = false;

try {
  manuallyPaused = localStorage.getItem("engram-motion-paused") === "true";
} catch {
  // The page also works when browser storage is unavailable.
}

function syncMotion() {
  const paused = manuallyPaused || motionPreference.matches;
  root.dataset.motion = paused ? "paused" : "playing";
  motionToggle.setAttribute("aria-pressed", String(paused));
  motionToggle.querySelector(".motion-label").textContent =
    motionPreference.matches
      ? "Reduced motion enabled"
      : paused
        ? "Resume motion"
        : "Pause motion";
  motionToggle.querySelector(".motion-icon").textContent = paused ? "▷" : "Ⅱ";
  motionToggle.disabled = motionPreference.matches;
}

syncMotion();
motionPreference.addEventListener("change", syncMotion);
motionToggle.addEventListener("click", () => {
  manuallyPaused = !manuallyPaused;
  try {
    localStorage.setItem("engram-motion-paused", String(manuallyPaused));
  } catch {
    // Keep the in-page control usable without persistent preferences.
  }
  syncMotion();
});

if ("IntersectionObserver" in window) {
  const observer = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (entry.isIntersecting) {
          entry.target.classList.add("visible");
          observer.unobserve(entry.target);
        }
      }
    },
    { threshold: 0.08 },
  );
  root.classList.add("js-motion");
  document
    .querySelectorAll(".reveal")
    .forEach((element) => observer.observe(element));
}

const menuToggle = document.querySelector(".menu-toggle");
const mobileNav = document.querySelector("#mobile-nav");

function closeMenu() {
  menuToggle.setAttribute("aria-expanded", "false");
  menuToggle.setAttribute("aria-label", "Open navigation");
  mobileNav.hidden = true;
}

menuToggle.addEventListener("click", () => {
  const expanded = menuToggle.getAttribute("aria-expanded") !== "true";
  menuToggle.setAttribute("aria-expanded", String(expanded));
  menuToggle.setAttribute(
    "aria-label",
    expanded ? "Close navigation" : "Open navigation",
  );
  mobileNav.hidden = !expanded;
});
mobileNav.addEventListener("click", (event) => {
  if (event.target.closest("a")) closeMenu();
});
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && !mobileNav.hidden) {
    closeMenu();
    menuToggle.focus();
  }
});
document.addEventListener("click", (event) => {
  if (!event.target.closest(".site-header") && !mobileNav.hidden) closeMenu();
});
window.matchMedia("(min-width: 761px)").addEventListener("change", closeMenu);

const folios = [...document.querySelectorAll("[data-folio]")];
const folioLinks = [...document.querySelectorAll("[data-folio-link]")];
const nextFolio = document.querySelector("#next-folio");
const folioTitles = ["The idea", "The mechanism", "Your notebook"];
let activeFolio = -1;
let folioFrame;

function updateFolio() {
  folioFrame = undefined;
  const midpoint = window.innerHeight / 2;
  let index = folios.findIndex((folio) => {
    const bounds = folio.getBoundingClientRect();
    return bounds.top <= midpoint && bounds.bottom > midpoint;
  });
  if (index === -1) index = 0;
  if (index === activeFolio) return;
  activeFolio = index;
  for (const link of folioLinks) {
    if (link.hash === `#${folios[index].id}`) {
      link.setAttribute("aria-current", "location");
    } else {
      link.removeAttribute("aria-current");
    }
  }
  const nextIndex = (index + 1) % folios.length;
  nextFolio.href = `#${folios[nextIndex].id}`;
  nextFolio.setAttribute(
    "aria-label",
    nextIndex === 0
      ? "Return to the first page"
      : `Next page: ${folioTitles[nextIndex]}`,
  );
  document.querySelector("#current-page").textContent =
    folios[index].dataset.folio;
  document.querySelector("#page-cue").textContent =
    nextIndex === 0 ? "Return to the first page" : "Scroll to turn the page";
  nextFolio.querySelector(".page-arrow").textContent =
    nextIndex === 0 ? "↑" : "↓";
}

function scheduleFolioUpdate() {
  if (folioFrame === undefined) folioFrame = requestAnimationFrame(updateFolio);
}

// Links, touch scrolling, and keyboard navigation retain their native behavior.
window.addEventListener("scroll", scheduleFolioUpdate, { passive: true });
window.addEventListener("resize", scheduleFolioUpdate);
window.addEventListener("pageshow", scheduleFolioUpdate);
updateFolio();

const pagedViewport = window.matchMedia(
  "(min-width: 761px) and (min-height: 621px)",
);
let lastWheelAt = 0;
let wheelDistance = 0;
let wheelHandled = false;
let wheelLockedUntil = 0;

window.addEventListener(
  "wheel",
  (event) => {
    // Long pages and horizontal or zoom gestures need their ordinary browser behavior.
    if (
      !pagedViewport.matches ||
      event.ctrlKey ||
      event.metaKey ||
      event.shiftKey ||
      Math.abs(event.deltaX) >= Math.abs(event.deltaY) ||
      folios.some((folio) => folio.offsetHeight > window.innerHeight + 1)
    )
      return;

    const now = performance.now();
    if (now - lastWheelAt > 220) {
      wheelDistance = 0;
      wheelHandled = false;
    }
    lastWheelAt = now;
    event.preventDefault();
    if (wheelHandled || now < wheelLockedUntil) return;

    const unit =
      event.deltaMode === 1
        ? 16
        : event.deltaMode === 2
          ? window.innerHeight
          : 1;
    wheelDistance += event.deltaY * unit;
    if (Math.abs(wheelDistance) < 28) return;

    updateFolio();
    const index = Math.max(
      0,
      Math.min(folios.length - 1, activeFolio + Math.sign(wheelDistance)),
    );
    wheelHandled = true;
    const paused = root.dataset.motion === "paused";
    wheelLockedUntil = now + (paused ? 100 : 700);
    folios[index].scrollIntoView({
      behavior: paused ? "instant" : "smooth",
      block: "start",
    });
  },
  { passive: false },
);

const cycle = {
  observe: {
    command: "engram work next",
    output:
      "Your next piece of work is ready.\n\n  Improve the search experience\n  Acceptance: relevant results come first\n\nRemembered constraint\n  Keep the search index on the local host.",
    aside: "First, understand the work.",
    description:
      "Find what you hold, what is ready, and what changed. Start with the context that matters.",
  },
  claim: {
    command: "engram work claim <item>",
    output:
      "Ownership, made explicit.\n\n  Improve the search experience\n  One executor holds the current run.\n  The claim is time-limited and renewable.\n\nBegin with a bounded piece of work.",
    aside: "A clear intention. A steady hand.",
    description:
      "Claim a ready item to take responsibility for its execution. Permission to change files is handled separately.",
  },
  capture: {
    command: 'engram work note "Rank exact matches first"',
    output:
      "A decision becomes shared context.\n\n  Rank exact matches first\n  Recorded in the work history.\n  Shared with other participants.\n\nOne note, carried into the work record.",
    aside: "Leave the next mind a useful mark.",
    description:
      "Record a finding or decision. Engram checkpoints your progress and makes the note available to other participants.",
  },
  complete: {
    command: 'engram work done "Search ranking verified"',
    output:
      "Completion is checked, then sealed.\n\n  Recorded evidence checked\n  Required child work accounted for\n  Open obligations must be resolved\n\nAn immutable record of the finished work.",
    aside: "The work ends. Its memory remains.",
    description:
      "Engram checks recorded evidence before sealing the run. Report publication is planned.",
  },
};

// Both tab groups support the WAI-ARIA keyboard pattern as well as clicks.
function bindTabs(tabs, activate) {
  function select(tab, focus = false) {
    for (const candidate of tabs) {
      const selected = candidate === tab;
      candidate.setAttribute("aria-selected", String(selected));
      candidate.tabIndex = selected ? 0 : -1;
      candidate.classList.toggle("active", selected);
    }
    activate(tab);
    if (focus) tab.focus();
  }

  tabs.forEach((tab, index) => {
    tab.addEventListener("click", () => select(tab));
    tab.addEventListener("keydown", (event) => {
      const vertical =
        tab.parentElement.getAttribute("aria-orientation") === "vertical";
      let next;
      if (event.key === (vertical ? "ArrowDown" : "ArrowRight"))
        next = (index + 1) % tabs.length;
      if (event.key === (vertical ? "ArrowUp" : "ArrowLeft"))
        next = (index - 1 + tabs.length) % tabs.length;
      if (event.key === "Home") next = 0;
      if (event.key === "End") next = tabs.length - 1;
      if (next !== undefined) {
        event.preventDefault();
        select(tabs[next], true);
      }
    });
  });
  select(
    tabs.find((tab) => tab.getAttribute("aria-selected") === "true") || tabs[0],
  );
}

bindTabs([...document.querySelectorAll("[data-step]")], (tab) => {
  const selected = tab.dataset.step;
  const step = cycle[selected];
  document
    .querySelector("#cycle-panel")
    .setAttribute("aria-labelledby", tab.id);
  document.querySelector("#demo-command").textContent = step.command;
  document.querySelector("#demo-output").textContent = step.output;
  document.querySelector("#demo-aside").textContent = step.aside;
  document.querySelector("#demo-description").textContent = step.description;
  document.querySelectorAll(".diagram-node").forEach((node) => {
    const active = node.dataset.node === selected;
    node.querySelector("circle").style.fill = active ? "#dddcc7" : "#f0ece0";
    node.querySelector("circle").style.stroke = active ? "#575b39" : "#96917d";
  });
});

const compactViewport = window.matchMedia("(max-width: 760px)");
function syncCycleOrientation() {
  document
    .querySelector(".cycle-steps")
    .setAttribute(
      "aria-orientation",
      compactViewport.matches ? "horizontal" : "vertical",
    );
}
syncCycleOrientation();
compactViewport.addEventListener("change", syncCycleOrientation);

const setupCode = document.querySelector("#setup-code");
const commands = {
  unix: setupCode.textContent,
  windows:
    '# 01 — Build from source (Git + Rust required)\ngit clone https://github.com/grlap/Engram.git\ncd Engram\ncargo install --path .\n\n# 02 — Open a local advisory notebook (PowerShell)\n$env:ENGRAM_HOME = "$env:USERPROFILE/.engram"\nengram init --required-assurance advisory `\n  --authorized-by "$env:USERNAME" --reason "Local advisory setup"\nengram work next',
};
const copyButton = document.querySelector("#copy-setup");
const copyFeedback = document.querySelector("#copy-feedback");
let feedbackTimer;

bindTabs([...document.querySelectorAll("[data-os]")], (tab) => {
  setupCode.textContent = commands[tab.dataset.os];
  document
    .querySelector("#setup-code-panel")
    .setAttribute("aria-labelledby", tab.id);
  clearTimeout(feedbackTimer);
  copyButton.querySelector("span").textContent = "Copy";
  copyFeedback.textContent = "";
});

copyButton.addEventListener("click", async () => {
  copyButton.disabled = true;
  clearTimeout(feedbackTimer);
  try {
    await navigator.clipboard.writeText(setupCode.textContent);
    copyButton.querySelector("span").textContent = "Copied";
    copyFeedback.textContent = "Setup commands copied to clipboard.";
  } catch {
    const selection = window.getSelection();
    const range = document.createRange();
    range.selectNodeContents(setupCode);
    selection.removeAllRanges();
    selection.addRange(range);
    copyButton.querySelector("span").textContent = "Selected";
    copyFeedback.textContent =
      "Clipboard unavailable. Commands selected; use your browser’s Copy command.";
  } finally {
    copyButton.disabled = false;
    feedbackTimer = setTimeout(() => {
      copyButton.querySelector("span").textContent = "Copy";
    }, 2500);
  }
});
