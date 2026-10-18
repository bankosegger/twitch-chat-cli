"use strict";

// ---------- helpers ----------

/** Creates an element. Strings become text nodes, so data is never parsed as HTML. */
function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (value == null || value === false) continue;
    if (key === "class") node.className = value;
    else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
    else node.setAttribute(key, value === true ? "" : value);
  }
  for (const child of children.flat()) {
    if (child == null || child === false) continue;
    node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return node;
}

const SVG_NS = "http://www.w3.org/2000/svg";
function svg(tag, attrs = {}) {
  const node = document.createElementNS(SVG_NS, tag);
  for (const [key, value] of Object.entries(attrs)) node.setAttribute(key, value);
  return node;
}

async function api(path, params = {}, init = undefined) {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value != null && value !== "") query.set(key, value);
  }
  const qs = query.toString();
  const res = await fetch(`api${path}${qs ? `?${qs}` : ""}`, init);
  if (!res.ok) {
    const body = await res.json().catch(() => ({}));
    const err = new Error(body.error || `${res.status} ${res.statusText}`);
    err.status = res.status;
    throw err;
  }
  return res.json();
}

/** Sends a JSON request (POST, DELETE, ...) and returns the JSON response. */
function apiSend(method, path, body = undefined) {
  return api(path, {}, {
    method,
    headers: body === undefined ? {} : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
}

/** Builds a `#/path?query` link, dropping empty params. */
function href(path, params = {}) {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value != null && value !== "") query.set(key, value);
  }
  const qs = query.toString();
  return `#${path}${qs ? `?${qs}` : ""}`;
}

const numberFmt = new Intl.NumberFormat();
const compactFmt = new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 });
const dateTimeFmt = new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" });
const timeFmt = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit" });
const timeSecFmt = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit" });
const dayFmt = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric" });
const dayTimeFmt = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });

const fmtInt = (n) => numberFmt.format(n);
const fmtCompact = (n) => (Math.abs(n) >= 1000 ? compactFmt.format(n) : numberFmt.format(n));
const fmtDateTime = (ms) => dateTimeFmt.format(new Date(ms));

function fmtBytes(bytes) {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  return `${value.toFixed(unit === 0 || value >= 100 ? 0 : 1)} ${units[unit]}`;
}

function fmtMessageTime(ms) {
  const d = new Date(ms);
  const sameDay = d.toDateString() === new Date().toDateString();
  return sameDay ? timeSecFmt.format(d) : `${dayFmt.format(d)} ${timeSecFmt.format(d)}`;
}

function fmtAgo(ms) {
  const s = Math.round((Date.now() - ms) / 1000);
  if (s < 60) return "just now";
  const units = [["day", 86400], ["hour", 3600], ["minute", 60]];
  for (const [name, size] of units) {
    if (s >= size) {
      const n = Math.floor(s / size);
      return `${n} ${name}${n === 1 ? "" : "s"} ago`;
    }
  }
  return "just now";
}

const MINUTE = 60e3;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

const RANGES = [
  { key: "1h", label: "1 hour", span: HOUR },
  { key: "24h", label: "24 hours", span: DAY },
  { key: "7d", label: "7 days", span: 7 * DAY },
  { key: "30d", label: "30 days", span: 30 * DAY },
  { key: "all", label: "All time", span: null },
];

const BUCKETS = [
  { ms: MINUTE, label: "minute" },
  { ms: 5 * MINUTE, label: "5 minutes" },
  { ms: 15 * MINUTE, label: "15 minutes" },
  { ms: 30 * MINUTE, label: "30 minutes" },
  { ms: HOUR, label: "hour" },
  { ms: 3 * HOUR, label: "3 hours" },
  { ms: 6 * HOUR, label: "6 hours" },
  { ms: 12 * HOUR, label: "12 hours" },
  { ms: DAY, label: "day" },
  { ms: 7 * DAY, label: "week" },
];

/** The smallest bucket that keeps the chart at or under ~150 points. */
function pickBucket(span) {
  return BUCKETS.find((b) => span / b.ms <= 150) || BUCKETS[BUCKETS.length - 1];
}

/** Local-time offset east of UTC in ms, so day buckets start at local midnight. */
const tzOffset = () => -new Date().getTimezoneOffset() * MINUTE;

const alignDown = (t, size) => Math.floor((t + tzOffset()) / size) * size - tzOffset();

