// TOUCH FEEDBACK — the ripple that starts where the finger lands.
//
// WHAT THIS IS FOR. Android answers a press with ink that grows from the point of contact. It is the
// single strongest "this was made for this device" cue a surface can carry, and no amount of corner
// radius substitutes for it: a button that does not react to being touched reads as a web page in a
// phone frame no matter how it is shaped.
//
// WHY IT IS A DELEGATED LISTENER AND NOT A COMPONENT. There are hundreds of controls on these screens
// — every library chip, every reader tool, every settings row — and each one belongs to a different
// component with its own inline styling. Wrapping them all would be a rewrite; listening once at the
// document and asking which control was hit is a file. The ink is a single span appended to whatever
// was pressed and removed when its animation ends, so nothing accumulates and no component has to
// know this exists.
//
// PHONE ONLY, AND THAT IS LOAD-BEARING. The listener is attached only while the phone breakpoint
// matches, and detached when the window grows past it. A desktop pointer is not a finger: there is no
// contact point to grow ink from, hover already answers, and a ripple under a mouse would be a
// decoration the desktop never asked for.
//
// IT DOES NOT STEAL THE PRESS. The listener is passive and never calls `preventDefault`, so the
// control's own handlers run exactly as they did; the ink is painted beside them.

const HOST_CLASS = "sard-ripple-host";
const INK_CLASS = "sard-ripple";
const SELECTOR = [
  "button",
  "[role='button']",
  "[role='tab']",
  "[role='option']",
  "a[href]",
  "label[role='button']",
  "summary",
].join(",");

const PHONE = "(max-width: 700px)";

function reducedMotion(): boolean {
  return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

function paint(host: HTMLElement, x: number, y: number): void {
  const rect = host.getBoundingClientRect();
  // The ink is wider than the control so it reads as arriving from outside the visible shape, which is
  // what makes the edge of a chip look like it is being covered rather than lit from within.
  const size = Math.max(rect.width, rect.height) * 2.2;
  const ink = document.createElement("span");
  ink.className = INK_CLASS;
  ink.style.width = `${size}px`;
  ink.style.height = `${size}px`;
  ink.style.left = `${x - rect.left - size / 2}px`;
  ink.style.top = `${y - rect.top - size / 2}px`;
  host.classList.add(HOST_CLASS);
  host.appendChild(ink);
  const done = () => {
    ink.remove();
    // The host class is what supplied `position` and `overflow`; it leaves with the ink so nothing is
    // left behind that could clip a child the component later renders (a menu, a badge, a tooltip).
    host.classList.remove(HOST_CLASS);
  };
  ink.addEventListener("animationend", done, { once: true });
  // A cancelled animation (the element removed mid-flight, a tab hidden) never fires `animationend`.
  window.setTimeout(done, 700);
}

function onPointerDown(event: PointerEvent): void {
  if (reducedMotion()) return;
  // A secondary or middle press is not a tap on a control; neither is a drag that starts on one.
  if (event.button !== 0) return;
  const target = event.target;
  if (!(target instanceof Element)) return;
  const host = target.closest<HTMLElement>(SELECTOR);
  if (!host) return;
  // Disabled controls do not answer, and neither do the ones the app has marked as inert.
  if (host.hasAttribute("disabled") || host.getAttribute("aria-disabled") === "true") return;
  if (host.closest("[inert]")) return;
  paint(host, event.clientX, event.clientY);
}

/** Attach while the viewport is phone-sized. Returns a disposer for the caller's effect cleanup. */
export function initTouchFeedback(): () => void {
  const query = window.matchMedia(PHONE);
  let attached = false;
  const attach = () => {
    if (attached) return;
    document.addEventListener("pointerdown", onPointerDown, { passive: true });
    attached = true;
  };
  const detach = () => {
    if (!attached) return;
    document.removeEventListener("pointerdown", onPointerDown);
    attached = false;
  };
  const sync = () => (query.matches ? attach() : detach());
  sync();
  query.addEventListener("change", sync);
  return () => {
    detach();
    query.removeEventListener("change", sync);
  };
}
