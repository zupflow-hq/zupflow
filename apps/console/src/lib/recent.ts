/** Per-viewer convenience: the last organization visited. */
const ORG_KEY = "zupflow.recent.org";

function get(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function set(key: string, value: string | null) {
  try {
    if (value) localStorage.setItem(key, value);
    else localStorage.removeItem(key);
  } catch {
    // Not remembered; harmless.
  }
}

export const lastOrgSlug = () => get(ORG_KEY);
export const rememberOrg = (slug: string) => set(ORG_KEY, slug);
