/**
 * Desktop-only integrations must be gated before making native calls, including at startup.
 * app_info currently exposes no OS. Android WebView includes Android in its user agent;
 * checking synchronously avoids a desktop-first render/check while IPC resolves.
 * The optional input keeps this decision testable without a browser or Tauri runtime.
 * iOS is also excluded from desktop integrations (this does not imply iOS support).
 */
export function isMobile(
  userAgent: string = typeof navigator === "undefined" ? "" : navigator.userAgent,
): boolean {
  return /Android|iPhone|iPad|iPod/i.test(userAgent);
}
