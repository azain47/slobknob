// SlobKnob settings window. Edits its own copy of config.json (as written,
// not sanitized, so half-finished actions survive autosave) and saves it
// through `save_settings`, which applies it right away.

import { ICON_NAMES, MODE_ICONS, PASTEL, PASTEL_FALLBACK, namedIconShapes, resolveIcon } from "./icons.js";

const T = window.__TAURI__;
const invoke = (cmd, args) => T.core.invoke(cmd, args);

const $ = (id) => document.getElementById(id);
const HEX_RE = /^#[0-9a-f]{6}$/i;
const SAVE_DEBOUNCE_MS = 450;

const SWATCHES = ["#f5adb9", "#f5cbad", "#f5e3ad", "#c5f5ad", "#adf5dd", "#ade9f5", "#adcbf5", "#b9adf5", "#ddadf5", "#f5addd"];

const ACCENT_PRESETS = ["#0a84ff", "#5e5ce6", "#bf5af2", "#ff375f", "#ff9f0a", "#30d158", "#64d2ff"];

const MEDIA_KEYS = [
  ["play_pause", "Play / pause"],
  ["next", "Next track"],
  ["previous", "Previous track"],
  ["mute", "Mute"],
  ["volume_up", "Volume up"],
  ["volume_down", "Volume down"],
  ["brightness_up", "Brightness up"],
  ["brightness_down", "Brightness down"],
];
const MEDIA_LABEL = Object.fromEntries(MEDIA_KEYS);

const ACTION_TYPES = [
  ["keys", "Shortcut", "keyboard"],
  ["open", "Open", "rocket"],
  ["text", "Type text", "type"],
  ["media", "Media key", "play"],
  ["wheel", "Sub-wheel", "layers"],
];

const TABS = [
  ["wheels", "Wheels", "layout-grid"],
  ["actions", "Actions", "zap"],
  ["knob", "Knob", "rotate-ccw"],
  ["general", "General", "settings"],
  ["scroll", "Scroll", "arrow-up-down"],
];

// ---------- State ----------

let data = null; // get_settings result
let cfg = null; // the config being edited
let saveTimer = 0;
let saving = false;
let saveAgain = false;
let saveError = "";
let problems = [];
let theme = null; // resolved theme for the preview
let blockedByParseError = false;

const ui = {
  tab: "wheels",
  wheel: "default", // "default" or an index into app_wheels (as a string)
  action: null, // selected action id
  previewSel: 0,
  learn: { state: "idle", step: null, keys: null, device: null, error: "" },
  perms: null,
  recording: null, // {input, append}
  newIds: new Set(), // actions created in this session: id still follows the name
};

const modeById = () => Object.fromEntries(data.modes.map((m) => [m.id, m]));

// ---------- DOM helpers ----------

function h(tag, props, ...children) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v == null || v === false) continue;
    if (k === "class") e.className = v;
    else if (k === "style") e.style.cssText = v;
    else if (k === "html") e.innerHTML = v;
    else if (k.startsWith("on") && typeof v === "function") e.addEventListener(k.slice(2).toLowerCase(), v);
    else if (k === "value" || k === "checked" || k === "disabled" || k === "selected") e[k] = v;
    else e.setAttribute(k, v === true ? "" : String(v));
  }
  for (const c of children.flat(Infinity)) {
    if (c == null || c === false) continue;
    e.append(c instanceof Node ? c : String(c));
  }
  return e;
}

function icon(name, size = 16, stroke = 2, cls = "ic") {
  return h("span", {
    class: cls,
    "aria-hidden": "true",
    html:
      `<svg width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" ` +
      `stroke-width="${stroke}" stroke-linecap="round" stroke-linejoin="round">${namedIconShapes(name)}</svg>`,
  });
}

