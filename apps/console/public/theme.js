// Applies the stored theme before first paint so the page never flashes. A
// file rather than an inline script, so the Content-Security-Policy needs no
// exception for it.
try {
  const theme = localStorage.getItem("zupflow.theme");
  const system = !theme || theme === "system";
  const dark = theme === "dark" || (system && matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.classList.toggle("dark", dark);
} catch {}
