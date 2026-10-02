/* Shared client for every Wayfarer page.
 *
 * Locking down the machine-facing routes meant browser pages had to carry a
 * token, and only fabric.html ever got one — dashboard, map and bodies-map
 * have been 401ing since. This is the one place that knows how to sign in, so
 * a page never has to reimplement it (and the next page can't forget to).
 */

const TOKEN_KEY = "tidw_token";

/* Same-origin. The old dashboard hardcoded localhost:3000, which broke the
 * moment the page was served from anywhere else. */
export const API = `${location.origin}/api`;

export const token = () => localStorage.getItem(TOKEN_KEY);
export const setToken = (t) => localStorage.setItem(TOKEN_KEY, t);
export const clearToken = () => localStorage.removeItem(TOKEN_KEY);

/* Authenticated fetch. A 401 means the token died — drop it and bounce to the
 * sign-in screen rather than rendering a page full of dashes. */
export async function api(path, options = {}) {
  const res = await fetch(`${API}${path}`, {
    ...options,
    headers: {
      "Content-Type": "application/json",
      ...(token() ? { Authorization: `Bearer ${token()}` } : {}),
      ...(options.headers || {}),
    },
  });

  if (res.status === 401) {
    clearToken();
    showLogin();
    throw new Error("unauthenticated");
  }
  if (res.status === 423) {
    throw new Error("This outpost is in lockdown and refused the change.");
  }
  if (!res.ok) {
    throw new Error((await res.text()) || `Request failed (${res.status})`);
  }
  return res.status === 204 ? null : res.json();
}

export async function login(email, password) {
  const res = await fetch(`${API}/local-auth/login`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ email, password }),
  });
  if (!res.ok) throw new Error("Those credentials weren't accepted.");
  const { token: t } = await res.json();
  setToken(t);
  return t;
}

export function logout() {
  clearToken();
  location.reload();
}

/* ---- Time since contact -------------------------------------------------
 * The organizing idea of this console. Everything here is potentially stale:
 * a value is only meaningful next to how old it is. */