/** Badge tag (`moderator/1,subscriber/12`) → badges to show, in Twitch's order. */
const BADGES = {
  broadcaster: { key: "broadcaster", icon: "▶︎", title: "Broadcaster" },
  staff: { key: "staff", icon: "⚙︎", title: "Staff" },
  admin: { key: "staff", icon: "⚙︎", title: "Staff" },
  moderator: { key: "mod", icon: "⚔︎", title: "Moderator" },
  vip: { key: "vip", icon: "◆︎", title: "VIP" },
  subscriber: { key: "sub", icon: "★︎", title: "Subscriber" },
  founder: { key: "sub", icon: "★︎", title: "Founder" },
};

function badgesFor(raw) {
  const out = [];
  for (const entry of raw ? raw.split(",") : []) {
    const badge = BADGES[entry.split("/")[0]];
    if (badge && !out.some((b) => b.key === badge.key)) out.push(badge);
  }
  return out;
}

/** Twitch's name colors for users who never picked one. */
const DEFAULT_NAME_COLORS = ["#FF0000", "#0000FF", "#008000", "#B22222", "#FF7F50", "#9ACD32", "#FF4500", "#2E8B57", "#DAA520", "#D2691E", "#5F9EA0", "#1E90FF", "#FF69B4", "#8A2BE2", "#00FF7F"];