function relLuminance(hex) {
  const c = [1, 3, 5].map((i) => {
    const v = parseInt(hex.slice(i, i + 2), 16) / 255;
    return v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
}

const onColor = (hex) => (relLuminance(hex) > 0.5 ? "#18181b" : "#fff");

// ---------- Slices ----------

function actionHint(a) {
  switch (a.type) {
    case "keys":
      return `tap: press\n${a.keys ?? ""}`;
    case "open":
      return `tap: open\n${a.target ?? ""}`;
    case "text":
      return "tap: type text";
    case "media":
      return `tap: ${(MEDIA_LABEL[a.key] || a.key || "").toLowerCase()}`;
    case "wheel":
      return `opens ${(a.slices || []).length} more`;
    default:
      return "";
  }
}

function findAction(id) {
  return cfg.actions.find((a) => a.id === id);
}

/** A slice as the overlay gets it (UiEvent Slice), or null for an unknown id. */
function sliceInfo(id) {
  const m = modeById()[id];
  if (m) return { id, name: m.name, hint: m.hint, kind: "mode", icon: null, color: null };
  const a = findAction(id);
  if (!a) return null;
  return {
    id,
    name: a.name || a.id,
    hint: actionHint(a),
    kind: a.type === "wheel" ? "wheel" : "action",
    icon: a.icon || null,
    color: a.color || null,
  };
}

function accentHex() {
  if (cfg.accent === "system") return data.system_accent;
  if (HEX_RE.test(cfg.accent)) return cfg.accent;
  return null; // pastel
}

function sliceColor(s) {
  if (s.color && HEX_RE.test(s.color)) return s.color;
  return accentHex() || PASTEL[s.id] || PASTEL_FALLBACK;
}

function badge(s, size = 30) {
  const c = sliceColor(s);
  const b = h("span", { class: "badge", style: `--c:${c};--on:${onColor(c)};width:${size}px;height:${size}px` });
  b.append(icon(resolveIcon(s.id, s.icon, s.kind), Math.round(size * 0.53), 2));
  return b;
}

// ---------- Saving ----------

function changed({ render = false } = {}) {
  if (render) renderContent();
  updatePreview();
  scheduleSave();
}

function scheduleSave() {
  if (blockedByParseError) {
    setSaveStatus("blocked");
    return;
  }
  clearTimeout(saveTimer);
  setSaveStatus("pending");
  saveTimer = setTimeout(saveNow, SAVE_DEBOUNCE_MS);
}

async function saveNow() {
  clearTimeout(saveTimer);
  saveTimer = 0;
  if (saving) {
    saveAgain = true;
    return;
  }
  saving = true;
  setSaveStatus("saving");
  try {
    const res = await invoke("save_settings", { config: cfg });
    saveError = res.autostart_error || "";
    problems = res.problems;
    theme = res.theme;
    sendTheme();
    renderBanners();
    markProblemActions();
    setSaveStatus(saveError ? "error" : "saved");
  } catch (err) {
    saveError = String(err);
    setSaveStatus("error");
  } finally {
    saving = false;
    if (saveAgain) {
      saveAgain = false;
      saveNow();
    }
  }
}

function setSaveStatus(s) {
  const el = $("save-status");
  el.className = `save-status ${s}`;
  el.replaceChildren();
  const text = {
    pending: "Editing…",
    saving: "Saving…",
    saved: "All changes saved",
    error: `Couldn't save: ${saveError}`,
    blocked: "Not saved: config.json has errors",
  }[s];
  if (s === "saved") el.append(icon("circle-check", 13));
  el.append(h("span", { title: text }, text));
}

// ---------- Banners (problems, parse error) ----------

function renderBanners() {
  const box = $("banners");
  box.replaceChildren();
  if (blockedByParseError) {
    box.append(
      h(
        "div",
        { class: "banner bad" },
        h("div", { class: "banner-title" }, "config.json couldn't be read"),
        h("div", { class: "banner-body mono" }, data.parse_error),
        h(
          "div",
          { class: "banner-body" },
          "SlobKnob is using the last working settings. Fix the file by hand, or replace it with what you see here.",
        ),
        h(
          "div",
          { class: "banner-actions" },
          h("button", { class: "btn", type: "button", onclick: () => invoke("open_config_file") }, "Open config file"),
          h(
            "button",
            {
              class: "btn primary",
              type: "button",
              onclick: () => {
                blockedByParseError = false;
                renderBanners();
                saveNow();
              },
            },
            "Replace with these settings",
          ),
        ),
      ),
    );
  }
  if (problems.length) {
    box.append(
      h(
        "div",
        { class: "banner warn" },
        h("div", { class: "banner-title" }, icon("bell", 14), problems.length === 1 ? "1 thing to fix" : `${problems.length} things to fix`),
        h("ul", { class: "banner-list" }, problems.map((p) => h("li", null, p))),
        h("div", { class: "banner-body dim" }, "Saved anyway. Until they're fixed, SlobKnob skips these entries."),
      ),
    );
  }
}

/** Refresh the "needs attention" dots in the action list without re-rendering. */
function markProblemActions() {
  document.querySelectorAll(".action-item").forEach((el) => {
    const id = el.dataset.id;
    const bad = problems.some((p) => p.includes(`"${id}"`));
    el.querySelector(".warn-dot")?.classList.toggle("none", !bad);
  });
}

// ---------- Navigation ----------

function renderNav() {
  const nav = $("nav");
  nav.replaceChildren(
    ...TABS.map(([id, name, ic]) =>
      h(
        "button",
        { class: `nav-item${ui.tab === id ? " on" : ""}`, type: "button", "data-tab": id, onclick: () => showTab(id) },
        icon(ic, 16),
        h("span", null, name),
        id === "actions" && cfg.actions.length ? h("span", { class: "nav-count" }, cfg.actions.length) : null,
      ),
    ),
  );
}

function showTab(tab) {
  if (!TABS.some(([id]) => id === tab)) return;
  if (ui.tab === "knob" && tab !== "knob" && ui.learn.state === "learning") cancelLearn();
  ui.tab = tab;
  renderNav();
  renderContent();
  $("scroller").scrollTop = 0;
  if (tab === "knob") refreshPermissions();
}

function renderContent() {
  const title = TABS.find(([id]) => id === ui.tab)[1];
  $("title").textContent = title;
  const content = $("content");
  const scroll = $("scroller").scrollTop;
  const render = { wheels: renderWheels, actions: renderActions, knob: renderKnob, general: renderGeneral, scroll: renderScroll }[ui.tab];
  // keep the preview iframe alive across re-renders of its tab
  const keep = content.querySelector(".preview-frame");
  content.replaceChildren(render());
  const slot = content.querySelector(".preview-slot");
  if (slot && keep) slot.replaceWith(keep);
  else if (slot) slot.replaceWith(makePreviewFrame());
  $("scroller").scrollTop = scroll;
  updatePreview();
}

// ---------- Form controls ----------

function row(label, desc, control, opts = {}) {
  return h(
    "div",
    { class: `row${opts.disabled ? " disabled" : ""}${opts.sub ? " sub" : ""}` },
    h("div", { class: "row-text" }, h("div", { class: "row-label" }, label), desc ? h("div", { class: "row-desc" }, desc) : null),
    h("div", { class: "row-control" }, control),
  );
}

function toggle(value, onchange, disabled = false) {
  const input = h("input", {
    type: "checkbox",
    role: "switch",
    checked: !!value,
    disabled,
    onchange: (e) => onchange(e.target.checked),
  });
  return h("label", { class: `switch${disabled ? " disabled" : ""}` }, input, h("span", { class: "switch-track" }, h("span", { class: "switch-thumb" })));
}

function segmented(options, value, onchange) {
  return h(
    "div",
    { class: "seg", role: "radiogroup" },
    options.map(([v, label, ic]) =>
      h(
        "button",
        {
          type: "button",
          role: "radio",
          "aria-checked": String(v === value),
          class: `seg-item${v === value ? " on" : ""}`,
          onclick: () => v !== value && onchange(v),
        },
        ic ? icon(ic, 14) : null,
        label,
      ),
    ),
  );
}

/** Range slider with a formatted value. */
function slider({ value, min, max, step = 1, format, onchange, width = 180 }) {
  const out = h("span", { class: "slider-value" }, format(value));
  const input = h("input", {
    type: "range",
    min,
    max,
    step,
    value: String(value),
    style: `width:${width}px`,
    oninput: (e) => {
      const v = Number(e.target.value);
      out.textContent = format(v);
      paint(e.target);
      onchange(v);
    },
  });
  const paint = (el) => {
    const pct = ((Number(el.value) - min) / (max - min)) * 100;
    el.style.setProperty("--pct", `${pct}%`);
  };
  paint(input);
  return h("div", { class: "slider" }, input, out);
}

function numberField({ value, min, max, step = 1, unit, onchange, width = 76 }) {
  const input = h("input", {
    class: "input num",
    type: "number",
    min,
    max,
    step,
    value: String(value),
    style: `width:${width}px`,
    onchange: (e) => {
      let v = Number(e.target.value);
      if (!Number.isFinite(v)) v = min;
      v = Math.min(max, Math.max(min, step < 1 ? v : Math.round(v)));
      e.target.value = String(v);
      onchange(v);
    },
  });
  return h("div", { class: "num-wrap" }, input, unit ? h("span", { class: "unit" }, unit) : null);
}

const ms = (v) => (v >= 1000 ? `${(v / 1000).toFixed(v % 1000 ? 1 : 0)} s` : `${v} ms`);

// ---------- Popover ----------

let closePopover = null;

function openPopover(anchor, content, { width = 300, align = "left" } = {}) {
  closePopover?.();
  const layer = $("popover-layer");
  const pop = h("div", { class: "popover", style: `width:${width}px` }, content);
  layer.append(pop);
  anchor.scrollIntoView({ block: "nearest" });
  const r = anchor.getBoundingClientRect();
  const vw = window.innerWidth;
  const vh = window.innerHeight;
  let left = align === "right" ? r.right - width : r.left;
  left = Math.max(12, Math.min(vw - width - 12, left));
  pop.style.left = `${left}px`;
  const ph = pop.offsetHeight;
  const below = r.bottom + 6;
  const top = below + ph > vh - 12 && r.top - ph - 6 > 12 ? r.top - ph - 6 : below;
  pop.style.top = `${Math.max(12, Math.min(top, vh - ph - 12))}px`;
  const onDown = (e) => {
    if (!pop.contains(e.target) && !anchor.contains(e.target)) close();
  };
  const onKey = (e) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      close();
    }
  };
  function close() {
    pop.remove();
    document.removeEventListener("pointerdown", onDown, true);
    document.removeEventListener("keydown", onKey, true);
    closePopover = null;
  }
  setTimeout(() => {
    document.addEventListener("pointerdown", onDown, true);
    document.addEventListener("keydown", onKey, true);
  });
  closePopover = close;
  pop.querySelector("input")?.focus();
  return close;
}

// ---------- Slice list editor ----------

/**
 * Editable list of slice ids (a wheel or a sub-wheel). `exclude(id)` hides
 * ids that can't be added (e.g. a sub-wheel inside itself).
 */
function sliceList(list, { exclude = () => false, onSelect, selected = -1, emptyText = "No slices yet." } = {}) {
  const wrap = h("div", { class: "slices" });
  const ul = h("div", { class: "slice-list", role: "list" });

  const rerender = () => {
    const fresh = sliceList(list, { exclude, onSelect, selected: ui.previewSel, emptyText });
    wrap.replaceWith(fresh);
  };

  const commit = () => {
    changed();
    rerender();
  };

  list.forEach((id, i) => {
    const s = sliceInfo(id);
    const known = !!s;
    const info = s || { id, name: id, hint: "", kind: "mode" };
    const kindLabel = !known ? "Unknown" : info.kind === "mode" ? "Mode" : info.kind === "wheel" ? "Sub-wheel" : "Action";
    const firstHint = info.hint.split("\n").filter(Boolean).join(" · ");
    const handle = h("button", { class: "grip", type: "button", "aria-label": `Drag to reorder ${info.name}`, title: "Drag to reorder" }, h("span"));
    const r = h(
      "div",
      {
        class: `slice-row${i === selected ? " sel" : ""}${known ? "" : " unknown"}`,
        role: "listitem",
        tabindex: "0",
        onclick: (e) => {
          if (e.target.closest("button")) return;
          onSelect?.(i);
          ul.querySelectorAll(".slice-row").forEach((el, k) => el.classList.toggle("sel", k === i));
        },
        onkeydown: (e) => {
          if (e.altKey && (e.key === "ArrowUp" || e.key === "ArrowDown")) {
            e.preventDefault();
            move(i, e.key === "ArrowUp" ? -1 : 1, true);
          } else if (e.key === "Backspace" || e.key === "Delete") {
            e.preventDefault();
            remove(i);
          }
        },
      },
      handle,
      h("span", { class: "slice-pos" }, i + 1),
      badge(info, 28),
      h(
        "div",
        { class: "slice-text" },
        h("div", { class: "slice-name" }, h("span", { class: "slice-name-text" }, info.name), info.kind === "wheel" ? icon("ellipsis", 13, 2.5, "ic more-ic") : null),
        h(
          "div",
          { class: "slice-hint" },
          known && info.kind === "mode" ? null : h("span", { class: `kind ${known ? info.kind : "bad"}` }, kindLabel),
          known ? firstHint : "Not a mode or action. Remove it, or create the action.",
        ),
      ),
      h(
        "div",
        { class: "slice-btns" },
        h("button", { class: "icon-btn", type: "button", title: "Move up", disabled: i === 0, onclick: () => move(i, -1) }, icon("chevron-up", 14)),
        h(
          "button",
          { class: "icon-btn", type: "button", title: "Move down", disabled: i === list.length - 1, onclick: () => move(i, 1) },
          icon("chevron-down", 14),
        ),
        h("button", { class: "icon-btn danger", type: "button", title: "Remove", onclick: () => remove(i) }, icon("x", 14)),
      ),
    );
    handle.addEventListener("pointerdown", (e) => startDrag(e, r, i));
    ul.append(r);
  });

  function move(i, d, refocus = false) {
    const j = i + d;
    if (j < 0 || j >= list.length) return;
    [list[i], list[j]] = [list[j], list[i]];
    if (ui.previewSel === i) ui.previewSel = j;
    else if (ui.previewSel === j) ui.previewSel = i;
    commit();
    if (refocus) document.querySelectorAll(".slice-row")[j]?.focus();
  }

  function remove(i) {
    list.splice(i, 1);
    ui.previewSel = Math.max(0, Math.min(ui.previewSel, list.length - 1));
    commit();
  }

  function startDrag(e, rowEl, startIndex) {
    if (e.button !== 0) return;
    e.preventDefault();
    const target = e.currentTarget;
    target.setPointerCapture(e.pointerId);
    let index = startIndex;
    let originY = e.clientY;
    const gap = 6;
    rowEl.classList.add("dragging");
    ul.classList.add("is-dragging");
    const onMove = (ev) => {
      let dy = ev.clientY - originY;
      const next = rowEl.nextElementSibling;
      const prev = rowEl.previousElementSibling;
      if (next && dy > next.offsetHeight / 2 + gap / 2) {
        next.after(rowEl);
        [list[index], list[index + 1]] = [list[index + 1], list[index]];
        index += 1;
        originY += next.offsetHeight + gap;
        dy = ev.clientY - originY;
        updatePreview();
      } else if (prev && dy < -(prev.offsetHeight / 2 + gap / 2)) {
        prev.before(rowEl);
        [list[index], list[index - 1]] = [list[index - 1], list[index]];
        index -= 1;
        originY -= prev.offsetHeight + gap;
        dy = ev.clientY - originY;
        updatePreview();
      }
      rowEl.style.transform = `translateY(${dy}px)`;
    };
    const onUp = () => {
      target.removeEventListener("pointermove", onMove);
      target.removeEventListener("pointerup", onUp);
      target.removeEventListener("pointercancel", onUp);
      rowEl.style.transform = "";
      rowEl.classList.remove("dragging");
      ul.classList.remove("is-dragging");
      if (index !== startIndex) {
        ui.previewSel = index;
        commit();
      }
    };
    target.addEventListener("pointermove", onMove);
    target.addEventListener("pointerup", onUp);
    target.addEventListener("pointercancel", onUp);
  }

  if (!list.length) ul.append(h("div", { class: "empty" }, emptyText));

  const addBtn = h("button", { class: "btn add-btn", type: "button" }, icon("plus", 14), "Add slice");
  addBtn.addEventListener("click", () => openSlicePicker(addBtn, list, exclude, (id) => {
    list.push(id);
    ui.previewSel = list.length - 1;
    onSelect?.(ui.previewSel);
    commit();
  }));

  wrap.append(ul, h("div", { class: "slices-foot" }, addBtn, h("span", { class: "foot-note" }, list.length ? `${list.length} slice${list.length === 1 ? "" : "s"} · clockwise from the top` : "")));
  return wrap;
}

