// Runs before first paint so an explicit theme choice never flashes.
try {
  const t = localStorage.getItem("cuthulu.theme");
  if (t === "light" || t === "dark") document.documentElement.dataset.theme = t;
} catch (_) { /* storage blocked: follow the OS */ }
