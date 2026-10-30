// SlobKnob overlay renderer. The Rust side decides everything; this only
// renders "knob-ui" events and "knob-theme" changes. Without window.__TAURI__
// it runs a demo (see runDemo) so it can be checked in a plain browser.
//
// With ?preview=1 (the settings window's iframe) it skips Tauri and the demo,
// fits the wheel to the frame, and renders whatever the parent posts: the same
// UiEvent JSON ({type:"open"|"select"|"close"|"hud", ...}) plus
// {type:"theme", accent, dim, platform}.

import { iconShapes, iconSvg, PASTEL, PASTEL_FALLBACK } from "./icons.js";

const PREVIEW = new URLSearchParams(location.search).has("preview");

// ---------- Geometry (logical px, wheel center = 0,0; 0deg = top, clockwise) ----------

const R_RING = 236;
const R_SLICE_IN = 118;
const R_SLICE_OUT = 228;
const R_SEL_OUT = 237; // selected slice pops out 9px
const R_ICON = 168;
const R_DOT = 217;
const GAP_DEG = 0.9; // on each side of each slice
const CORNER = 4; // corner rounding of the slice shapes

const ICON_SIZE = 34;
const ICON_STROKE = 1.75;
const LABEL_DY = 31; // label baseline below the icon center

const HUB_ICON_Y = -34;
const HUB_ICON_SIZE = 44;
const HUB_ICON_STROKE = 1.5;
const HUB_NAME_Y = 16;
const HUB_HINT_Y = 37;
const HUB_HINT_LH = 15;
const HUB_HINT_MAX_LINES = 3;

// Selection slide: time constant of the exponential approach. 18ms puts the
// highlight ~99% of the way there after ~80ms.
const SELECT_TAU = 18;
const HUD_IN_MS = 120;
const HUD_HOLD_MS = 1000;

const DEFAULT_ACCENT = "#0a84ff";

const HEX_RE = /^#[0-9a-f]{6}$/i;

// ---------- DOM ----------

const $ = (id) => document.getElementById(id);
const el = {
  dim: $("dim"),
  wheel: $("wheel"),
  slices: $("slices"),
  hl: $("hl"),
  items: $("items"),
  dot: $("active-dot"),
  hubIcon: $("hub-icon"),
  hubName: $("hub-name"),
  hubHint: $("hub-hint"),
  footer: $("footer"),
  title: $("wheel-title"),
  titleText: $("wheel-title-text"),
  glow: $("kw-glow-shadow"),
  hud: $("hud"),
  hudIcon: $("hud-icon"),
  hudText: $("hud-text"),
};

const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

// ---------- State ----------

const state = {
  theme: { accent: DEFAULT_ACCENT, pastel: true, dim: true, platform: guessPlatform() },
  open: false,
  wheel: "",
  nested: false,
  slices: [],
  selected: 0,
  active: null,
  via: "tap",
  autoConfirmMs: 0,
  hudPinned: false, // demo only: keep the HUD up for screenshots
};

let hlAngle = 0; // displayed rotation of the highlight, degrees
let hlTarget = 0; // unwrapped target rotation
let hlRaf = 0;
let hlLast = 0; // timestamp of the previous highlight frame
let hudTimer = 0;

// ---------- Helpers ----------

function guessPlatform() {
  const p = String(navigator.userAgentData?.platform || navigator.platform || "").toLowerCase();
  if (p.includes("mac")) return "macos";
  if (p.includes("win")) return "windows";
  return "linux";
}

const n3 = (v) => Math.round(v * 1000) / 1000;
const rad = (deg) => (deg * Math.PI) / 180;
const deg = (r) => (r * 180) / Math.PI;

/** Point at radius r and angle a (degrees, 0 = top, clockwise). */
function pt(r, a) {
  return `${n3(r * Math.sin(rad(a)))} ${n3(-r * Math.cos(rad(a)))}`;
}

function xy(r, a) {
  return [r * Math.sin(rad(a)), -r * Math.cos(rad(a))];
}