function openSlicePicker(anchor, list, exclude, onPick) {
  const search = h("input", { class: "input search", type: "search", placeholder: "Search modes and actions", spellcheck: "false" });
  const body = h("div", { class: "picker-body" });
  const render = () => {
    const q = search.value.trim().toLowerCase();
    const match = (s) => !q || s.name.toLowerCase().includes(q) || s.id.toLowerCase().includes(q);
    const item = (s) => {
      const used = list.includes(s.id);
      return h(
        "button",
        {
          class: `pick-item${used ? " used" : ""}`,
          type: "button",
          title: used ? "Already on this wheel" : s.hint.replace("\n", " · "),
          onclick: () => {
            close();
            onPick(s.id);
          },
        },
        badge(s, 26),
        h("span", { class: "pick-name" }, s.name),
        used ? h("span", { class: "pick-used" }, "on wheel") : null,
      );
    };
    const modes = data.modes.map((m) => sliceInfo(m.id)).filter(match);
    const actions = cfg.actions.filter((a) => a.id && !exclude(a.id)).map((a) => sliceInfo(a.id)).filter(match);
    body.replaceChildren(
      h("div", { class: "pick-group" }, "Modes"),
      h("div", { class: "pick-grid" }, modes.map(item)),
      h("div", { class: "pick-group" }, "Actions & sub-wheels"),
      actions.length
        ? h("div", { class: "pick-grid" }, actions.map(item))
        : h("div", { class: "pick-empty" }, q ? "No matching actions." : "No custom actions yet. Make one in Actions."),
    );
  };
  search.addEventListener("input", render);
  render();
  const close = openPopover(anchor, h("div", { class: "picker" }, search, body), { width: 340 });
}

// ---------- Live preview ----------

let previewFrame = null;
let previewReady = false;
let hudTimer = 0;

function makePreviewFrame() {
  if (!previewFrame) {
    previewFrame = h("iframe", { class: "preview-frame", src: "index.html?preview=1", title: "Wheel preview", tabindex: "-1" });
  }
  return previewFrame;
}

window.addEventListener("message", (e) => {
  if (previewFrame && e.source === previewFrame.contentWindow && e.data?.type === "preview-ready") {
    previewReady = true;
    lastPreviewKey = "";
    sendTheme();
    updatePreview();
  }
});

function post(msg) {
  if (!previewReady || !previewFrame?.isConnected) return false;
  previewFrame.contentWindow.postMessage(msg, "*");
  return true;
}

function sendTheme() {
  if (!theme) return;
  post({ type: "theme", accent: cfg.accent === "pastel" ? "pastel" : theme.accent, dim: false, platform: data.platform });
}

/** What the preview shows: {title, nested, list} for the current tab, or null. */
function previewModel() {
  if (ui.tab === "wheels") {
    if (ui.wheel === "default") return { title: "default", nested: false, list: cfg.wheel };
    const w = cfg.app_wheels[Number(ui.wheel)];
    return w ? { title: w.name, nested: false, list: w.modes } : null;
  }
  if (ui.tab === "actions") {
    const a = findAction(ui.action);
    if (a?.type === "wheel") return { title: a.name || a.id, nested: true, list: a.slices };
  }
  return null;
}

let lastPreviewKey = "";

function updatePreview() {
  const m = previewModel();
  const caption = document.querySelector(".preview-caption");
  if (!m) return;
  const slices = m.list.map(sliceInfo).filter(Boolean);
  ui.previewSel = Math.max(0, Math.min(ui.previewSel, slices.length - 1));
  sendTheme();
  const msg = {
    type: "open",
    wheel: m.title,
    nested: m.nested,
    slices,
    selected: ui.previewSel,
    active: null,
    via: "tap",
    auto_confirm_ms: cfg.auto_confirm_ms,
  };
  const key = JSON.stringify(msg);
  if (key !== lastPreviewKey && post(msg)) lastPreviewKey = key;
  if (caption) renderCaption(caption, slices[ui.previewSel]);
}

function renderCaption(el, s) {
  el.replaceChildren();
  if (!s) {
    el.append(h("span", { class: "dim" }, "Empty wheel: add a slice."));
    return;
  }
  const behaviour =
    s.kind === "action" ? "Tap to run · never auto-picks" : s.kind === "wheel" ? "Opens a sub-wheel" : cfg.auto_confirm_ms > 0 ? `Picks after ${ms(cfg.auto_confirm_ms)} or on tap` : "Tap to pick";
  el.append(h("div", { class: "cap-name" }, s.name), h("div", { class: "cap-desc" }, behaviour));
}

function previewSelect(i) {
  ui.previewSel = i;
  post({ type: "select", selected: i });
  const m = previewModel();
  const caption = document.querySelector(".preview-caption");
  if (m && caption) renderCaption(caption, sliceInfo(m.list[i]));
}

/** The real knob, while this window is focused. */
function onKnobPreview({ key, down }) {
  if (!down) return;
  const m = previewModel();
  if (!m) return;
  const n = m.list.map(sliceInfo).filter(Boolean).length;
  if (!n) return;
  if (key === "clockwise" || key === "counter_clockwise") {
    previewSelect((ui.previewSel + (key === "clockwise" ? 1 : -1) + n) % n);
    document.querySelectorAll(".slice-list .slice-row").forEach((el, k) => el.classList.toggle("sel", k === ui.previewSel));
  } else if (key === "press") {
    const s = sliceInfo(m.list.filter((id) => sliceInfo(id))[ui.previewSel]);
    if (!s) return;
    post({ type: "hud", mode: s.id, text: s.name, icon: s.icon, color: s.color });
    clearTimeout(hudTimer);
  }
  document.querySelector(".preview-card")?.classList.add("live");
}

function previewPanel(note) {
  return h(
    "div",
    { class: "preview-card" },
    h("div", { class: "preview-head" }, h("span", null, "Preview"), h("span", { class: "live-dot", title: "Turn the knob to try it" }, "knob live")),
    h("div", { class: "preview-box" }, h("div", { class: "preview-slot" })),
    h("div", { class: "preview-caption" }),
    h("div", { class: "preview-note" }, note),
  );
}

// ---------- Wheels ----------