/** The user's chat color, lightened like Twitch does when it's too dark to read on the dark background. */
function nameColor(color, login) {
  let hex = color;
  if (!hex) {
    let hash = 0;
    for (const ch of login) hash = (hash * 31 + ch.charCodeAt(0)) | 0;
    hex = DEFAULT_NAME_COLORS[Math.abs(hash) % DEFAULT_NAME_COLORS.length];
  }
  const n = parseInt(hex.slice(1), 16);
  let [r, g, b] = [(n >> 16) & 255, (n >> 8) & 255, n & 255];
  const lum = (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255;
  const min = 0.5;
  if (lum < min) {
    const t = (min - lum) / (1 - lum);
    [r, g, b] = [r, g, b].map((c) => Math.round(c + (255 - c) * t));
  }
  return `rgb(${r}, ${g}, ${b})`;
}

// ---------- shared components ----------

function messageRow(m, { showChannel = false, focus = false } = {}) {
  const userHref = m.user_id ? href(`/u/${encodeURIComponent(m.user_id)}`) : href("/search", { login: m.login });
  const name = el("a", { class: "name", href: userHref }, m.display_name);
  name.style.color = nameColor(m.color, m.login);
  return el(
    "li",
    { class: `msg${focus ? " focus" : ""}`, "data-id": m.id },
    el(
      "span",
      { class: "time" },
      el("a", { href: href(`/m/${m.id}`), title: `${fmtDateTime(m.sent_at)} · show surrounding chat` }, fmtMessageTime(m.sent_at)),
    ),
    showChannel ? el("a", { class: "channel", href: href(`/c/${m.channel}`) }, `#${m.channel}`) : null,
    badgesFor(m.badges).map((b) => el("span", { class: `badge badge-${b.key}`, title: b.title, "aria-label": b.title }, b.icon)),
    name,
    ": ",
    m.text,
  );
}

const LIVE_POLL_MS = 2000;

/**
 * A message list that loads older pages on demand. `live()` starts polling for
 * new messages, adding them at the top, and returns a function that stops it.
 */
function pagedMessages(params, { showChannel = false, emptyText = "No messages." } = {}) {
  const list = el("ul", { class: "messages" });
  const footer = el("div", { class: "more" });
  const root = el("div", {}, list, footer);
  let newestId = 0;

  async function load(before) {
    footer.replaceChildren(el("span", { class: "muted" }, "Loading…"));
    try {
      const page = await api("/messages", { ...params, before });
      for (const m of page.messages) newestId = Math.max(newestId, m.id);
      list.append(...page.messages.map((m) => messageRow(m, { showChannel })));
      footer.replaceChildren();
      if (!list.children.length) footer.append(el("p", { class: "muted" }, emptyText));
      if (page.next) footer.append(el("button", { class: "secondary", onclick: () => load(page.next) }, "Load older messages"));
    } catch (e) {
      footer.replaceChildren(el("p", { class: "error" }, `Failed to load messages: ${e.message}`));
    }
  }

  async function poll() {
    const page = await api("/messages", { ...params, after: newestId, limit: 200 });
    if (!page.messages.length) return;
    newestId = Math.max(newestId, ...page.messages.map((m) => m.id));
    const rows = page.messages.map((m) => {
      const row = messageRow(m, { showChannel });
      row.classList.add("new");
      return row;
    });
    list.prepend(...rows);
    if (list.children.length === rows.length) footer.querySelector("p.muted")?.remove();
  }

  function live() {
    let stopped = false;
    let timer;
    const tick = async () => {
      // Skip polls while the tab is hidden; a busy chat catches up on return.
      if (!document.hidden) await poll().catch(() => {});
      if (!stopped) timer = setTimeout(tick, LIVE_POLL_MS);
    };
    ready.then(() => {
      if (!stopped) timer = setTimeout(tick, LIVE_POLL_MS);
    });
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
  }

  const ready = load(null);
  return { root, ready, live };
}

function liveBadge() {
  return el("span", { class: "live", role: "img", "aria-label": "Live", title: "Live: new messages appear automatically" });
}

function statTile(label, value, sub) {
  return el(
    "div",
    { class: "tile" },
    el("div", { class: "label" }, label),
    el("div", { class: "value" }, value),
    sub ? el("div", { class: "sub" }, sub) : null,
  );
}

function rangeFilter(path, params, current) {
  return el(
    "div",
    { class: "segmented", role: "group", "aria-label": "Time range" },
    RANGES.map((r) =>
      el("a", { href: href(path, { ...params, range: r.key }), class: r.key === current ? "selected" : null }, r.label),
    ),
  );
}

// ---------- activity chart ----------

/** Rounds up to 1, 2 or 5 × 10^k. */
function niceStep(raw) {
  if (raw <= 0) return 1;
  const pow = 10 ** Math.floor(Math.log10(raw));
  for (const m of [1, 2, 5, 10]) if (raw <= m * pow) return m * pow;
  return 10 * pow;
}

const TICK_INTERVALS = [MINUTE, 5 * MINUTE, 15 * MINUTE, 30 * MINUTE, HOUR, 3 * HOUR, 6 * HOUR, 12 * HOUR, DAY, 2 * DAY, 7 * DAY, 30 * DAY];

/**
 * Messages-over-time line chart with a hover crosshair. `points` must be dense
 * (one entry per bucket, zeros included). Clicking a point calls `onSelect`.
 */
function activityChart(container, points, bucket, { onSelect } = {}) {
  const height = 160;
  const margin = { top: 8, right: 8, bottom: 22, left: 40 };
  const tooltip = el("div", { class: "tooltip", hidden: true });
  let svgNode = null;

  function draw() {
    const width = Math.max(container.clientWidth, 280);
    const innerW = width - margin.left - margin.right;
    const innerH = height - margin.top - margin.bottom;
    const t0 = points[0].t;
    const t1 = points[points.length - 1].t;
    const maxN = Math.max(1, ...points.map((p) => p.n));
    const step = niceStep(maxN / 4);
    const yMax = Math.ceil(maxN / step) * step;
    const x = (t) => margin.left + (t1 === t0 ? innerW / 2 : ((t - t0) / (t1 - t0)) * innerW);
    const y = (n) => margin.top + innerH - (n / yMax) * innerH;

    const root = svg("svg", {
      viewBox: `0 0 ${width} ${height}`,
      height,
      tabindex: 0,
      role: "img",
      "aria-label": `Messages per ${bucket.label}. Use arrow keys to inspect values.`,
    });

    for (let v = 0; v <= yMax; v += step) {
      root.append(svg("line", { class: v === 0 ? "baseline" : "gridline", x1: margin.left, x2: width - margin.right, y1: y(v), y2: y(v) }));
      const label = svg("text", { class: "tick", x: margin.left - 8, y: y(v) + 4, "text-anchor": "end" });
      label.textContent = fmtCompact(v);
      root.append(label);
    }

    const span = t1 - t0;
    const maxTicks = Math.max(2, Math.floor(innerW / 90));
    const interval = TICK_INTERVALS.find((iv) => span / iv <= maxTicks) || TICK_INTERVALS[TICK_INTERVALS.length - 1];
    const tickFmt = interval >= DAY ? dayFmt : span > DAY ? dayTimeFmt : timeFmt;
    for (let t = alignDown(t0, interval) + (alignDown(t0, interval) < t0 ? interval : 0); t <= t1; t += interval) {
      const label = svg("text", { class: "tick", x: x(t), y: height - 6, "text-anchor": "middle" });
      label.textContent = tickFmt.format(new Date(t));
      root.append(label);
    }

    const line = points.map((p, i) => `${i ? "L" : "M"}${x(p.t).toFixed(1)},${y(p.n).toFixed(1)}`).join("");
    if (points.length > 1) {
      root.append(svg("path", { class: "area", d: `${line}L${x(t1)},${y(0)}L${x(t0)},${y(0)}Z` }));
      root.append(svg("path", { class: "line", d: line }));
    }

    const crosshair = svg("line", { class: "crosshair", y1: margin.top, y2: margin.top + innerH, visibility: "hidden" });
    const marker = svg("circle", { class: "marker", r: 4, visibility: points.length > 1 ? "hidden" : "visible", cx: x(t0), cy: y(points[0].n) });
    const hit = svg("rect", { class: "hit", x: margin.left - 8, y: 0, width: innerW + 16, height });
    root.append(crosshair, marker, hit);

    let active = -1;
    function show(i) {
      active = Math.max(0, Math.min(points.length - 1, i));
      const p = points[active];
      const px = x(p.t);
      for (const [node, attrs] of [
        [crosshair, { x1: px, x2: px, visibility: "visible" }],
        [marker, { cx: px, cy: y(p.n), visibility: "visible" }],
      ]) {
        for (const [k, v] of Object.entries(attrs)) node.setAttribute(k, v);
      }
      tooltip.replaceChildren(
        el("div", { class: "tt-value" }, el("span", { class: "tt-key" }), `${fmtInt(p.n)} message${p.n === 1 ? "" : "s"}`),
        el("div", { class: "tt-label" }, `${dayTimeFmt.format(new Date(p.t))} – ${timeFmt.format(new Date(p.t + bucket.ms))}`),
        onSelect ? el("div", { class: "tt-hint" }, "Click to read this chat") : null,
      );
      tooltip.hidden = false;
      const left = Math.min(Math.max(px - tooltip.offsetWidth / 2, 0), width - tooltip.offsetWidth);
      tooltip.style.left = `${left}px`;
      tooltip.style.top = `${Math.max(y(p.n) - tooltip.offsetHeight - 14, 0)}px`;
    }
    function hide() {
      active = -1;
      crosshair.setAttribute("visibility", "hidden");
      if (points.length > 1) marker.setAttribute("visibility", "hidden");
      tooltip.hidden = true;
    }
    function nearest(evt) {
      const rect = root.getBoundingClientRect();
      const px = ((evt.clientX - rect.left) / rect.width) * width;
      const frac = t1 === t0 ? 0 : (px - margin.left) / innerW;
      return Math.round(frac * (points.length - 1));
    }

    hit.addEventListener("pointermove", (e) => show(nearest(e)));
    hit.addEventListener("pointerleave", hide);
    hit.addEventListener("click", (e) => onSelect && onSelect(points[nearest(e)]));
    root.addEventListener("focus", () => show(active < 0 ? points.length - 1 : active));
    root.addEventListener("blur", hide);
    root.addEventListener("keydown", (e) => {
      if (e.key === "ArrowLeft") show(active - 1);
      else if (e.key === "ArrowRight") show(active + 1);
      else if (e.key === "Home") show(0);
      else if (e.key === "End") show(points.length - 1);
      else if (e.key === "Enter" && active >= 0 && onSelect) onSelect(points[active]);
      else return;
      e.preventDefault();
    });

    if (svgNode) svgNode.replaceWith(root);
    else container.prepend(root);
    svgNode = root;
  }

  container.append(tooltip);
  draw();
  let lastWidth = container.clientWidth;
  const observer = new ResizeObserver(() => {
    if (container.clientWidth !== lastWidth) {
      lastWidth = container.clientWidth;
      draw();
    }
  });
  observer.observe(container);
  return () => observer.disconnect();
}

/** Expands sparse `[t, n]` pairs into one point per bucket from `start` to `end`. */
function densify(pairs, bucketMs, start, end) {
  const byT = new Map(pairs);
  const out = [];
  for (let t = alignDown(start, bucketMs); t <= end; t += bucketMs) out.push({ t, n: byT.get(t) || 0 });
  if (out.length > 5000) return pairs.map(([t, n]) => ({ t, n }));
  return out;
}

function dataTable(points, bucket) {
  const rows = points.filter((p) => p.n > 0);
  return el(
    "details",
    { class: "data-table" },
    el("summary", {}, "Show data table"),
    el(
      "div",
      { class: "scroll" },
      el(
        "table",
        {},
        el("thead", {}, el("tr", {}, el("th", {}, `${bucket.label[0].toUpperCase()}${bucket.label.slice(1)} starting`), el("th", { class: "num" }, "Messages"))),
        el("tbody", {}, rows.map((p) => el("tr", {}, el("td", {}, fmtDateTime(p.t)), el("td", { class: "num" }, fmtInt(p.n))))),
      ),
    ),
  );
}

const COLLAPSED_KEY = "collapsed-panels";
const collapsedPanels = new Set(JSON.parse(localStorage.getItem(COLLAPSED_KEY) || "[]"));

/**
 * A card with a clickable heading that collapses its body. `key` identifies
 * the panel across views and reloads so its open/closed state is remembered.
 */
function panel(key, title, ...children) {
  const node = el(
    "details",
    { class: "card panel", open: !collapsedPanels.has(key) },
    el("summary", {}, el("h2", {}, title)),
    el("div", { class: "panel-body" }, children),
  );
  node.addEventListener("toggle", () => {
    if (node.open) collapsedPanels.delete(key);
    else collapsedPanels.add(key);
    localStorage.setItem(COLLAPSED_KEY, JSON.stringify([...collapsedPanels]));
  });
  return node;
}

// ---------- views ----------

/** Accepts `name`, `#name` or a twitch.tv link; the server validates the rest. */
function channelFromInput(value) {
  return value
    .trim()
    .replace(/^(https?:\/\/)?(www\.|m\.)?twitch\.tv\//i, "")
    .split(/[/?#]/, 1)[0] || value.trim();
}

/** Lists the channels the recorder is in, with a form to add and remove them. */
function watchedPanel(initial) {
  const list = el("div");
  const error = el("p", { class: "error", hidden: true });
  const input = el("input", { type: "text", name: "channel", placeholder: "Channel name or twitch.tv link", "aria-label": "Channel to record", autocomplete: "off", spellcheck: "false" });
  const addButton = el("button", { type: "submit" }, "Add");

  function showError(message) {
    error.textContent = message;
    error.hidden = !message;
  }

  async function change(request) {
    showError("");
    try {
      draw(await request());
      return true;
    } catch (e) {
      showError(e.message);
      return false;
    }
  }

  function draw(watched) {
    if (!watched.length) {
      list.replaceChildren(el("p", { class: "muted" }, "Not recording any channels yet. Add one above."));
      return;
    }
    list.replaceChildren(
      el(
        "table",
        {},
        el("thead", {}, el("tr", {}, el("th", {}, "Channel"), el("th", {}, "Last message"), el("th", { class: "num" }, ""))),
        el(
          "tbody",
          {},
          watched.map((w) =>
            el(
              "tr",
              {},
              el("td", {}, w.last_message_at ? el("a", { href: href(`/c/${w.channel}`) }, w.channel) : w.channel),
              el(
                "td",
                { class: "muted", title: w.last_message_at ? fmtDateTime(w.last_message_at) : null },
                w.last_message_at ? fmtAgo(w.last_message_at) : "No messages yet",
              ),
              el(
                "td",
                { class: "num" },
                w.pinned
                  ? el("span", { class: "muted", title: "Set on the recorder's command line; remove it there." }, "Config")
                  : el(
                      "button",
                      {
                        type: "button",
                        class: "small",
                        "aria-label": `Stop recording ${w.channel}`,
                        onclick: () => change(() => apiSend("DELETE", `/watched/${encodeURIComponent(w.channel)}`)),
                      },
                      "Remove",
                    ),
              ),
            ),
          ),
        ),
      ),
    );
  }

  const form = el(
    "form",
    {
      class: "filters",
      onsubmit: async (e) => {
        e.preventDefault();
        const channel = channelFromInput(input.value);
        if (!channel) return;
        addButton.disabled = true;
        if (await change(() => apiSend("POST", "/watched", { channel }))) input.value = "";
        addButton.disabled = false;
        input.focus();
      },
    },
    input,
    addButton,
  );

  draw(initial);
  return panel(
    "watched",
    "Recording",
    form,
    error,
    list,
    el("p", { class: "muted hint" }, "The recorder joins or leaves channels within a few seconds."),
  );
}

async function viewOverview() {
  const [channels, storage, watched] = await Promise.all([api("/channels"), api("/storage"), api("/watched")]);
  if (!channels.length) {
    return el(
      "div",
      {},
      el("h1", {}, "Channels"),
      el("p", { class: "subtitle" }, "No messages recorded yet."),
      watchedPanel(watched),
      el(
        "div",
        { class: "card empty" },
        el("p", {}, "Add a channel above, and make sure the recorder is running: ", el("code", {}, "twitch-chat record --db chat.db"), "."),
      ),
    );
  }
  const total = channels.reduce((sum, c) => sum + c.messages, 0);
  return el(
    "div",
    {},
    el("h1", {}, "Channels"),
    el("p", { class: "subtitle" }, `${fmtInt(total)} messages across ${channels.length} channel${channels.length === 1 ? "" : "s"}`),
    storageTiles(storage),
    watchedPanel(watched),
    el(
      "div",
      { class: "card" },
      el(
        "table",
        {},
        el("thead", {}, el("tr", {}, el("th", {}, "Channel"), el("th", { class: "num" }, "Messages"), el("th", {}, "Recording since"), el("th", {}, "Last message"))),
        el(
          "tbody",
          {},
          channels.map((c) =>
            el(
              "tr",
              {},
              el("td", {}, el("a", { href: href(`/c/${c.channel}`) }, c.channel)),
              el("td", { class: "num" }, fmtInt(c.messages)),
              el("td", {}, fmtDateTime(c.first_at)),
              el("td", { title: fmtDateTime(c.last_at) }, fmtAgo(c.last_at)),
            ),
          ),
        ),
      ),
    ),
  );
}

function storageTiles(storage) {
  const perMessage = storage.messages ? storage.bytes / storage.messages : 0;
  return el(
    "div",
    { class: "tiles" },
    statTile("Database size", fmtBytes(storage.bytes), perMessage ? `about ${fmtBytes(perMessage)} per message` : null),
    statTile("Messages", fmtCompact(storage.messages), `${fmtInt(storage.messages)} total`),
    statTile(
      "Last 24 hours",
      fmtCompact(storage.messages_24h),
      perMessage ? `about ${fmtBytes(storage.messages_24h * perMessage)} per day at this rate` : null,
    ),
  );
}

async function viewChannel(channel, params) {
  const range = RANGES.find((r) => r.key === params.get("range")) || RANGES[1];
  const now = Date.now();
  const from = range.span ? now - range.span : null;
  const filter = { channel, from };

  const stats = await api("/stats", filter);
  const path = `/c/${channel}`;
  const header = [
    el("h1", {}, channel),
    el("p", { class: "subtitle" }, el("a", { href: `https://twitch.tv/${encodeURIComponent(channel)}`, target: "_blank", rel: "noopener" }, `twitch.tv/${channel}`)),
    el("div", { class: "filters" }, rangeFilter(path, {}, range.key), el("a", { href: href("/search", { channel }) }, "Search this channel")),
  ];

  if (!stats.messages) {
    return el("div", {}, header, el("div", { class: "card empty" }, `No messages in the last ${range.label}.`));
  }

  const start = from ?? stats.first_at;
  const end = range.span ? now : stats.last_at;
  const bucket = pickBucket(end - start);
  const [activity, chatters, repeated] = await Promise.all([
    api("/activity", { ...filter, bucket: bucket.ms, tz_offset: tzOffset() }),
    api("/top-chatters", { ...filter, limit: 15 }),
    api("/top-messages", { ...filter, limit: 15 }),
  ]);
  const points = densify(activity.points, activity.bucket, start, end);
  const peak = points.reduce((best, p) => (p.n > best.n ? p : best), points[0]);

  const chartBox = el("div", { class: "chart" });
  const latest = pagedMessages({ channel, limit: 50 });

  const view = el(
    "div",
    {},
    header,
    el(
      "div",
      { class: "tiles" },
      statTile("Messages", fmtCompact(stats.messages), `${fmtInt(stats.messages)} total`),
      statTile("Unique chatters", fmtCompact(stats.chatters), `${(stats.messages / stats.chatters).toFixed(1)} messages each on average`),
      statTile(`Busiest ${bucket.label}`, fmtCompact(peak.n), `${dayTimeFmt.format(new Date(peak.t))}`),
    ),
    panel("activity", `Messages per ${bucket.label}`, chartBox, dataTable(points, bucket)),
    el(
      "div",
      { class: "grid-2" },
      panel("top-chatters", "Top chatters", chattersTable(chatters, stats.messages)),
      panel("repeated-messages", "Most repeated messages", repeatedTable(repeated, channel, from)),
    ),
    panel("latest-messages", ["Latest messages", liveBadge()], latest.root),
  );

  view.onMount = () => {
    const stopChart = activityChart(chartBox, points, bucket, {
      onSelect: (p) => (location.hash = href("/search", { channel, from: p.t, to: p.t + bucket.ms })),
    });
    const stopLive = latest.live();
    return () => {
      stopChart();
      stopLive();
    };
  };
  return view;
}

/**
 * A ranked list: one fixed-height row per item, with a bar behind it scaled
 * to `value` relative to the first (largest) item.
 * `columns` labels the header: [label, sub, value].
 */
function rankList(columns, items) {
  const max = items[0].value;
  return el(
    "ol",
    { class: "rank" },
    el("li", { class: "rank-head", "aria-hidden": "true" }, el("span"), ...columns.map((c) => el("span", {}, c))),
    items.map((item, i) => {
      const row = el(
        "li",
        {},
        el("span", { class: "rank-n" }, i + 1),
        el("a", { class: "rank-label", href: item.href, title: item.title }, item.label),
        el("span", { class: "rank-sub" }, item.sub),
        el("span", { class: "rank-value" }, fmtInt(item.value)),
      );
      row.style.setProperty("--w", `${Math.max((item.value / max) * 100, 1)}%`);
      return row;
    }),
  );
}

function chattersTable(chatters, total) {
  if (!chatters.length) return el("p", { class: "muted" }, "No chatters.");
  return rankList(
    ["Chatter", "Share", "Messages"],
    chatters.map((c) => ({
      label: c.display_name,
      href: c.user_id ? href(`/u/${encodeURIComponent(c.user_id)}`) : href("/search", { login: c.login }),
      sub: `${((c.messages / total) * 100).toFixed(1)}%`,
      value: c.messages,
    })),
  );
}

function repeatedTable(rows, channel, from) {
  if (!rows.length) return el("p", { class: "muted" }, "No message was sent more than once.");
  return rankList(
    ["Message", "Chatters", "Times"],
    rows.map((r) => ({
      label: r.text,
      title: r.text,
      href: href("/search", { channel, q: r.text, from }),
      sub: fmtInt(r.chatters),
      value: r.messages,
    })),
  );
}

async function viewSearch(params) {
  const current = Object.fromEntries(["q", "channel", "login", "user_id", "from", "to"].map((k) => [k, params.get(k) || ""]));
  const channels = await api("/channels");

  const qInput = el("input", { type: "search", name: "q", value: current.q, placeholder: "Words in the message…", "aria-label": "Message text" });
  const channelSelect = el(
    "select",
    { name: "channel", "aria-label": "Channel" },
    el("option", { value: "" }, "All channels"),
    channels.map((c) => el("option", { value: c.channel, selected: c.channel === current.channel }, c.channel)),
  );
  const suggestions = el("datalist", { id: "login-suggestions" });
  const loginInput = el("input", { type: "search", name: "login", value: current.login, placeholder: "Username", list: "login-suggestions", "aria-label": "Username", autocomplete: "off" });
  let suggestTimer;
  loginInput.addEventListener("input", () => {
    clearTimeout(suggestTimer);
    const q = loginInput.value.trim();
    if (q.length < 2) return;
    suggestTimer = setTimeout(async () => {
      const users = await api("/users", { q }).catch(() => []);
      suggestions.replaceChildren(...users.map((u) => el("option", { value: u.login }, `${u.display_name} · ${fmtInt(u.messages)} messages`)));
    }, 200);
  });

  const form = el(
    "form",
    {
      class: "filters",
      onsubmit: (e) => {
        e.preventDefault();
        // A typed username replaces a user_id carried over from a link.
        const login = loginInput.value.trim();
        location.hash = href("/search", {
          q: qInput.value.trim(),
          channel: channelSelect.value,
          login,
          user_id: login ? "" : current.user_id,
          from: current.from,
          to: current.to,
        });
      },
    },
    qInput,
    channelSelect,
    loginInput,
    suggestions,
    el("button", { type: "submit" }, "Search"),
  );

  const chips = [];
  if (current.from || current.to) {
    const from = current.from ? fmtDateTime(Number(current.from)) : "the beginning";
    const to = current.to ? timeFmt.format(new Date(Number(current.to))) : "now";
    chips.push(el("span", { class: "chip" }, `${from} – ${to}`, el("a", { href: href("/search", { ...current, from: "", to: "" }), "aria-label": "Clear time window" }, "✕")));
  }
  if (current.user_id) {
    chips.push(el("span", { class: "chip" }, "Single user", el("a", { href: href("/search", { ...current, user_id: "" }), "aria-label": "Clear user" }, "✕")));
  }

  const results = pagedMessages(
    { q: current.q, channel: current.channel, login: current.login, user_id: current.user_id, from: current.from, to: current.to },
    { showChannel: !current.channel, emptyText: "No messages match." },
  );
  await results.ready;
  // A window that ends in the past can't gain messages.
  const isLive = !current.to;

  const view = el(
    "div",
    {},
    el("h1", {}, "Search", isLive ? liveBadge() : null),
    el("p", { class: "subtitle" }, "Every word must appear; words match as prefixes (kek finds KEKW). Newest first."),
    form,
    chips.length ? el("div", { class: "filters" }, chips) : null,
    el("div", { class: "card" }, results.root),
  );
  view.onMount = () => {
    if (!current.q && !current.login) qInput.focus();
    return isLive ? results.live() : null;
  };
  return view;
}

async function viewContext(id) {
  const ctx = await api(`/messages/${id}/context`, { n: 40 });
  const focus = ctx.messages.find((m) => m.id === ctx.focus);
  const list = el("ul", { class: "messages" }, ctx.messages.map((m) => messageRow(m, { focus: m.id === ctx.focus })));
  const view = el(
    "div",
    {},
    el("h1", {}, `#${focus.channel}`),
    el(
      "p",
      { class: "subtitle" },
      `Chat around ${fmtDateTime(focus.sent_at)} · `,
      el("a", { href: href("/search", { channel: focus.channel, from: focus.sent_at - 5 * MINUTE, to: focus.sent_at + 5 * MINUTE }) }, "open ±5 minutes in search"),
    ),
    el("div", { class: "card" }, list),
  );
  view.onMount = () => {
    list.querySelector(".msg.focus")?.scrollIntoView({ block: "center" });
  };
  return view;
}

async function viewUser(userId) {
  const user = await api(`/users/${encodeURIComponent(userId)}`);
  const current = user.names[0];
  const total = user.channels.reduce((sum, c) => sum + c.messages, 0);
  const others = user.names.slice(1).filter((n) => n.login !== current.login || n.display_name !== current.display_name);
  const messages = pagedMessages({ user_id: userId, limit: 100 }, { showChannel: user.channels.length > 1 });
  await messages.ready;

  const view = el(
    "div",
    {},
    el("h1", {}, current.display_name),
    el(
      "p",
      { class: "subtitle" },
      `@${current.login} · user id ${user.user_id}`,
      others.length ? ` · previously ${others.map((n) => n.display_name).join(", ")}` : "",
    ),
    el(
      "div",
      { class: "tiles" },
      statTile("Messages", fmtCompact(total), `${fmtInt(total)} total`),
      statTile("First seen", dayFmt.format(new Date(Math.min(...user.channels.map((c) => c.first_at)))), fmtAgo(Math.min(...user.channels.map((c) => c.first_at)))),
      statTile("Last seen", dayFmt.format(new Date(current.last_at)), fmtAgo(current.last_at)),
    ),
    panel(
      "user-channels",
      "Channels",
      el(
        "table",
        {},
        el("thead", {}, el("tr", {}, el("th", {}, "Channel"), el("th", { class: "num" }, "Messages"), el("th", {}, "First seen"), el("th", {}, "Last seen"))),
        el(
          "tbody",
          {},
          user.channels.map((c) =>
            el(
              "tr",
              {},
              el("td", {}, el("a", { href: href("/search", { channel: c.channel, user_id: userId }) }, c.channel)),
              el("td", { class: "num" }, fmtInt(c.messages)),
              el("td", {}, fmtDateTime(c.first_at)),
              el("td", {}, fmtDateTime(c.last_at)),
            ),
          ),
        ),
      ),
    ),
    panel("user-messages", ["Messages", liveBadge()], messages.root),
  );
  view.onMount = () => messages.live();
  return view;
}

// ---------- router ----------

const main = document.getElementById("main");
let renderSeq = 0;
let unmount = null;

function parseHash() {
  const raw = location.hash.replace(/^#/, "") || "/";
  const [path, query = ""] = raw.split("?");
  return { parts: path.split("/").filter(Boolean).map(decodeURIComponent), params: new URLSearchParams(query) };
}

async function render() {
  const seq = ++renderSeq;
  const { parts, params } = parseHash();
  const [section, arg] = parts;

  for (const link of document.querySelectorAll("[data-nav]")) {
    const nav = link.getAttribute("data-nav");
    link.classList.toggle("active", nav === "search" ? section === "search" : !section || section === "c");
  }

  // Keep the previous page visible (dimmed) while the next one loads.
  main.classList.add("loading");
  let view;
  try {
    if (!section) view = await viewOverview();
    else if (section === "c" && arg) view = await viewChannel(arg.toLowerCase(), params);
    else if (section === "search") view = await viewSearch(params);
    else if (section === "m" && arg) view = await viewContext(arg);
    else if (section === "u" && arg) view = await viewUser(arg);
    else view = el("div", { class: "card empty" }, "Page not found. ", el("a", { href: "#/" }, "Back to channels"));
  } catch (e) {
    view = el("div", { class: "card empty" }, e.status === 404 ? "Not found." : el("span", { class: "error" }, `Something went wrong: ${e.message}`));
  }
  if (seq !== renderSeq) return;

  if (unmount) unmount();
  unmount = null;
  main.replaceChildren(view);
  main.classList.remove("loading");
  // onMount may return a cleanup function, run before the next page replaces this one.
  const cleanup = view.onMount ? view.onMount() : null;
  if (typeof cleanup === "function") unmount = cleanup;
  if (!location.hash.startsWith("#/m/")) window.scrollTo(0, 0);
  document.title = section === "c" && arg ? `${arg} · twitch-chat` : "twitch-chat";
}

window.addEventListener("hashchange", render);
render();