/** Annular sector from angle a0 to a1 (a0 < a1), with rounded corners. */
function sectorPath(rIn, rOut, a0, a1, rc) {
  const span = a1 - a0;
  const phiO = rc > 0 ? deg(Math.asin(rc / (rOut - rc))) : 0;
  const phiI = rc > 0 ? deg(Math.asin(rc / (rIn + rc))) : 0;
  if (rc <= 0 || 2 * phiI >= span || 2 * rc >= rOut - rIn) {
    const large = span > 180 ? 1 : 0;
    return (
      `M${pt(rOut, a0)}A${rOut} ${rOut} 0 ${large} 1 ${pt(rOut, a1)}` +
      `L${pt(rIn, a1)}A${rIn} ${rIn} 0 ${large} 0 ${pt(rIn, a0)}Z`
    );
  }
  const dO = (rOut - rc) * Math.cos(rad(phiO));
  const dI = (rIn + rc) * Math.cos(rad(phiI));
  const largeO = span - 2 * phiO > 180 ? 1 : 0;
  const largeI = span - 2 * phiI > 180 ? 1 : 0;
  return (
    `M${pt(dO, a0)}` +
    `A${rc} ${rc} 0 0 1 ${pt(rOut, a0 + phiO)}` +
    `A${rOut} ${rOut} 0 ${largeO} 1 ${pt(rOut, a1 - phiO)}` +
    `A${rc} ${rc} 0 0 1 ${pt(dO, a1)}` +
    `L${pt(dI, a1)}` +
    `A${rc} ${rc} 0 0 1 ${pt(rIn, a1 - phiI)}` +
    `A${rIn} ${rIn} 0 ${largeI} 0 ${pt(rIn, a0 + phiI)}` +
    `A${rc} ${rc} 0 0 1 ${pt(dI, a0)}Z`
  );
}

function esc(s) {
  return String(s).replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}

function textWidth(t) {
  try {
    return t.getComputedTextLength();
  } catch {
    return 0;
  }
}

/** Shrinks an SVG <text> to fit maxW (down to minPx), then ellipsizes. */
function fitText(t, maxW, basePx, minPx) {
  t.style.fontSize = "";
  const w = textWidth(t);
  if (!w || w <= maxW) return;
  // Width doesn't scale exactly with size (hinting), so step down until it fits.
  let px = Math.max(minPx, Math.floor(((basePx * maxW) / w) * 4) / 4);
  t.style.fontSize = `${px}px`;
  while (textWidth(t) > maxW && px > minPx) {
    px = Math.max(minPx, px - 0.25);
    t.style.fontSize = `${px}px`;
  }
  if (textWidth(t) <= maxW) return;
  const full = t.textContent;
  let lo = 0;
  let hi = full.length;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    t.textContent = full.slice(0, mid).trimEnd() + "…";
    if (textWidth(t) <= maxW) lo = mid;
    else hi = mid - 1;
  }
  t.textContent = full.slice(0, lo).trimEnd() + "…";
}

function fmtSeconds(ms) {
  return String(Math.round(ms / 10) / 100);
}