function renderWheels() {
  const chips = h(
    "div",
    { class: "wheel-tabs" },
    h("button", { type: "button", class: `wheel-chip${ui.wheel === "default" ? " on" : ""}`, onclick: () => pickWheel("default") }, icon("layout-grid", 14), "Default"),
    cfg.app_wheels.map((w, i) =>
      h(
        "button",
        { type: "button", class: `wheel-chip${ui.wheel === String(i) ? " on" : ""}`, onclick: () => pickWheel(String(i)) },
        icon("app-window", 14),
        h("span", { class: "chip-text" }, w.name || "Untitled"),
      ),
    ),
    h("button", { type: "button", class: "wheel-chip add", title: "Add a wheel for an app", onclick: addAppWheel }, icon("plus", 14), "App wheel"),
  );

  let editor;
  if (ui.wheel === "default") {
    editor = h(
      "div",
      { class: "card" },
      h("div", { class: "card-head" }, h("div", null, h("div", { class: "card-title" }, "Default wheel"), h("div", { class: "card-sub" }, "Used everywhere without an app wheel."))),
      sliceList(cfg.wheel, { onSelect: previewSelect, selected: ui.previewSel, emptyText: "Empty. SlobKnob falls back to the built-in wheel." }),
    );
  } else {
    const i = Number(ui.wheel);
    const w = cfg.app_wheels[i];
    editor = appWheelEditor(w, i);
  }

  return h(
    "div",
    { class: "split" },
    h("div", { class: "split-main" }, chips, editor),
    h("div", { class: "split-side" }, previewPanel("Click a slice, or turn your knob: while this window is in front, the knob only moves the preview.")),
  );
}

function pickWheel(key) {
  ui.wheel = key;
  ui.previewSel = 0;
  renderContent();
}

function addAppWheel() {
  let name = "New app";
  for (let k = 2; cfg.app_wheels.some((w) => w.name === name); k++) name = `New app ${k}`;
  cfg.app_wheels.push({ name, matches: [], modes: ["volume", "scroll", "zoom", "media"] });
  ui.wheel = String(cfg.app_wheels.length - 1);
  ui.previewSel = 0;
  changed({ render: true });
  document.querySelector(".app-name")?.select();
}

function appWheelEditor(w, i) {
  const matchWrap = h("div", { class: "chips-input" });
  const renderMatches = () => {
    const input = h("input", {
      class: "chip-entry",
      placeholder: w.matches.length ? "Add another…" : platformAppHint(),
      spellcheck: "false",
      onkeydown: (e) => {
        if ((e.key === "Enter" || e.key === ",") && e.target.value.trim()) {
          e.preventDefault();
          addMatch(e.target.value.trim());
        } else if (e.key === "Backspace" && !e.target.value && w.matches.length) {
          w.matches.pop();
          changed();
          renderMatches();
          matchWrap.querySelector("input").focus();
        }
      },
      onblur: (e) => e.target.value.trim() && addMatch(e.target.value.trim(), false),
    });
    matchWrap.replaceChildren(
      ...w.matches.map((m, k) =>
        h(
          "span",
          { class: "chip" },
          m,
          h(
            "button",
            {
              type: "button",
              class: "chip-x",
              "aria-label": `Remove ${m}`,
              onclick: () => {
                w.matches.splice(k, 1);
                changed();
                renderMatches();
              },
            },
            icon("x", 11, 2.5),
          ),
        ),
      ),
      input,
    );
  };
  const addMatch = (text, refocus = true) => {
    if (!w.matches.some((m) => m.toLowerCase() === text.toLowerCase())) w.matches.push(text);
    changed();
    renderMatches();
    if (refocus) matchWrap.querySelector("input").focus();
  };
  renderMatches();
  matchWrap.addEventListener("click", (e) => e.target === matchWrap && matchWrap.querySelector("input").focus());

  const warn = [];
  if (!w.matches.length) warn.push("Add at least one app name, or this wheel is ignored.");
  if (!w.modes.length) warn.push("Add at least one slice, or this wheel is ignored.");
  if (cfg.app_wheels.some((o, k) => k !== i && o.name === w.name)) warn.push("Two app wheels share this name; give each its own.");

  return h(
    "div",
    { class: "card" },
    h(
      "div",
      { class: "card-head" },
      h(
        "div",
        { class: "grow" },
        h("input", {
          class: "input title-input app-name",
          value: w.name,
          spellcheck: "false",
          "aria-label": "Wheel name",
          oninput: (e) => {
            w.name = e.target.value;
            document.querySelectorAll(".wheel-chip.on .chip-text").forEach((el) => (el.textContent = w.name || "Untitled"));
            changed();
          },
        }),
      ),
      h(
        "button",
        {
          class: "btn ghost danger",
          type: "button",
          onclick: () => {
            cfg.app_wheels.splice(i, 1);
            ui.wheel = "default";
            ui.previewSel = 0;
            changed({ render: true });
          },
        },
        icon("trash-2", 14),
        "Delete wheel",
      ),
    ),
    h(
      "div",
      { class: "field" },
      h("div", { class: "field-label" }, "Used when one of these apps is in front"),
      matchWrap,
      h("div", { class: "field-hint" }, data.platform === "windows" ? "The program's file name without .exe, e.g. Resolve. Case doesn't matter." : "The app's name as in the menu bar, e.g. DaVinci Resolve. Case doesn't matter."),
    ),
    warn.length ? h("div", { class: "inline-warn" }, warn.map((t) => h("div", null, t))) : null,
    h("div", { class: "field-label slices-label" }, "Slices"),
    sliceList(w.modes, { onSelect: previewSelect, selected: ui.previewSel }),
  );
}

const platformAppHint = () => (data.platform === "windows" ? "e.g. Resolve, then Enter" : "e.g. DaVinci Resolve, then Enter");

// ---------- Actions ----------

function slugify(name) {
  return (
    name
      .toLowerCase()
      .normalize("NFKD")
      .replace(/[^\w\s-]/g, "")
      .trim()
      .replace(/[\s_]+/g, "-")
      .replace(/-+/g, "-")
      .slice(0, 32) || "action"
  );
}

function uniqueId(base, except = null) {
  const taken = (id) => modeById()[id] || cfg.actions.some((a) => a.id === id && a !== except);
  let id = base;
  for (let k = 2; taken(id); k++) id = `${base}-${k}`;
  return id;
}

/** Rename an action id everywhere it is used. */
function renameId(from, to) {
  const fix = (list) => list.forEach((id, i) => id === from && (list[i] = to));
  fix(cfg.wheel);
  cfg.app_wheels.forEach((w) => fix(w.modes));
  cfg.actions.forEach((a) => a.type === "wheel" && fix(a.slices));
}

function removeIdEverywhere(id) {
  const drop = (list) => {
    for (let i = list.length - 1; i >= 0; i--) if (list[i] === id) list.splice(i, 1);
  };
  drop(cfg.wheel);
  cfg.app_wheels.forEach((w) => drop(w.modes));
  cfg.actions.forEach((a) => a.type === "wheel" && drop(a.slices));
}

function usedIn(id) {
  const out = [];
  if (cfg.wheel.includes(id)) out.push("Default");
  cfg.app_wheels.forEach((w) => w.modes.includes(id) && out.push(w.name));
  cfg.actions.forEach((a) => a.type === "wheel" && a.slices.includes(id) && out.push(a.name || a.id));
  return out;
}

/** Whether sub-wheel `outer` contains `inner`, directly or deeper down. */
function contains(outer, inner, seen = new Set()) {
  const a = findAction(outer);
  if (!a || a.type !== "wheel" || seen.has(outer)) return false;
  seen.add(outer);
  return a.slices.some((id) => id === inner || contains(id, inner, seen));
}

function setType(a, type) {
  const keep = { id: a.id, name: a.name, icon: a.icon, color: a.color };
  for (const k of Object.keys(a)) delete a[k];
  Object.assign(a, keep, { type });
  if (a.icon == null) delete a.icon;
  if (a.color == null) delete a.color;
  if (type === "keys") a.keys = "";
  if (type === "open") a.target = "";
  if (type === "text") a.text = "";
  if (type === "media") a.key = "play_pause";
  if (type === "wheel") a.slices = [];
}

function newAction() {
  const a = { id: uniqueId("new-action"), name: "New action", type: "keys", keys: "" };
  cfg.actions.push(a);
  ui.newIds.add(a.id);
  ui.action = a.id;
  changed({ render: true });
  renderNav();
  const n = document.querySelector(".action-name");
  n?.focus();
  n?.select();
}

