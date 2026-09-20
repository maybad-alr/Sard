// IS THE VIEWPORT A PHONE? — one answer, tracked, for the places that change BEHAVIOUR by width.
//
// WHY A HOOK AND NOT A CSS RULE. Most of the phone work is styling and lives in stylesheets. A few
// decisions are not: whether the reader shows its page chevrons, whether a chosen chapter dismisses the
// contents sheet. Those need the answer in JavaScript, and they need it to be REACTIVE — a phone
// rotated to landscape, or a desktop window dragged narrow, must change the answer without a reload.
//
// The query is the same `(max-width: 700px)` the library's chrome and every phone stylesheet use. It is
// stated once here so a second breakpoint cannot appear by accident.

import { useEffect, useState } from "react";

export const PHONE_QUERY = "(max-width: 700px)";

export function usePhoneViewport(): boolean {
  const [phone, setPhone] = useState(() => window.matchMedia(PHONE_QUERY).matches);
  useEffect(() => {
    const media = window.matchMedia(PHONE_QUERY);
    const update = () => setPhone(media.matches);
    // `matches` is re-read on the event rather than trusted from the closure: a listener can fire after
    // a second change has already been applied, and reading the live value makes that harmless.
    update();
    media.addEventListener("change", update);
    return () => media.removeEventListener("change", update);
  }, []);
  return phone;
}
