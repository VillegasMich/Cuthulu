// Runs before first paint so an explicit theme choice never flashes.
try {
  const t = localStorage.getItem("cuthulu.theme");
  if (t === "light" || t === "dark") document.documentElement.dataset.theme = t;
} catch (_) { /* storage blocked: follow the OS */ }

// Pane sizes set with the splitters (app.js): applied here too, so stored
// sizes are in place for the first paint. Keys and properties: DESIGN.md.
try {
  const css = document.documentElement.style;
  for (const k of ["meta", "sys"]) {
    const v = Number(localStorage.getItem(`cuthulu.split.${k}`));
    if (v > 0 && v < 10000) css.setProperty(`--split-${k}`, `${v}px`);
  }
  const cols = JSON.parse(localStorage.getItem("cuthulu.split.cols") || "{}");
  for (const k of ["state", "group", "image", "ports", "up"]) {
    const v = Number(cols[k]);
    if (v > 0 && v < 80) css.setProperty(`--col-${k}`, `${v}%`);
  }
} catch (_) { /* storage blocked or garbled: default sizes */ }