function renderActions() {
  if (!findAction(ui.action)) ui.action = cfg.actions[0]?.id ?? null;
  const list = h(
    "div",
    { class: "action-list" },
    cfg.actions.map((a) => {
      const s = sliceInfo(a.id);
      const typeName = ACTION_TYPES.find(([t]) => t === a.type)?.[1] || a.type;
      const bad = problems.some((p) => p.includes(`"${a.id}"`));
      return h(
        "button",
        {
          type: "button",
          title: a.name || a.id,
          "data-id": a.id,
          class: `action-item${a.id === ui.action ? " on" : ""}`,
          onclick: () => {
            ui.action = a.id;
            ui.previewSel = 0;
            renderContent();
          },
        },
        badge(s, 30),
        h("span", { class: "action-item-text" }, h("span", { class: "action-item-name" }, a.name || a.id || "Untitled"), h("span", { class: "action-item-type" }, typeName)),
        h("span", { class: `warn-dot${bad ? "" : " none"}`, title: "Needs attention" }),
      );
    }),
    h("button", { type: "button", class: "btn add-btn wide", title: "New action", onclick: newAction }, icon("plus", 14), h("span", { class: "add-label" }, "New action")),
  );

  const a = findAction(ui.action);
  const detail = a
    ? actionEditor(a)
    : h(
        "div",
        { class: "card empty-card" },
        icon("zap", 28, 1.5, "ic big"),
        h("div", { class: "empty-title" }, "Custom actions"),
        h("div", { class: "empty-body" }, "Shortcuts, apps or links to open, text to type, media keys, or a whole sub-wheel. Put them on any wheel."),
        h("button", { type: "button", class: "btn primary", onclick: newAction }, icon("plus", 14), "New action"),
      );

  if (!a) return detail;
  return h("div", { class: "split actions" }, h("div", { class: "split-list" }, list), h("div", { class: "split-main" }, detail));
}

function actionEditor(a) {
  const refreshListItem = () => {
    const item = document.querySelector(".action-item.on");
    if (!item) return;
    const s = sliceInfo(a.id);
    item.querySelector(".badge")?.replaceWith(badge(s, 30));
    item.querySelector(".action-item-name").textContent = a.name || a.id || "Untitled";
  };

  const idInput = h("input", {
    class: "input mono id-input",
    value: a.id,
    spellcheck: "false",
    "aria-label": "Action id",
    onchange: (e) => {
      const want = slugify(e.target.value);
      const id = uniqueId(want, a);
      if (id !== a.id) {
        renameId(a.id, id);
        ui.newIds.delete(a.id);
        a.id = id;
        ui.action = id;
      }
      e.target.value = a.id;
      changed();
    },
  });

  const nameInput = h("input", {
    class: "input title-input action-name",
    value: a.name,
    placeholder: "Name",
    spellcheck: "false",
    "aria-label": "Action name",
    oninput: (e) => {
      a.name = e.target.value;
      // a new action's id follows its name until saved under another tab
      if (ui.newIds.has(a.id)) {
        const id = uniqueId(slugify(a.name || "action"), a);
        if (id !== a.id) {
          renameId(a.id, id);
          ui.newIds.delete(a.id);
          ui.newIds.add(id);
          a.id = id;
          ui.action = id;
          idInput.value = id;
        }
      }
      refreshListItem();
      changed();
    },
  });

  const uses = usedIn(a.id);
  const onDefault = cfg.wheel.includes(a.id);

  return h(
    "div",
    { class: "card action-card" },
    h(
      "div",
      { class: "action-top" },
      appearanceButton(a, refreshListItem),
      h("div", { class: "grow" }, nameInput, h("div", { class: "id-row" }, h("span", { class: "id-label" }, "id"), idInput)),
    ),
    h(
      "div",
      { class: "field" },
      h("div", { class: "field-label" }, "Does"),
      h(
        "div",
        { class: "type-grid", role: "radiogroup" },
        ACTION_TYPES.map(([t, label, ic]) =>
          h(
            "button",
            {
              type: "button",
              role: "radio",
              "aria-checked": String(t === a.type),
              class: `type-tile${t === a.type ? " on" : ""}`,
              onclick: () => {
                if (t === a.type) return;
                setType(a, t);
                changed({ render: true });
              },
            },
            icon(ic, 18),
            label,
          ),
        ),
      ),
    ),
    actionValueField(a),
    h(
      "div",
      { class: "used-row" },
      h("span", { class: "field-label inline" }, "On"),
      uses.length ? uses.map((u) => h("span", { class: "chip static" }, u)) : h("span", { class: "dim" }, "no wheel yet"),
      onDefault
        ? null
        : h(
            "button",
            {
              type: "button",
              class: "btn small",
              onclick: () => {
                cfg.wheel.push(a.id);
                changed({ render: true });
              },
            },
            icon("plus", 12),
            "Add to default wheel",
          ),
    ),
    h(
      "div",
      { class: "card-foot" },
      h(
        "button",
        {
          type: "button",
          class: "btn ghost danger",
          onclick: () => {
            const i = cfg.actions.indexOf(a);
            removeIdEverywhere(a.id);
            cfg.actions.splice(i, 1);
            ui.action = cfg.actions[Math.min(i, cfg.actions.length - 1)]?.id ?? null;
            changed({ render: true });
            renderNav();
          },
        },
        icon("trash-2", 14),
        uses.length ? `Delete (and remove from ${uses.length} wheel${uses.length === 1 ? "" : "s"})` : "Delete action",
      ),
    ),
  );
}

/** Big badge that opens the icon and color picker. */
function appearanceButton(a, refresh) {
  const btn = h("button", { type: "button", class: "appearance-btn", title: "Icon and color" });
  const paint = () => btn.replaceChildren(badge(sliceInfo(a.id), 52), h("span", { class: "appearance-edit" }, icon("pencil", 11, 2.5)));
  paint();
  btn.addEventListener("click", () => {
    const grid = h("div", { class: "icon-grid" });
    const swatches = h("div", { class: "swatches" });
    const renderPicker = () => {
      const current = resolveIcon(a.id, a.icon, a.type === "wheel" ? "wheel" : "action");
      grid.replaceChildren(
        ...ICON_NAMES.map((n) =>
          h(
            "button",
            {
              type: "button",
              class: `icon-cell${n === current ? " on" : ""}`,
              title: n,
              onclick: () => {
                a.icon = n;
                update();
              },
            },
            icon(n, 18, 1.75),
          ),
        ),
      );
      const auto = !a.color;
      swatches.replaceChildren(
        h(
          "button",
          {
            type: "button",
            class: `swatch auto${auto ? " on" : ""}`,
            title: "Theme color",
            onclick: () => {
              delete a.color;
              update();
            },
          },
          "Auto",
        ),
        ...SWATCHES.map((c) =>
          h("button", {
            type: "button",
            class: `swatch${a.color?.toLowerCase() === c ? " on" : ""}`,
            style: `--c:${c}`,
            title: c,
            onclick: () => {
              a.color = c;
              update();
            },
          }),
        ),
      );
    };
    const hexInput = h("input", {
      class: "input mono hex",
      value: a.color || "",
      placeholder: "#hex",
      spellcheck: "false",
      maxlength: "7",
      oninput: (e) => {
        let v = e.target.value.trim();
        if (v && !v.startsWith("#")) v = `#${v}`;
        if (HEX_RE.test(v)) {
          a.color = v.toLowerCase();
          colorInput.value = a.color;
          update(false);
        }
      },
    });
    const colorInput = h("input", {
      type: "color",
      class: "color-well",
      value: a.color && HEX_RE.test(a.color) ? a.color : "#b9adf5",
      oninput: (e) => {
        a.color = e.target.value;
        hexInput.value = a.color;
        update(false);
      },
    });
    const update = (repaintHex = true) => {
      renderPicker();
      paint();
      refresh();
      if (repaintHex) hexInput.value = a.color || "";
      changed();
    };
    renderPicker();
    openPopover(
      btn,
      h(
        "div",
        { class: "appearance" },
        h("div", { class: "pop-label" }, "Color"),
        swatches,
        h("div", { class: "hex-row" }, colorInput, hexInput, h("span", { class: "dim small" }, "custom")),
        h("div", { class: "pop-label" }, "Icon"),
        grid,
      ),
      { width: 352 },
    );
  });
  return btn;
}

function actionValueField(a) {
  switch (a.type) {
    case "keys":
      return shortcutField(a);
    case "open":
      return h(
        "div",
        { class: "field" },
        h("div", { class: "field-label" }, "Open"),
        h("input", {
          class: "input",
          value: a.target,
          placeholder: data.platform === "windows" ? "https://…, C:\\Folder, or an app like notepad" : "https://…, ~/Documents, or an app like Safari",
          spellcheck: "false",
          oninput: (e) => {
            a.target = e.target.value;
            changed();
          },
        }),
        h("div", { class: "field-hint" }, "A URL, a file or folder path, or an app name."),
      );
    case "text":
      return h(
        "div",
        { class: "field" },
        h("div", { class: "field-label" }, "Type this"),
        h("textarea", {
          class: "input textarea",
          rows: "4",
          value: a.text,
          placeholder: "Text to type into the app in front",
          oninput: (e) => {
            a.text = e.target.value;
            changed();
          },
        }),
      );
    case "media":
      return h(
        "div",
        { class: "field" },
        h("div", { class: "field-label" }, "Media key"),
        h(
          "div",
          { class: "select-wrap" },
          h(
            "select",
            {
              class: "input select",
              onchange: (e) => {
                a.key = e.target.value;
                changed();
              },
            },
            MEDIA_KEYS.map(([k, label]) => h("option", { value: k, selected: a.key === k }, label)),
          ),
          icon("chevron-down", 14, 2, "ic select-caret"),
        ),
      );
    case "wheel":
      return h(
        "div",
        { class: "field" },
        h("div", { class: "field-label" }, "Slices of the sub-wheel"),
        h(
          "div",
          { class: "subwheel" },
          h(
            "div",
            { class: "subwheel-list" },
            sliceList(a.slices, {
              exclude: (id) => id === a.id || contains(id, a.id),
              onSelect: previewSelect,
              selected: ui.previewSel,
              emptyText: "Empty sub-wheels are skipped. Add a slice.",
            }),
          ),
          h("div", { class: "subwheel-preview" }, previewPanel("How the sub-wheel looks when it opens.")),
        ),
      );
    default:
      return null;
  }
}

