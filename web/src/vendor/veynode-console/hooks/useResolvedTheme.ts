import { useSyncExternalStore } from "react";

// The provider resolves the theme onto <html class="dark">; artwork that cannot
// read CSS tokens (an SVG drawn as an image) follows that class. One observer
// serves every subscriber.
const listeners = new Set<() => void>();
let observer: MutationObserver | undefined;
function subscribe(notify: () => void) {
  listeners.add(notify);
  if (!observer) {
    observer = new MutationObserver(() => listeners.forEach((listener) => listener()));
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ["class"] });
  }
  return () => {
    listeners.delete(notify);
    if (!listeners.size) {
      observer?.disconnect();
      observer = undefined;
    }
  };
}
const snapshot = () => (document.documentElement.classList.contains("dark") ? "dark" : "light");

export function useResolvedTheme(): "light" | "dark" {
  return useSyncExternalStore(subscribe, snapshot, () => "light");
}
