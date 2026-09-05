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
window.matchMedia("(min-width: 681px)").addEventListener("change", closeMenu);

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
      "Ownership, made explicit.\n\n  Improve the search experience\n  One executor holds the current run.\n  The claim carries a lease and a fence.\n\nBegin with a bounded piece of work.",
    aside: "A clear intention. A steady hand.",
    description:
      "Claim the item returned by next. Fenced work claims schedule execution; resource leases separately authorize mutation.",
  },
  capture: {
    command: 'engram work note "Rank exact matches first"',
    output:
      "A decision becomes shared context.\n\n  Rank exact matches first\n  Captured in the task working memory.\n  Available to the next participant.\n\nOne note, carried into the work record.",
    aside: "Leave the next mind a useful mark.",
    description:
      "Capture decisions and findings while you work. A holder note checkpoints progress and becomes shared task context.",
  },
  complete: {
    command: 'engram work done "Search ranking verified"',
    output:
      "Completion is checked, then sealed.\n\n  Acceptance and evidence validated\n  Required child work accounted for\n  Open obligations must be resolved\n\nAn immutable record of the finished work.",
    aside: "The work ends. Its memory remains.",
    description:
      "Once evidence and obligations are satisfied, close the run with a completion seal. Publication is a separate, planned capability.",
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

const compactViewport = window.matchMedia("(max-width: 680px)");
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
    '# 01 — Build from source (Rust required)\ngit clone https://github.com/grlap/Engram.git\ncd Engram\ncargo install --path .\n\n# 02 — Open a local advisory notebook (PowerShell)\n$env:ENGRAM_HOME = "$env:USERPROFILE/.engram"\nengram init --required-assurance advisory `\n  --authorized-by "$env:USERNAME" --reason "Local advisory setup"\nengram work next',
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