// ---------- Shortcut recording ----------

const CODE_KEYS = {
  Minus: "-",
  Equal: "=",
  BracketLeft: "[",
  BracketRight: "]",
  Backslash: "\\",
  Semicolon: ";",
  Quote: "'",
  Comma: "comma",
  Period: ".",
  Slash: "/",
  Backquote: "`",
  Enter: "enter",
  NumpadEnter: "enter",
  Tab: "tab",
  Space: "space",
  Escape: "esc",
  Backspace: "backspace",
  Delete: "delete",
  ArrowUp: "up",
  ArrowDown: "down",
  ArrowLeft: "left",
  ArrowRight: "right",
  Home: "home",
  End: "end",
  PageUp: "pageup",
  PageDown: "pagedown",
};

function keyWord(code) {
  if (/^Key[A-Z]$/.test(code)) return code.slice(3).toLowerCase();
  if (/^Digit\d$/.test(code)) return code.slice(5);
  if (/^Numpad\d$/.test(code)) return code.slice(6);
  if (/^F\d{1,2}$/.test(code) && Number(code.slice(1)) <= 20) return code.toLowerCase();
  return CODE_KEYS[code] || null;
}

function modWords(e) {
  const mac = data.platform === "macos";
  const out = [];
  if (mac) {
    if (e.metaKey) out.push("cmd");
    if (e.ctrlKey) out.push("ctrl");
    if (e.altKey) out.push("opt");
    if (e.shiftKey) out.push("shift");
  } else {
    if (e.ctrlKey) out.push("ctrl");
    if (e.altKey) out.push("alt");
    if (e.shiftKey) out.push("shift");
    if (e.metaKey) out.push("win");
  }
  return out;
}

const MOD_WORDS = { cmd: 1, command: 1, meta: 1, super: 1, win: 1, windows: 1, ctrl: 1, control: 1, alt: 1, opt: 1, option: 1, shift: 1, mod: 1, cmdorctrl: 1, primary: 1 };
const NAMED = new Set(["enter", "return", "tab", "space", "esc", "escape", "backspace", "delete", "del", "forwarddelete", "up", "down", "left", "right", "home", "end", "pageup", "pgup", "pagedown", "pgdn", "plus", "comma", "minus"]);
const PUNCT = "`-=[]\\;',./";

/** Mirrors keys.rs: returns an error message or "". */
function checkShortcut(text) {
  const combos = text.split(",").filter((p) => p.trim());
  if (!combos.length) return "Record or type a shortcut, e.g. cmd+shift+t.";
  for (let c of combos) {
    c = c.trim().toLowerCase();
    let parts;
    let key;
    if (c.endsWith("++")) {
      parts = c.slice(0, -2).split("+").map((s) => s.trim());
      key = "plus";
    } else {
      parts = c.split("+").map((s) => s.trim());
      key = parts.pop();
    }
    for (const p of parts) if (!MOD_WORDS[p]) return `Unknown modifier "${p}".`;
    const ok = NAMED.has(key) || /^f([1-9]|1\d|20)$/.test(key) || (key.length === 1 && (/[a-z0-9]/.test(key) || PUNCT.includes(key)));
    if (!ok) return key ? `Unknown key "${key}".` : `"${c}" has no key.`;
  }
  return "";
}

const GLYPHS = { cmd: "⌘", command: "⌘", ctrl: "⌃", control: "⌃", opt: "⌥", alt: "⌥", option: "⌥", shift: "⇧", enter: "↩", return: "↩", tab: "⇥", esc: "esc", escape: "esc", backspace: "⌫", delete: "⌦", up: "↑", down: "↓", left: "←", right: "→", space: "space", comma: ",", plus: "+", minus: "-" };

function keycaps(text) {
  const mac = data.platform === "macos";
  const combos = text.split(",").map((c) => c.trim()).filter(Boolean);
  return h(
    "div",
    { class: "keycaps" },
    combos.map((c, i) => [
      i ? h("span", { class: "then" }, "then") : null,
      h(
        "span",
        { class: "combo" },
        (c.endsWith("++") ? [...c.slice(0, -2).split("+"), "plus"] : c.split("+")).map((k) => {
          const w = k.trim().toLowerCase();
          let label = mac ? GLYPHS[w] : { cmd: "Win", win: "Win", ctrl: "Ctrl", alt: "Alt", shift: "Shift", mod: "Ctrl" }[w] || GLYPHS[w];
          if (w === "mod") label = mac ? "⌘" : "Ctrl";
          if (w === "win") label = mac ? "⌘" : "Win";
          return h("kbd", null, label || w.toUpperCase());
        }),
      ),
    ]),
  );
}

function shortcutField(a) {
  const err = h("div", { class: "field-error" });
  const caps = h("div", { class: "caps-slot" });
  const input = h("input", {
    class: "input mono",
    value: a.keys,
    placeholder: data.platform === "macos" ? "cmd+shift+t   or   ctrl+k, ctrl+c" : "ctrl+shift+t   or   ctrl+k, ctrl+c",
    spellcheck: "false",
    autocapitalize: "off",
    oninput: (e) => {
      a.keys = e.target.value;
      refresh();
      changed();
    },
  });
  const refresh = () => {
    const msg = a.keys.trim() ? checkShortcut(a.keys) : "";
    err.textContent = msg;
    input.classList.toggle("bad", !!msg);
    caps.replaceChildren(a.keys.trim() && !msg ? keycaps(a.keys) : "");
  };
  const rec = h("button", { type: "button", class: "btn record" }, h("span", { class: "rec-dot" }), "Record");
  const then = h("button", { type: "button", class: "btn", title: "Record another combo to press after this one" }, icon("plus", 12), "Then…");
  rec.addEventListener("click", () => startRecording(a, input, rec, false, refresh));
  then.addEventListener("click", () => startRecording(a, input, then, true, refresh));
  refresh();
  return h(
    "div",
    { class: "field" },
    h("div", { class: "field-label" }, "Shortcut"),
    h("div", { class: "shortcut-row" }, input, rec, a.keys.trim() ? then : null),
    caps,
    err,
    h(
      "div",
      { class: "field-hint" },
      "Record captures the next key combo. Some combos (",
      data.platform === "macos" ? "⌘Tab, ⌘Space" : "Win+L, Alt+Tab",
      ") never reach this window, so type those by hand. ",
      h("code", null, "mod"),
      " means ⌘ on Mac and Ctrl on Windows; separate a sequence with commas.",
    ),
  );
}

function startRecording(a, input, btn, append, refresh) {
  stopRecording();
  btn.classList.add("recording");
  const label = btn.lastChild;
  const was = label.textContent;
  label.textContent = "Press keys…";
  const onKey = (e) => {
    e.preventDefault();
    e.stopPropagation();
    const mods = modWords(e);
    const word = keyWord(e.code);
    if (!word) {
      // a modifier on its own: show what's held so far
      label.textContent = mods.length ? `${mods.join("+")}+…` : "Press keys…";
      return;
    }
    if (word === "esc" && !mods.length) {
      stopRecording();
      return;
    }
    const combo = [...mods, word].join("+");
    a.keys = append && a.keys.trim() ? `${a.keys.trim()}, ${combo}` : combo;
    input.value = a.keys;
    stopRecording();
    refresh();
    changed({ render: !append });
  };
  const onKeyUp = (e) => {
    e.preventDefault();
    if (!keyWord(e.code)) {
      const mods = modWords(e);
      label.textContent = mods.length ? `${mods.join("+")}+…` : "Press keys…";
    }
  };
  const onDown = (e) => {
    if (!btn.contains(e.target)) stopRecording();
  };
  window.addEventListener("keydown", onKey, true);
  window.addEventListener("keyup", onKeyUp, true);
  document.addEventListener("pointerdown", onDown, true);
  ui.recording = () => {
    window.removeEventListener("keydown", onKey, true);
    window.removeEventListener("keyup", onKeyUp, true);
    document.removeEventListener("pointerdown", onDown, true);
    btn.classList.remove("recording");
    label.textContent = was;
    ui.recording = null;
  };
}

function stopRecording() {
  ui.recording?.();
}

// ---------- Knob ----------

const STEPS = [
  ["clockwise", "Turn right", "Turn the knob one click clockwise."],
  ["counter_clockwise", "Turn left", "Now one click the other way."],
  ["press", "Press", "Finally, press the knob."],
];

