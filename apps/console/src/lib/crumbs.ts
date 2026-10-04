import { useEffect, useSyncExternalStore } from "react";

/** The current page's resource label for the breadcrumb trail. */
let label: string | null = null;
const listeners = new Set<() => void>();

function setLabel(next: string | null) {
  label = next;
  for (const l of listeners) l();
}

export function useCrumbs() {
  return useSyncExternalStore(
    (l) => {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    () => label,
  );
}

/** Set the final breadcrumb while the calling page is mounted. */
export function useDetailCrumb(value: string | undefined | null) {
  useEffect(() => {
    setLabel(value ?? null);
    return () => setLabel(null);
  }, [value]);
}
