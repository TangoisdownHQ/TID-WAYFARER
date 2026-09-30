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