function stepIcon(step) {
  if (step === "press") return h("span", { class: "ic", html: '<svg width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="3.2" fill="currentColor" stroke="none"/></svg>' });
  const i = icon("rotate-ccw", 20);
  if (step === "clockwise") i.classList.add("flip");
  return i;
}

function devName(d) {
  if (!d) return null;
  const hex = (v) => v.toString(16).padStart(4, "0");
  return { name: d.name || "Unnamed keyboard", ids: `${hex(d.vendor_id)}:${hex(d.product_id)}` };
}

async function startLearn() {
  ui.learn = { state: "learning", step: "clockwise", keys: null, device: null, error: "" };
  renderContent();
  try {
    await invoke("start_learn");
  } catch (err) {
    ui.learn = { state: "idle", step: null, keys: null, device: null, error: String(err) };
    renderContent();
  }
}

function cancelLearn() {
  invoke("cancel_learn").catch(() => {});
  ui.learn = { state: "idle", step: null, keys: null, device: null, error: "" };
}

function onLearn(ev) {
  if (ev.type === "prompt") ui.learn = { ...ui.learn, state: "learning", step: ev.step };
  else if (ev.type === "done") ui.learn = { state: "done", step: null, keys: ev.keys, device: ev.device ?? null, error: "" };
  else if (ev.type === "cancelled") {
    if (ui.learn.state !== "learning") return;
    ui.learn = { state: "idle", step: null, keys: null, device: null, error: "Timed out. Nothing changed." };
  }
  if (ui.tab === "knob") renderContent();
}

function keyMap(keys, device) {
  const d = devName(device);
  return h(
    "div",
    { class: "keymap" },
    STEPS.map(([k, label]) => h("div", { class: "keymap-row" }, stepIcon(k), h("span", { class: "keymap-label" }, label), h("span", { class: "keymap-value" }, MEDIA_LABEL[keys[k]] || keys[k]))),
    h(
      "div",
      { class: "keymap-row" },
      icon("keyboard", 20),
      h("span", { class: "keymap-label" }, "Keyboard"),
      d ? h("span", { class: "keymap-value" }, d.name, h("span", { class: "mono dim small" }, ` ${d.ids}`)) : h("span", { class: "keymap-value dim" }, "not learned"),
    ),
  );
}

function renderKnob() {
  const L = ui.learn;
  let learnBody;
  if (L.state === "learning") {
    const at = STEPS.findIndex(([k]) => k === L.step);
    learnBody = h(
      "div",
      { class: "learn" },
      h(
        "div",
        { class: "steps" },
        STEPS.map(([k, label], i) =>
          h("div", { class: `step${i < at ? " done" : i === at ? " now" : ""}` }, h("div", { class: "step-ic" }, i < at ? icon("circle-check", 20) : stepIcon(k)), h("div", { class: "step-label" }, label)),
        ),
      ),
      h("div", { class: "learn-prompt" }, STEPS[at]?.[2] || ""),
      h("div", { class: "learn-actions" }, h("button", { type: "button", class: "btn", onclick: () => { cancelLearn(); renderContent(); } }, "Cancel")),
    );
  } else if (L.state === "done") {
    learnBody = h(
      "div",
      { class: "learn" },
      h("div", { class: "learn-prompt ok" }, icon("circle-check", 16), "Got it. Save to use these keys."),
      keyMap(L.keys, L.device),
      h(
        "div",
        { class: "learn-actions" },
        h("button", { type: "button", class: "btn", onclick: () => { ui.learn = { state: "idle", step: null, keys: null, device: null, error: "" }; renderContent(); } }, "Discard"),
        h(
          "button",
          {
            type: "button",
            class: "btn primary",
            onclick: () => {
              cfg.knob_keys = L.keys;
              if (L.device) cfg.knob_device = L.device;
              ui.learn = { state: "idle", step: null, keys: null, device: null, error: "" };
              changed({ render: true });
            },
          },
          "Save",
        ),
      ),
    );
  } else {
    learnBody = h(
      "div",
      { class: "learn" },
      keyMap(cfg.knob_keys, cfg.knob_device),
      L.error ? h("div", { class: "field-error" }, L.error) : null,
      h("div", { class: "learn-actions" }, h("button", { type: "button", class: "btn primary", onclick: startLearn, disabled: !data.running && !ui.perms?.accessibility }, icon("sparkles", 14), "Learn my knob")),
    );
  }

  const mac = data.platform === "macos";
  const P = ui.perms;
  const permRow = (label, ok, kind, why) =>
    h(
      "div",
      { class: "perm" },
      h("span", { class: `perm-dot ${P == null ? "" : ok ? "ok" : "no"}` }),
      h("div", { class: "perm-text" }, h("div", { class: "row-label" }, label), h("div", { class: "row-desc" }, P == null ? "Checking…" : ok ? "Allowed" : why)),
      ok
        ? null
        : h(
            "div",
            { class: "perm-btns" },
            h("button", { type: "button", class: "btn small", onclick: async () => { await invoke("request_permission", { kind }); refreshPermissions(); } }, "Request"),
            h("button", { type: "button", class: "btn small", onclick: () => invoke("open_privacy", { kind }) }, "Open System Settings"),
          ),
    );

  const hasDevice = !!cfg.knob_device;
  return h(
    "div",
    { class: "stack" },
    h(
      "div",
      { class: "card" },
      h("div", { class: "card-head" }, h("div", null, h("div", { class: "card-title" }, "Learn my knob"), h("div", { class: "card-sub" }, "Most knobs send volume up, volume down and mute. If yours sends something else, or you want SlobKnob to recognise its keyboard, teach it here."))),
      learnBody,
    ),
    h(
      "div",
      { class: "card" },
      row("Invert direction", "Swap clockwise and counter-clockwise for everything the knob does.", toggle(cfg.invert_direction, (v) => { cfg.invert_direction = v; changed(); })),
      row(
        "Only use the knob's keyboard",
        h(
          "span",
          null,
          "Volume keys on other keyboards (like the MacBook's) keep working normally.",
          mac ? " Needs Input Monitoring." : "",
          hasDevice ? "" : h("span", { class: "need" }, " Learn your knob first so SlobKnob knows which keyboard it is."),
        ),
        toggle(cfg.only_knob_device && hasDevice, (v) => { cfg.only_knob_device = v; changed({ render: true }); }, !hasDevice),
        { disabled: !hasDevice },
      ),
      hasDevice
        ? h(
            "div",
            { class: "device-line" },
            icon("keyboard", 14),
            h("span", null, devName(cfg.knob_device).name),
            h("span", { class: "mono dim" }, devName(cfg.knob_device).ids),
            h("button", { type: "button", class: "link-btn", onclick: () => { cfg.knob_device = null; cfg.only_knob_device = false; changed({ render: true }); } }, "Forget"),
          )
        : null,
    ),
    mac
      ? h(
          "div",
          { class: "card" },
          h("div", { class: "card-head" }, h("div", null, h("div", { class: "card-title" }, "Permissions"))),
          permRow("Accessibility", P?.accessibility, "accessibility", "Needed to see and swallow the knob's keys. SlobKnob does nothing without it."),
          permRow("Input Monitoring", P?.input_monitoring, "input_monitoring", "Needed to tell keyboards apart (\"Only use the knob's keyboard\" and learning the device)."),
        )
      : null,
  );
}

let permTimer = 0;
async function refreshPermissions() {
  clearTimeout(permTimer);
  try {
    const p = await invoke("permissions");
    const before = JSON.stringify(ui.perms);
    ui.perms = p;
    if (ui.tab === "knob" && before !== JSON.stringify(p) && ui.learn.state !== "learning") renderContent();
  } catch {
    /* ignore */
  }
  if (ui.tab === "knob") permTimer = setTimeout(refreshPermissions, 3000);
}

// ---------- General ----------

