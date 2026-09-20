// ANDROID'S BACK GESTURE — what it closes, and when it is allowed to leave the app.
//
// THE DEFECT THIS EXISTS FOR. Android's back gesture is not a nicety on a phone, it is the way out of
// everything: a sheet, a dialog, a book. Sard handled none of it. Pressing back in the reader, with a
// book open, ended the process — the launcher came up and the book was gone. Reported from a real
// device, and reproduced: the activity was simply finished because nothing had claimed the press.
//
// HOW THE PRESS IS CLAIMED. `MainActivity` refuses to finish on its own and forwards the press here as
// a `sard:back` event (see the `onBackPressed` override). The decision is then made in one place, in
// this order:
//
//   1. SOMETHING IS OPEN — a sheet, a dialog, the drawer, a reader panel. The app already answers
//      Escape for every one of those through its dismissal stack, so the press is turned into that
//      Escape rather than a second, parallel close path that could disagree with the first. Whatever
//      is topmost closes, exactly as pressing Escape would have closed it.
//   2. A SCREEN OWNS THE BACK — the reader registers here while it is open, and its handler returns to
//      the library. Registration is a stack, so a surface opened later gets the press first.
//   3. NOTHING LEFT TO CLOSE — the app exits, through the window's own close path so the existing
//      flush (saving, tearing down) still runs. Never by finishing the activity behind the page's back.
//
// WHY THE EXIT IS NOT SIMPLY `super.onBackPressed()`. The page owns book state; ending the activity
// without its close path is what the reader reported as being thrown out of the program. Exiting
// through the window means the same save/flush the ✕ performs.
//
// THE DESKTOP IS UNAFFECTED. Nothing here runs unless the host sends `sard:back`, and nothing else on
// the desktop sends it.

/** A screen that wants the back press: return true when it consumed it. */
type BackHandler = () => boolean;

const handlers: BackHandler[] = [];

/**
 * Claim the back press while this screen is mounted. The most recent registration wins, so a surface
 * opened on top of another gets the press first; the returned function releases the claim.
 */
export function onAndroidBack(handler: BackHandler): () => void {
  handlers.push(handler);
  return () => {
    const i = handlers.lastIndexOf(handler);
    if (i >= 0) handlers.splice(i, 1);
  };
}

/**
 * Is there a surface on screen that the app already closes with Escape? Checked by what is RENDERED
 * rather than by a list of ids kept in step by hand: every one of these is a surface this app draws
 * only while it is open, and every one of them answers Escape.
 */
function openSurface(): boolean {
  return !!document.querySelector(
    [
      ".reader-panel.show",
      ".settings-panel.show",
      ".libd-mobile-drawer.is-open",
      ".gs",
      "[role='dialog']",
      "[role='menu']",
      "[aria-modal='true']",
    ].join(","),
  );
}

/** The same event the app's dismissal stack listens for, so the topmost surface closes as it would. */
function sendEscape(): void {
  document.dispatchEvent(
    new KeyboardEvent("keydown", { key: "Escape", code: "Escape", bubbles: true, cancelable: true }),
  );
  // The stack listens on the document; a panel that traps focus may listen on itself or on the window,
  // so the same press is offered there too. Duplicates are harmless: a closed surface ignores the
  // second, and the app's stack pops once per open surface.
  window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", code: "Escape", bubbles: true }));
}

/** The decision, as a value the activity can act on without waiting for a promise. */
export type BackOutcome = "handled" | "exit";

/**
 * ANSWER THE PRESS SYNCHRONOUSLY, AND LET THE ACTIVITY DO THE EXIT.
 *
 * The first version answered by calling a command to quit. It read well and did not work: the press was
 * consumed, the exit never happened, and the app then could not be left at all — measured on the device,
 * the process was still alive after back. An `invoke` on the way out depends on the IPC being up and the
 * command being in the handler, and neither failure is visible from here.
 *
 * So the page answers a question instead of taking an action: `window.__sardBack()` returns "handled"
 * when it closed something, and "exit" when there was nothing left — and `MainActivity` finishes the
 * activity on "exit". The only fact that crosses the boundary is a word, and the platform's own way of
 * leaving an app is what ends it.
 */
export function backOutcome(): BackOutcome {
  if (openSurface()) {
    sendEscape();
    return "handled";
  }
  for (let i = handlers.length - 1; i >= 0; i--) {
    if (handlers[i]!()) return "handled";
  }
  return "exit";
}

/**
 * Arm the bridge.
 *
 * `MainActivity` calls `window.__sardBack()` for every back press and finishes the activity when it
 * answers "exit". The `sard:back` event is kept as well: it is what a host that cannot read a return
 * value has to use, and it costs one listener.
 */
export function initAndroidBack(): () => void {
  (window as unknown as { __sardBack?: () => BackOutcome }).__sardBack = backOutcome;
  const onBack = () => {
    void backOutcome();
  };
  window.addEventListener("sard:back", onBack);
  return () => {
    window.removeEventListener("sard:back", onBack);
    delete (window as unknown as { __sardBack?: () => BackOutcome }).__sardBack;
  };
}