/** Compact age: 4s, 12m, 3h, 2d. */
export function age(seconds) {
  if (seconds === null || seconds === undefined) return "—";
  const s = Math.max(0, Math.floor(seconds));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

export const ageSince = (iso) => age((Date.now() - new Date(iso).getTime()) / 1000);

/** Freshness class driving the four-state colour scale. */
export function freshness(seconds) {
  if (seconds === null || seconds === undefined) return "unknown";
  if (seconds < 60) return "live";
  if (seconds < 300) return "lagging";
  return "dark";
}

/* ---- DOM helpers -------------------------------------------------------- */

export const $ = (sel) => document.querySelector(sel);
export const el = (id) => document.getElementById(id);

export function setText(id, value) {
  const node = el(id);
  if (node) node.textContent = value;
}

/** Escape anything that came from the database before it reaches innerHTML. */
export function esc(value) {
  return String(value ?? "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  }[c]));
}

/* ---- Sign-in gate ------------------------------------------------------- */

export function showLogin() {
  const login = el("login");
  const app = el("app");
  if (login) login.style.display = "block";
  if (app) app.style.display = "none";
}

export function showApp() {
  const login = el("login");
  const app = el("app");
  if (login) login.style.display = "none";
  if (app) app.style.display = "block";
}


/* ---- Scanning -------------------------------------------------------------
 * Two ways a code gets in, and the first matters more than people expect.
 *
 * Most real warehouse scanning is a USB or Bluetooth scanner that presents as
 * a keyboard: it types the code and presses Enter, faster than a human can.
 * That needs no camera permission, no library, and works on a machine with no
 * camera at all — so it is handled first, by watching for a burst of keystrokes
 * ending in Enter.
 *
 * Phone cameras use the browser's own BarcodeDetector where it exists. It is
 * deliberately not polyfilled: a scanning library is hundreds of kilobytes an
 * offline outpost would have to carry, and the hardware-scanner path already
 * covers the case that matters most. Where the API is absent the button simply
 * is not offered, and typing still works.
 */

/** A hardware scanner types much faster than a person. */
const SCANNER_MAX_GAP_MS = 35;
const SCANNER_MIN_LENGTH = 4;

export const canScanWithCamera = () => "BarcodeDetector" in window;

/**
 * Watch an input for hardware-scanner input.
 *
 * Returns a detach function. `onScan` fires only for a burst that looks
 * machine-typed, so a person typing a code by hand and pressing Enter still
 * submits the form normally rather than being hijacked.
 */
export function watchForScanner(input, onScan) {
  let last = 0;
  let fast = 0;

  const onKey = (e) => {
    const now = performance.now();
    const gap = now - last;
    last = now;

    if (e.key === "Enter") {
      const value = input.value.trim();
      // Only treat it as a scan if most of it arrived at machine speed.
      if (value.length >= SCANNER_MIN_LENGTH && fast >= value.length - 2) {
        e.preventDefault();
        fast = 0;
        onScan(value);
      }
      fast = 0;
      return;
    }
    if (e.key.length === 1) fast = gap < SCANNER_MAX_GAP_MS ? fast + 1 : 0;
  };

  input.addEventListener("keydown", onKey);
  return () => input.removeEventListener("keydown", onKey);
}

/**
 * Open the camera and resolve the first code seen.
 *
 * The stream is stopped on every exit path — a camera left running is both a
 * battery drain and a light nobody can explain.
 */
export async function scanWithCamera({ onStatus } = {}) {
  if (!canScanWithCamera()) throw new Error("This browser cannot scan with the camera.");

  const detector = new window.BarcodeDetector({
    formats: ["qr_code", "code_128", "code_39", "ean_13", "ean_8", "upc_a", "upc_e", "data_matrix"],
  });

  let stream;
  try {
    stream = await navigator.mediaDevices.getUserMedia({
      video: { facingMode: "environment" },   // the back camera, on a phone
    });
  } catch {
    throw new Error("Camera access was refused.");
  }

  const overlay = document.createElement("div");
  overlay.className = "scanoverlay";
  overlay.innerHTML = `
    <div class="scanbox">
      <video playsinline muted></video>
      <div class="scanframe"></div>
      <div class="scanfoot">
        <span class="scanstatus">Point the camera at a code</span>
        <button type="button" class="ghost scancancel">Cancel</button>
      </div>
    </div>`;
  document.body.appendChild(overlay);

  const video = overlay.querySelector("video");
  video.srcObject = stream;
  await video.play().catch(() => {});

  const status = (t) => {
    overlay.querySelector(".scanstatus").textContent = t;
    onStatus?.(t);
  };

  return new Promise((resolve, reject) => {
    let done = false;
    const finish = (fn, arg) => {
      if (done) return;
      done = true;
      stream.getTracks().forEach((t) => t.stop());
      overlay.remove();
      fn(arg);
    };

    overlay.querySelector(".scancancel").addEventListener("click", () => finish(resolve, null));
    overlay.addEventListener("click", (e) => { if (e.target === overlay) finish(resolve, null); });

    const tick = async () => {
      if (done) return;
      try {
        const found = await detector.detect(video);
        if (found.length) {
          status(`Read ${found[0].rawValue}`);
          return finish(resolve, found[0].rawValue);
        }
      } catch {
        // A detect() failure on one frame is normal while focusing.
      }
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);

    // Give up rather than hold the camera open indefinitely.
    setTimeout(() => finish(reject, new Error("No code was read.")), 45000);
  });
}

/**
 * Turn an input into a scan field: hardware scanner, a camera button where
 * supported, and typing. `onCode` receives the code however it arrived.
 */
export function makeScannable(input, onCode) {
  watchForScanner(input, onCode);

  if (!canScanWithCamera()) return;
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "scanbtn";
  btn.title = "Scan with the camera";
  btn.textContent = "Scan";
  input.insertAdjacentElement("afterend", btn);
  btn.addEventListener("click", async () => {
    try {
      const code = await scanWithCamera();
      if (code) { input.value = code; onCode(code); }
    } catch (e) {
      alert(e.message);
    }
  });
}

/* ---- Navigation ----------------------------------------------------------
 * One list, so a new page cannot be added and then be unreachable — which is
 * how map.html and bodies-map.html ended up orphaned. Order follows the
 * question each page answers, from "what needs me now" outward. */

export const PAGES = [
  ["console.html",    "Console",    "what needs me right now"],
  ["resources.html",  "Resources",  "every resource, everywhere"],
  ["catalogue.html",  "Catalogue",  "what each resource is: part numbers, specs"],
  ["forecast.html",   "Resupply",   "what runs out, and when"],
  ["lots.html",       "Lots",       "batches, serials, expiry, recall"],
  ["market.html",     "Supply",     "orders, bids, settlement"],
  ["capsules.html",   "Capsules",   "shared hulls and manifests"],
  ["compliance.html", "Compliance", "certificates and holds"],
  ["map.html",        "Map",        "where things are"],
  ["messages.html",   "Messages",   "colleagues, and the other side of a deal"],
  ["settings.html",   "Settings",   "your account, and who else has one"],
];

export function mountNav(current, ident) {
  const header = $("header");
  if (!header) return;
  header.innerHTML = `
    <h1>Wayfarer <span class="ident">${esc(ident ?? current.replace(".html", ""))}</span></h1>
    <nav>
      ${PAGES.map(([href, label, title]) =>
        `<a href="/static/${href}" title="${esc(title)}"${
          href === current ? ' aria-current="page"' : ""}>${esc(label)}</a>`).join("")}
      <button class="linkish" id="signout" type="button">Sign out</button>
    </nav>`;
  const out = el("signout");
  if (out) out.addEventListener("click", logout);
  markUnread();
}

/* The unread count rides on the nav rather than only on the Messages page,
 * because a question from the other side of a deal is time-sensitive and
 * nobody is going to sit on one tab waiting for it.
 *
 * Deliberately silent on failure: a chat endpoint that is down must not put an
 * error on every page in the application. */
async function markUnread() {
  const link = document.querySelector('nav a[href$="messages.html"]');
  if (!link) return;
  try {
    const { unread } = await api("/chat/unread");
    link.textContent = unread ? `Messages (${unread})` : "Messages";
    link.classList.toggle("has-unread", unread > 0);
  } catch {
    /* leave the label as it was */
  }
}

/* ---- Fabric bar ----------------------------------------------------------
 * Which outpost you are signed in to, who else is in the fabric, and how to
 * reach them. The console could report a count of online nodes but never said
 * *which* or *where*, so an operator who needed the farm's console had no way
 * to find it.
 *
 * Following a peer link lands on that outpost's own sign-in. Outposts are
 * sovereign and hold their own user tables, so there is genuinely no shared
 * session — the bar says so rather than letting a link imply otherwise. */

const PEER_TONE = {
  self: "ok", online: "ok", lagging: "wait", dark: "bad",
  unknown: "unknown", revoked: "bad",
};

export async function mountFabricBar(hostId = "fabricbar") {
  const host = el(hostId);
  if (!host) return;
  let f;
  try {
    f = await api("/fabric/outposts");
  } catch {
    host.innerHTML = "";               // never block a page on this
    return;
  }

  const peers = f.peers.map((p) => {
    const tone = PEER_TONE[p.status] ?? "idle";
    const label = esc(p.name ?? p.nodeId.slice(0, 8));
    const age = p.status === "self" ? "this outpost"
      : p.ageSeconds == null ? "never heard from"
      : `${age_(p.ageSeconds)} ago`;
    const inner = `<span class="pill ${tone}">${p.status}</span>
       <span class="peer-name">${label}</span>
       <span class="peer-age">${esc(age)}</span>`;
    return p.ui
      ? `<a class="peer" href="${esc(p.ui)}" title="Sign in at ${label}">${inner}</a>`
      : `<span class="peer current">${inner}</span>`;
  }).join("");

  // Which organisation the page is showing. Every read of business data is
  // filtered by it, so without this an empty page looks like a broken one.
  let orgChip = "";
  try {
    const me = await api("/me/profile");
    if (me.org_name) {
      orgChip = `<span class="orgchip" title="${
        me.org_count > 1
          ? `You belong to ${me.org_count} organisations; this is the one being shown`
          : "All data on this page belongs to this organisation"}">
        <span class="label">Org</span> ${esc(me.org_name)}${
        me.org_count > 1 ? ` <span class="orgmore">+${me.org_count - 1}</span>` : ""}</span>`;
    } else {
      // Fails closed, so say so rather than leaving an unexplained empty page.
      orgChip = `<span class="orgchip none" title="You are not a member of any organisation, so no business data is visible">
        <span class="label">Org</span> none</span>`;
    }
  } catch { /* older core */ }

  host.innerHTML = `
    <div class="fabricbar">
      <span class="label">Fabric</span>
      <span class="fabric-count">${f.online}/${f.total} reporting</span>
      ${orgChip}
      <div class="peers">${peers}</div>
      <span class="fabric-note">${esc(f.note)}</span>
    </div>`;
}

const age_ = (s) => age(s);

/* ---- Search --------------------------------------------------------------
 * One box over resources, lots and serials, orders, capsules and rates. A
 * person hunting a lot code and a person hunting "oxygen" are asking the same
 * question and should not have to know which table it lives in. */

const KIND_LABEL = {
  resource: "resource", lot: "lot / serial", order: "order",
  capsule: "capsule", rate: "rate card",
};

export function mountSearch(hostId = "searchbar", { placeholder } = {}) {
  const host = el(hostId);
  if (!host) return;

  host.innerHTML = `
    <div class="searchwrap">
      <input id="q" type="search" autocomplete="off" spellcheck="false"
             placeholder="${esc(placeholder ?? "Search resources, lot codes, serials, orders, capsules…")}" />
      <div class="searchresults" id="qresults" hidden></div>
    </div>`;

  const input = el("q");
  const panel = el("qresults");
  let timer = null;

  const close = () => { panel.hidden = true; panel.innerHTML = ""; };

  const run = async (term) => {
    if (term.trim().length < 2) return close();
    let r;
    try {
      r = await api(`/search?q=${encodeURIComponent(term.trim())}`);
    } catch (e) {
      panel.hidden = false;
      panel.innerHTML = `<div class="searchempty">${esc(e.message)}</div>`;
      return;
    }
    panel.hidden = false;
    if (!r.count) {
      panel.innerHTML = `<div class="searchempty">Nothing matches “${esc(term)}”.</div>`;
      return;
    }
    panel.innerHTML = `
      <div class="searchcount">${r.count} result${r.count === 1 ? "" : "s"}</div>
      ${r.results.map((x) => `
        <div class="hit${x.needsReorder || x.expired ? " flagged" : ""}">
          <span class="hitkind">${esc(KIND_LABEL[x.kind] ?? x.kind)}</span>
          <div class="hitbody">
            <strong>${esc(x.title)}</strong>
            ${x.subtitle ? `<span class="hitsub">${esc(x.subtitle)}</span>` : ""}
          </div>
          <span class="hitdetail">${esc(x.detail ?? "")}</span>
          ${x.needsReorder ? `<span class="pill bad">reorder</span>` : ""}
          ${x.expired ? `<span class="pill bad">expired</span>` : ""}
        </div>`).join("")}`;
  };

  // Debounced: a keystroke per character would hammer an outpost that may be
  // on a thin link.
  input.addEventListener("input", () => {
    clearTimeout(timer);
    timer = setTimeout(() => run(input.value), 220);
  });
  input.addEventListener("keydown", (e) => { if (e.key === "Escape") { input.value = ""; close(); } });
  document.addEventListener("click", (e) => {
    if (!host.contains(e.target)) close();
  });
}

/**
 * Everything a page needs that is not its own content: the sign-in panel, the
 * header and nav, the fabric bar and the search box.
 *
 * Exists so adding a page is writing its content and nothing else. The pages
 * that predate it each hand-rolled a login block, which is how one of them
 * ended up with no token and 401ing silently for weeks.
 */
export function page({ current, ident, blurb, onReady, intervalMs = 15000, search = true }) {
  const login = el("login");
  if (login && !login.querySelector("#login-form")) {
    login.innerHTML = `
      <h2>${esc(ident ?? "Wayfarer")}</h2>
      <p>${esc(blurb ?? "Sign in to continue.")}</p>
      <form id="login-form">
        <label class="field"><span class="label">Email</span>
          <input id="email" type="email" autocomplete="username" required /></label>
        <label class="field"><span class="label">Password</span>
          <input id="password" type="password" autocomplete="current-password" required /></label>
        <button type="submit">Sign in</button>
        <div class="err" id="login-err"></div>
      </form>
      <p class="muted" style="font-size:13px;margin-top:14px">
        No account? <a href="/static/signup.html">Create one</a>.
      </p>`;
  }

  boot(() => {
    mountNav(current, ident);
    mountFabricBar();
    if (search) mountSearch();
    return Promise.resolve(onReady()).catch((e) => console.error(e));
  }, intervalMs);
}

/**
 * Wire the standard sign-in panel and start the page.
 * `onReady` runs once a token is present and is re-run on `intervalMs`.
 */
export function boot(onReady, intervalMs = 5000) {
  const form = el("login-form");
  if (form) {
    form.addEventListener("submit", async (e) => {
      e.preventDefault();
      const err = el("login-err");
      if (err) err.textContent = "";
      try {
        await login(el("email").value, el("password").value);
        showApp();
        onReady();
      } catch (ex) {
        if (err) err.textContent = ex.message;
      }
    });
  }

  const out = el("signout");
  if (out) out.addEventListener("click", logout);

  if (!token()) {
    showLogin();
    return;
  }
  showApp();
  onReady();
  if (intervalMs) setInterval(() => { if (token()) onReady(); }, intervalMs);
}