function relLuminance(hex) {
  const c = [1, 3, 5].map((i) => {
    const v = parseInt(hex.slice(i, i + 2), 16) / 255;
    return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
}

function parsePayload(p) {
  if (typeof p === "string") {
    try {
      return JSON.parse(p);
    } catch {
      return null;
    }
  }
  return p;
}

function toIndex(v, n) {
  const i = Number(v);
  if (!Number.isFinite(i) || n === 0) return 0;
  return Math.min(n - 1, Math.max(0, Math.trunc(i)));
}

// ---------- Layout ----------

function place() {
  el.wheel.style.left = `${Math.round(window.innerWidth / 2 - 300)}px`;
  el.wheel.style.top = `${Math.round(window.innerHeight / 2 - 300)}px`;
  if (PREVIEW) {
    // Fit the ring (474px with the popped-out highlight), plus the title pill
    // above it for a sub-wheel, into the frame. The footer is hidden here.
    const extra = state.nested ? 72 : 0;
    const fit = Math.min((window.innerWidth - 8) / 476, (window.innerHeight - 8) / (476 + extra), 1.25);
    const root = document.documentElement;
    root.style.setProperty("--fit", String(Math.max(0.2, fit)));
    root.style.setProperty("--shift", `${(extra / 2) * fit}px`);
  }
}

// ---------- Theme ----------

function applyTheme(raw) {
  const t = parsePayload(raw) || {};
  const hex = typeof t.accent === "string" && HEX_RE.test(t.accent);
  const pastel = t.accent === "pastel" || (!hex && state.theme.pastel);
  const accent = hex ? t.accent.toLowerCase() : state.theme.accent;
  const dim = typeof t.dim === "boolean" ? t.dim : state.theme.dim;
  const platform = ["macos", "windows", "linux"].includes(t.platform) ? t.platform : state.theme.platform;
  state.theme = { accent, pastel, dim, platform };

  document.documentElement.dataset.platform = platform;
  setAccent(document.documentElement, colorFor(state.slices[state.selected]));
  el.dim.classList.toggle("on", state.open && dim && !PREVIEW);
  renderFooter();
}

/** Accent for a slice ({id, color}) or a plain mode id. A slice's own color wins. */
function colorFor(slice) {
  const s = typeof slice === "string" ? { id: slice } : slice || {};
  if (typeof s.color === "string" && HEX_RE.test(s.color)) return s.color.toLowerCase();
  if (!state.theme.pastel) return state.theme.accent;
  return PASTEL[s.id] || PASTEL_FALLBACK;
}

/** Sets the accent used under `node` (the whole page, or just the HUD). */
function setAccent(node, color) {
  node.style.setProperty("--accent", color);
  // White on accent, except for light accents (pastels, yellow).
  node.style.setProperty("--on-accent", relLuminance(color) > 0.6 ? "#141414" : "#fff");
  if (node === document.documentElement) el.glow.setAttribute("flood-color", color);
}

// ---------- Wheel ----------

function footerText() {
  const { via, autoConfirmMs } = state;
  // Actions (shortcuts, typing, opening things) never run by just pausing on
  // them; sub-wheels open like modes are picked.
  const kind = state.slices[state.selected]?.kind || "mode";
  const verb = kind === "action" ? "run" : kind === "wheel" ? "open" : "pick";
  if (via === "modifier") {
    const key = state.theme.platform === "macos" ? "⌘" : "Win";
    return `turn to choose · release ${key} to ${verb} · esc to cancel`;
  }
  if (via === "hold") return `turn, then release to ${verb} · esc to cancel`;
  const parts = ["turn to choose"];
  if (kind === "action") {
    parts.push("tap to run");
  } else {
    if (autoConfirmMs > 0) parts.push(`${verb}s after ${fmtSeconds(autoConfirmMs)}s`);
    parts.push(`tap to ${verb} now`);
  }
  parts.push("esc to cancel");
  return parts.join(" · ");
}

function renderFooter() {
  el.footer.textContent = state.slices.length ? footerText() : "";
}

function build() {
  const n = state.slices.length;
  stopHighlight();
  if (n === 0) {
    el.slices.innerHTML = "";
    el.items.innerHTML = "";
    el.hl.classList.add("none");
    el.dot.classList.add("none");
    renderHub();
    renderFooter();
    return;
  }

  const step = 360 / n;
  const half = step / 2 - GAP_DEG;
  let slices = "";
  let items = "";
  state.slices.forEach((s, i) => {
    const c = i * step;
    slices += `<path class="slice" d="${sectorPath(R_SLICE_IN, R_SLICE_OUT, c - half, c + half, CORNER)}"/>`;
    const [x, y] = xy(R_ICON, c).map(Math.round);
    items +=
      `<g class="item" data-i="${i}">` +
      iconSvg(s.id, x, y, ICON_SIZE, ICON_STROKE, "item-icon", s.icon, s.kind) +
      (s.kind === "wheel" ? moreMarker(x + 19, y - 15) : "") +
      `<text class="item-label" x="${x}" y="${y + LABEL_DY}" text-anchor="middle">${esc(s.name)}</text>` +
      `</g>`;
  });
  el.slices.innerHTML = slices;
  el.items.innerHTML = items;

  // Labels: shrink long names to the slice width (semibold when selected, so leave room).
  const maxW = Math.min(124, 2 * R_ICON * Math.sin(rad(half)) - 12) * 0.94;
  el.items.querySelectorAll(".item-label").forEach((t) => fitText(t, maxW, 12, 9.5));

  el.hl.setAttribute("d", sectorPath(R_SLICE_IN, R_SEL_OUT, -half, half, CORNER));
  el.hl.classList.remove("none");
  hlTarget = state.selected * step;
  setHighlight(hlTarget);

  if (state.active == null) {
    el.dot.classList.add("none");
  } else {
    const [dx, dy] = xy(R_DOT, state.active * step);
    el.dot.setAttribute("cx", n3(dx));
    el.dot.setAttribute("cy", n3(dy));
    el.dot.classList.remove("none");
  }

  renderSelection();
  renderFooter();
}

/** Small "opens more" badge (three dots) beside a sub-wheel's icon. */
function moreMarker(x, y) {
  return (
    `<g class="more" transform="translate(${x} ${y})">` +
    `<circle class="more-bg" r="8"/>` +
    `<circle r="1.35" cx="-3.4"/><circle r="1.35"/><circle r="1.35" cx="3.4"/></g>`
  );
}

function renderSelection() {
  const sel = state.selected;
  setAccent(document.documentElement, colorFor(state.slices[sel]));
  el.items.querySelectorAll(".item").forEach((g, i) => g.classList.toggle("sel", i === sel));
  el.dot.classList.toggle("on-sel", state.active === sel);
  renderHub();
}

function renderHub() {
  const s = state.slices[state.selected];
  el.hubHint.innerHTML = "";
  if (!s) {
    el.hubIcon.innerHTML = "";
    el.hubName.textContent = "";
    return;
  }
  el.hubIcon.innerHTML = iconSvg(s.id, 0, HUB_ICON_Y, HUB_ICON_SIZE, HUB_ICON_STROKE, "", s.icon, s.kind);
  el.hubName.textContent = s.name;
  fitText(el.hubName, 176, 20, 15);

  const lines = s.hint.split("\n").map((l) => l.trim()).filter(Boolean).slice(0, HUB_HINT_MAX_LINES);
  lines.forEach((line, k) => {
    const y = HUB_HINT_Y + k * HUB_HINT_LH;
    const t = document.createElementNS("http://www.w3.org/2000/svg", "text");
    t.setAttribute("x", "0");
    t.setAttribute("y", String(y));
    t.setAttribute("text-anchor", "middle");
    t.textContent = line;
    el.hubHint.appendChild(t);
    fitText(t, 2 * Math.sqrt(106 * 106 - y * y) - 28, 11, 9);
  });
}

function setHighlight(a) {
  hlAngle = a;
  el.hl.setAttribute("transform", `rotate(${n3(a)})`);
}

function stopHighlight() {
  if (hlRaf) cancelAnimationFrame(hlRaf);
  hlRaf = 0;
}

/**
 * Slides the highlight to `index` the short way round. Exponential smoothing
 * toward the target, so rapid selects retarget mid-flight without restarting
 * and the motion doesn't depend on the frame rate.
 */
function moveHighlight(index, animate) {
  const n = state.slices.length;
  if (!n) return;
  const want = (index * 360) / n;
  const delta = ((((want - hlTarget) % 360) + 540) % 360) - 180;
  hlTarget += delta;
  if (!animate || reducedMotion.matches) {
    stopHighlight();
    setHighlight(hlTarget);
    return;
  }
  if (!hlRaf) {
    hlLast = 0;
    hlRaf = requestAnimationFrame(tickHighlight);
  }
}

function tickHighlight(now) {
  const dt = hlLast ? Math.min(50, Math.max(0, now - hlLast)) : 16;
  hlLast = now;
  const diff = hlTarget - hlAngle;
  if (Math.abs(diff) < 0.05) {
    setHighlight(hlTarget);
    hlRaf = 0;
    return;
  }
  setHighlight(hlAngle + diff * (1 - Math.exp(-dt / SELECT_TAU)));
  hlRaf = requestAnimationFrame(tickHighlight);
}

// ---------- Events ----------

function onOpen(p) {
  const slices = Array.isArray(p.slices) ? p.slices : [];
  state.slices = slices.map((s) => ({
    id: String(s?.id ?? ""),
    name: String(s?.name ?? s?.id ?? ""),
    hint: String(s?.hint ?? ""),
    kind: ["mode", "action", "wheel"].includes(s?.kind) ? s.kind : "mode",
    icon: typeof s?.icon === "string" ? s.icon : null,
    color: typeof s?.color === "string" ? s.color : null,
  }));
  const n = state.slices.length;
  state.wheel = String(p.wheel ?? "");
  state.nested = p.nested === true;
  el.titleText.textContent = state.nested ? state.wheel : "";
  el.title.classList.toggle("on", state.nested && state.wheel !== "");
  if (PREVIEW) place();
  state.selected = toIndex(p.selected, n);
  const a = p.active == null ? null : Number(p.active);
  state.active = a != null && Number.isInteger(a) && a >= 0 && a < n ? a : null;
  state.via = ["tap", "hold", "modifier"].includes(p.via) ? p.via : "tap";
  state.autoConfirmMs = Math.max(0, Number(p.auto_confirm_ms) || 0);

  el.wheel.dataset.wheel = state.wheel;
  build();
  // Already open: rebuilt in place. Closed or closing: fades (back) in.
  state.open = true;
  place();
  el.wheel.classList.add("open");
  el.dim.classList.toggle("on", state.theme.dim && !PREVIEW);
}

function onSelect(p) {
  const n = state.slices.length;
  if (!n) return;
  const i = toIndex(p.selected, n);
  if (i === state.selected) return;
  state.selected = i;
  renderSelection();
  renderFooter();
  moveHighlight(i, state.open);
}

function onClose() {
  state.open = false;
  stopHighlight();
  setHighlight(hlTarget);
  el.wheel.classList.remove("open");
  el.dim.classList.remove("on");
}

function onHud(p) {
  const mode = String(p.mode ?? "");
  const icon = typeof p.icon === "string" ? p.icon : null;
  setAccent(el.hud, colorFor({ id: mode, color: p.color }));
  // a custom slice without an icon gets the action default
  el.hudIcon.innerHTML = iconShapes(mode, icon, "action");
  el.hudText.textContent = String(p.text ?? "");
  el.hud.classList.add("show");
  clearTimeout(hudTimer);
  if (!state.hudPinned) {
    hudTimer = setTimeout(() => el.hud.classList.remove("show"), HUD_IN_MS + HUD_HOLD_MS);
  }
}

function handle(raw) {
  const p = parsePayload(raw);
  if (!p || typeof p !== "object") return;
  try {
    switch (p.type) {
      case "open":
        onOpen(p);
        break;
      case "select":
        onSelect(p);
        break;
      case "close":
        onClose();
        break;
      case "hud":
        onHud(p);
        break;
      case "theme":
        if (PREVIEW) applyTheme(p);
        break;
      default:
        break;
    }
  } catch (err) {
    console.error("slobknob: failed to handle", p, err);
  }
}

// ---------- Boot ----------

async function initTauri(T) {
  try {
    await Promise.all([
      T.event.listen("knob-ui", (e) => handle(e.payload)),
      T.event.listen("knob-theme", (e) => applyTheme(e.payload)),
    ]);
  } catch (err) {
    console.error("slobknob: listen failed", err);
  }
  try {
    applyTheme(await T.core.invoke("ui_ready"));
  } catch (err) {
    console.error("slobknob: ui_ready failed", err);
  }
}

const DEMO_SLICES = [
  { id: "volume", name: "Volume", hint: "turn: volume\ntap: mute" },
  { id: "brightness", name: "Brightness", hint: "turn: screen brightness" },
  { id: "scroll", name: "Scroll", hint: "turn: scroll\ntap: switch axis" },
  { id: "zoom", name: "Zoom", hint: "turn: zoom in / out\ntap: reset" },
  { id: "apps", name: "Apps", hint: "turn: switch app\ntap: pick" },
  { id: "tabs", name: "Tabs", hint: "turn: switch tab" },
  { id: "spaces", name: "Desktops", hint: "turn: switch desktop\ntap: overview" },
  { id: "media", name: "Media", hint: "turn: skip track\ntap: play / pause" },
  { id: "seek", name: "Seek", hint: "turn: seek\ntap: play / pause" },
];

const DEMO_EXTRA = [
  { id: "windows", name: "Windows", hint: "turn: switch window" },
  { id: "drag", name: "Drag", hint: "turn: drag\ntap: switch axis" },
  { id: "jog", name: "Jog", hint: "turn: frame by frame" },
  { id: "edits", name: "Edits", hint: "turn: jump to edit" },
  { id: "undo", name: "Undo", hint: "turn: undo / redo" },
];

// ?custom=1: some action and sub-wheel slices with their own icon and color.
const DEMO_CUSTOM = [
  { id: "volume", name: "Volume", hint: "turn: volume\ntap: mute" },
  { id: "term", name: "Terminal", hint: "tap: open\nGhostty", kind: "action", icon: "terminal" },
  { id: "reopen", name: "Reopen tab", hint: "tap: press\ncmd+shift+t", kind: "action", icon: "rotate-ccw", color: "#ffd6a5" },
  { id: "dev", name: "Dev", hint: "opens 4 more", kind: "wheel", icon: "code" },
  { id: "scroll", name: "Scroll", hint: "turn: scroll\ntap: switch axis" },
  { id: "sig", name: "Signature", hint: "tap: type text", kind: "action" },
];

function demoBackground() {
  const line = (w, cls = "") => `<div class="demo-line ${cls}" style="width:${w}%"></div>`;
  const bg = document.createElement("div");
  bg.className = "demo-bg";
  bg.innerHTML =
    `<div class="demo-app"><div class="demo-title"><i></i><i></i><i></i>` +
    `<span>SlobKnob demo · ← → select · return pick · esc close · o open</span></div>` +
    `<div class="demo-body"><div class="demo-side">` +
    [70, 55, 80, 62, 48, 75, 58, 66, 40].map((w, i) => line(w, i === 2 ? "a" : "")).join("") +
    `</div><div class="demo-main">` +
    line(38, "h") +
    [96, 88, 92, 60].map((w) => line(w)).join("") +
    `<div class="demo-cards"><div class="demo-card"></div><div class="demo-card"></div><div class="demo-card"></div></div>` +
    [90, 94, 85, 70, 92, 50].map((w) => line(w)).join("") +
    `</div></div></div>`;
  document.body.prepend(bg);
}

function runDemo() {
  const q = new URLSearchParams(location.search);
  demoBackground();

  const accent = q.get("accent");
  applyTheme({
    accent: accent ? (accent === "pastel" ? "pastel" : `#${accent.replace(/^#/, "")}`) : "pastel",
    dim: q.get("dim") !== "0",
    platform: q.get("platform") || guessPlatform(),
  });

  const count = Math.max(1, Math.min(14, Number(q.get("n")) || 9));
  const slices = q.get("custom") === "1" ? DEMO_CUSTOM : [...DEMO_SLICES, ...DEMO_EXTRA].slice(0, count);
  const nested = q.get("nested") === "1";
  let active = q.has("active") ? (q.get("active") === "none" ? null : Number(q.get("active"))) : 0;
  let sel = Number(q.get("sel")) || 0;
  const via = q.get("via") || "tap";
  const auto = q.has("auto") ? Number(q.get("auto")) : via === "modifier" ? 0 : 600;

  const open = () =>
    handle({ type: "open", wheel: nested ? "Dev" : "main", nested, slices, selected: sel, active, via, auto_confirm_ms: auto });

  if (q.get("hud") === "1") {
    state.hudPinned = true;
    const s = slices[toIndex(sel, slices.length)];
    handle({ type: "hud", mode: s.id, text: s.name, icon: s.icon, color: s.color });
  } else {
    open();
  }

  window.addEventListener("keydown", (e) => {
    const n = slices.length;
    if (e.key === "ArrowRight" || e.key === "ArrowDown") {
      if (!state.open) return;
      sel = (state.selected + 1) % n;
      handle({ type: "select", selected: sel });
    } else if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
      if (!state.open) return;
      sel = (state.selected - 1 + n) % n;
      handle({ type: "select", selected: sel });
    } else if (e.key === "Enter" || e.key === " ") {
      if (!state.open) return;
      const s = slices[state.selected];
      active = state.selected;
      state.hudPinned = false;
      handle({ type: "close" });
      handle({ type: "hud", mode: s.id, text: s.name, icon: s.icon, color: s.color });
    } else if (e.key === "Escape") {
      handle({ type: "close" });
    } else if (e.key === "o") {
      open();
    } else {
      return;
    }
    e.preventDefault();
  });
}

window.addEventListener("resize", place);
place();
applyTheme({});

// Debug hook: slobknob.handle({type:"select", selected:3}) from devtools.
window.slobknob = { handle, applyTheme };

function initPreview() {
  document.documentElement.classList.add("preview");
  window.addEventListener("message", (e) => {
    // only the settings window that embeds us
    if (e.source !== window.parent || window.parent === window) return;
    handle(e.data);
  });
  window.parent.postMessage({ type: "preview-ready" }, "*");
}

if (PREVIEW) {
  initPreview();
} else if (window.__TAURI__?.event && window.__TAURI__?.core) {
  initTauri(window.__TAURI__);
} else {
  runDemo();
}