function renderGeneral() {
  const mac = data.platform === "macos";
  const mod = mac ? "⌘" : "Win";
  const accentMode = cfg.accent === "pastel" ? "pastel" : cfg.accent === "system" ? "system" : "custom";

  const accentPicker = h(
    "div",
    { class: "accent-pick" },
    h("button", { type: "button", class: `accent-opt pastel${accentMode === "pastel" ? " on" : ""}`, title: "Pastel: a soft color per mode", onclick: () => setAccent("pastel") }),
    h("button", { type: "button", class: `accent-opt system${accentMode === "system" ? " on" : ""}`, style: `--c:${data.system_accent}`, title: "System accent color", onclick: () => setAccent("system") }),
    h("span", { class: "accent-sep" }),
    ACCENT_PRESETS.map((c) => h("button", { type: "button", class: `accent-opt${cfg.accent.toLowerCase() === c ? " on" : ""}`, style: `--c:${c}`, title: c, onclick: () => setAccent(c) })),
    h(
      "label",
      { class: `accent-opt custom${accentMode === "custom" && !ACCENT_PRESETS.includes(cfg.accent.toLowerCase()) ? " on" : ""}`, title: "Custom color", style: accentMode === "custom" ? `--c:${cfg.accent}` : "" },
      h("input", { type: "color", value: accentMode === "custom" ? cfg.accent : "#3478f6", oninput: (e) => setAccent(e.target.value, false), onchange: () => renderContent() }),
      icon("plus", 12, 2.5),
    ),
  );

  const label = { pastel: "Pastel: a soft color per mode", system: "System accent color", custom: `Custom ${cfg.accent}` }[accentMode];

  return h(
    "div",
    { class: "stack" },
    h(
      "div",
      { class: "card" },
      h("div", { class: "card-head" }, h("div", null, h("div", { class: "card-title" }, "Opening the wheel"))),
      row(
        "Knob gesture",
        cfg.trigger === "hold" ? "Hold the knob, turn, and release to pick. Only works if your knob reports how long it is held; many (like the AULA F75) send an instant click, so the wheel never opens." : "Double-tap the knob, then turn. Single taps still do what the mode does.",
        segmented([["doubletap", "Double tap"], ["hold", "Hold"]], cfg.trigger, (v) => { cfg.trigger = v; changed({ render: true }); }),
      ),
      cfg.trigger === "hold"
        ? row("Hold for", "How long to hold before the wheel opens.", slider({ value: cfg.hold_ms, min: 150, max: 1000, step: 10, format: ms, onchange: (v) => { cfg.hold_ms = v; changed(); } }), { sub: true })
        : row("Double-tap window", "Longest gap between the two taps. Single taps wait this long before acting.", slider({ value: cfg.double_tap_ms, min: 120, max: 600, step: 10, format: ms, onchange: (v) => { cfg.double_tap_ms = v; changed(); } }), { sub: true }),
      row(`${mod} + turn`, `Hold ${mod} and turn the knob to open the wheel; release ${mod} to pick.`, toggle(cfg.modifier_trigger, (v) => { cfg.modifier_trigger = v; changed(); })),
    ),
    h(
      "div",
      { class: "card" },
      h("div", { class: "card-head" }, h("div", null, h("div", { class: "card-title" }, "Timing"))),
      row("Auto-pick after", "After you stop turning, pick the highlighted mode. Actions always need a tap.", slider({ value: cfg.auto_confirm_ms, min: 0, max: 2000, step: 50, format: (v) => (v ? ms(v) : "Off: tap"), onchange: (v) => { cfg.auto_confirm_ms = v; changed(); } })),
      row("Close an idle wheel after", "If you open the wheel and leave it.", slider({ value: cfg.wheel_timeout_ms, min: 1000, max: 20000, step: 500, format: ms, onchange: (v) => { cfg.wheel_timeout_ms = v; changed(); } })),
      row("Back to volume after", "Return to Volume mode when the knob has been idle this long.", slider({ value: Math.round(cfg.revert_after_ms / 1000), min: 0, max: 600, step: 5, format: (v) => (v ? (v >= 60 ? `${Math.floor(v / 60)} min${v % 60 ? ` ${v % 60} s` : ""}` : `${v} s`) : "Never"), onchange: (v) => { cfg.revert_after_ms = v * 1000; changed(); } })),
      row("Seek step", "How far Seek mode jumps per click.", numberField({ value: cfg.seek_seconds, min: 1, max: 120, unit: "s", onchange: (v) => { cfg.seek_seconds = v; changed(); } })),
    ),
    h(
      "div",
      { class: "card" },
      h("div", { class: "card-head" }, h("div", null, h("div", { class: "card-title" }, "Look"))),
      row("Accent", label, accentPicker),
      row("Dim the screen", "Darken everything behind the wheel while it's open.", toggle(cfg.dim_screen, (v) => { cfg.dim_screen = v; changed(); })),
    ),
    h(
      "div",
      { class: "card" },
      row("Launch at login", "Start SlobKnob when you log in.", toggle(cfg.launch_at_login, (v) => { cfg.launch_at_login = v; changed(); })),
    ),
  );
}

function setAccent(v, render = true) {
  cfg.accent = v;
  if (render) renderContent();
  changed();
}

// ---------- Scroll ----------

function renderScroll() {
  return h(
    "div",
    { class: "stack" },
    h(
      "div",
      { class: "card" },
      row("Smooth scrolling", "Glide by pixels instead of jumping whole lines.", toggle(cfg.smooth_scroll, (v) => { cfg.smooth_scroll = v; changed({ render: true }); })),
      cfg.smooth_scroll
        ? row("Distance per click", "Before acceleration.", slider({ value: cfg.scroll_pixels, min: 5, max: 200, step: 1, format: (v) => `${v} px`, onchange: (v) => { cfg.scroll_pixels = v; changed(); } }), { sub: true })
        : row("Lines per click", "How many lines each knob click scrolls.", slider({ value: cfg.scroll_lines, min: 1, max: 20, step: 1, format: (v) => `${v} line${v === 1 ? "" : "s"}`, onchange: (v) => { cfg.scroll_lines = v; changed(); } }), { sub: true }),
      row("Acceleration", "Turning fast multiplies the step up to this much.", slider({ value: cfg.scroll_acceleration, min: 1, max: 10, step: 0.5, format: (v) => (v <= 1 ? "Off" : `up to ${v}×`), onchange: (v) => { cfg.scroll_acceleration = v; changed(); } })),
    ),
    h(
      "div",
      { class: "card" },
      h("div", { class: "card-head" }, h("div", null, h("div", { class: "card-title" }, "Drag & editing modes"))),
      row("Drag distance", "Drag mode: pixels the mouse moves per click.", slider({ value: cfg.drag_pixels, min: 1, max: 40, step: 1, format: (v) => `${v} px`, onchange: (v) => { cfg.drag_pixels = v; changed(); } })),
      row("Release drag after", "Drag mode lets go of the mouse button this long after the last turn.", slider({ value: cfg.drag_release_ms, min: 100, max: 2000, step: 50, format: ms, onchange: (v) => { cfg.drag_release_ms = v; changed(); } })),
      row("Pick app after", "Apps mode switches to the highlighted app this long after the last turn.", slider({ value: cfg.app_switch_commit_ms, min: 200, max: 3000, step: 50, format: ms, onchange: (v) => { cfg.app_switch_commit_ms = v; changed(); } })),
    ),
  );
}

// ---------- Sidebar footer ----------

function renderRunStatus() {
  const el = $("run-status");
  const state = data.paused ? ["paused", "Paused from the menu bar"] : data.running ? ["on", "Running"] : ["off", "Waiting for Accessibility"];
  el.className = `run-status ${state[0]}`;
  el.replaceChildren(h("span", { class: "run-dot" }), h("span", null, state[1]));
}

// ---------- Boot ----------

async function boot() {
  data = await invoke("get_settings");
  cfg = data.config;
  problems = data.problems;
  blockedByParseError = !!data.parse_error;
  theme = await invoke("preview_theme", { accent: cfg.accent, dim: cfg.dim_screen });
  document.documentElement.dataset.platform = data.platform;

  const initial = window.__KW_INITIAL_TAB || location.hash.slice(1);
  if (TABS.some(([id]) => id === initial)) ui.tab = initial;

  renderNav();
  renderBanners();
  renderContent();
  renderRunStatus();
  setSaveStatus(blockedByParseError ? "blocked" : "saved");

  $("open-config").addEventListener("click", () => invoke("open_config_file"));

  await T.event.listen("knob-learn", (e) => onLearn(e.payload));
  await T.event.listen("knob-preview", (e) => onKnobPreview(e.payload));
  window.addEventListener("focus", () => {
    if (ui.tab === "knob") refreshPermissions();
    invoke("get_settings").then((d) => {
      data.running = d.running;
      data.paused = d.paused;
      renderRunStatus();
    });
  });
  if (ui.tab === "knob") refreshPermissions();

  // Cmd/Ctrl+W closes; Cmd/Ctrl+S saves now.
  window.addEventListener("keydown", (e) => {
    const primary = data.platform === "macos" ? e.metaKey : e.ctrlKey;
    if (primary && e.key.toLowerCase() === "s") {
      e.preventDefault();
      if (!blockedByParseError) saveNow();
    } else if (primary && e.key.toLowerCase() === "w") {
      e.preventDefault();
      T.window?.getCurrentWindow?.().close();
    }
  });
  // flush a pending save before the window goes away
  const win = T.window?.getCurrentWindow?.();
  await win?.onCloseRequested(async () => {
    stopRecording();
    if (ui.learn.state === "learning") cancelLearn();
    if (saveTimer && !blockedByParseError) await saveNow();
  });
}

// Used by the Rust side to switch tabs (`--settings=<tab>` on a running window).
window.knobSettings = {
  showTab,
  // for poking at the UI from devtools / SLOBKNOB_EVAL in debug builds
  debug: { state: () => ({ cfg, ui, problems }), changed, renderContent, newAction, saveNow },
};

boot().catch((err) => {
  console.error(err);
  $("content").replaceChildren(h("div", { class: "banner bad" }, h("div", { class: "banner-title" }, "Settings failed to load"), h("div", { class: "banner-body mono" }, String(err))));
});
