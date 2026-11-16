import { api, setCsrf } from "./js/api.js";
import { icon } from "./js/icons.js";
import { esc } from "./js/format.js";
import { toast } from "./js/ui.js";
import * as overview from "./js/pages/overview.js";
import * as live from "./js/pages/live.js";
import * as vod from "./js/pages/vod.js";
import * as paths from "./js/pages/paths.js";
import * as keys from "./js/pages/keys.js";
import * as proxies from "./js/pages/proxies.js";
import * as srt from "./js/pages/srt.js";
import * as recordings from "./js/pages/recordings.js";
import * as system from "./js/pages/system.js";

const BASE = window.__ADMIN_BASE__ || "/admin";
const app = document.getElementById("app");
const content = () => document.getElementById("page-content");
const routes = {
  overview: { page: overview, group: "运行", label: "概览" },
  live: { page: live, group: "运行", label: "直播流" },
  vod: { page: vod, group: "运行", label: "点播" },
  "config/paths": { page: paths, group: "配置", label: "路径" },
  "config/keys": { page: keys, group: "配置", label: "推流密钥" },
  "config/proxies": { page: proxies, group: "配置", label: "RTSP 代理" },
  "config/srt": { page: srt, group: "配置", label: "SRT" },
  "config/recordings": { page: recordings, group: "配置", label: "录制" },
  system: { page: system, group: "系统", label: "系统" },
};
const state = { session: null, route: "overview", loading: false };
let pageCleanup = null;

const navGroups = [
  ["运行", [["overview", "概览"], ["live", "直播流"], ["vod", "点播"]]],
  ["配置", [["config/paths", "路径"], ["config/keys", "推流密钥"], ["config/proxies", "RTSP 代理"], ["config/srt", "SRT"], ["config/recordings", "录制"]]],
  ["系统", [["system", "系统"]]],
];
const navIcons = { overview: "overview", live: "live", vod: "vod", "config/paths": "paths", "config/keys": "keys", "config/proxies": "proxy", "config/srt": "srt", "config/recordings": "record", system: "system" };

function currentRoute() {
  let route = (location.hash || "").replace(/^#\/?/, "").split("?")[0];
  if (route === "control") route = "config/paths";
  return routes[route] ? route : "overview";
}

function theme() {
  return localStorage.getItem("rtflow-theme") || "dark";
}
function applyTheme(value) {
  document.documentElement.dataset.theme = value;
  localStorage.setItem("rtflow-theme", value);
}
applyTheme(theme());

async function boot() {
  try {
    const session = await api("/session");
    if (!session.authenticated) return renderLogin();
    state.session = session;
    setCsrf(session.csrf);
    renderShell();
    await navigate();
  } catch (error) {
    renderLogin(error.message);
  }
}

function renderLogin(message = "") {
  app.innerHTML = `<main class="login-wrap"><div class="login-card">
    <div class="brand"><span class="brand-logo">R</span><span>${esc(state.session?.title || "rtflow")}</span></div>
    <h1>管理控制台</h1><p class="sub">登录以管理流、路径、代理与系统配置。</p>
    <form id="login-form" class="stack">
      ${message ? `<div class="form-error">${esc(message)}</div>` : ""}
      <div class="field"><label for="login-user">用户名</label><input class="input" id="login-user" name="username" autocomplete="username" required autofocus></div>
      <div class="field"><label for="login-pass">密码</label><input class="input" id="login-pass" name="password" type="password" autocomplete="current-password" required></div>
      <button class="btn primary block" type="submit">登录</button>
    </form>
    <div class="login-foot">rtflow admin · ${esc(location.host)}</div>
  </div></main>`;
  const form = document.getElementById("login-form");
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const button = form.querySelector("button");
    button.disabled = true;
    try {
      await api("/login", { method: "POST", body: { username: form.username.value, password: form.password.value } });
      await boot();
    } catch (error) {
      renderLogin(error.message);
    }
  });
}

function renderShell() {
  app.innerHTML = `<div class="layout">
    <aside class="sidebar" id="sidebar">
      <div class="brand"><span class="brand-logo">R</span><span>rtflow</span></div>
      ${navGroups.map(([label, items]) => `<nav class="nav-group" aria-label="${label}">
        <div class="nav-label">${label}</div>
        ${items.map(([route, text]) => `<a class="nav-item" data-route="${route}" href="#/${route}">${icon(navIcons[route] || "overview")}<span>${text}</span></a>`).join("")}
      </nav>`).join("")}
      <div class="sidebar-foot"><span>rtflow ${esc(state.session?.version || "")}</span><span>${state.session?.readonly ? "只读模式" : "管理模式"}</span></div>
    </aside>
    <div class="main">
      <header class="topbar">
        <button class="btn ghost small menu-btn" data-menu aria-label="打开菜单">${icon("menu")}</button>
        <div class="crumbs" id="crumbs"></div>
        <div class="top-actions">
          <span class="health"><span class="dot ok"></span>运行中</span>
          <button class="btn ghost small" data-theme-toggle aria-label="切换主题">${icon("sun")}</button>
          <span class="user-chip"><span class="avatar">${esc((state.session?.username || "A").slice(0, 1).toUpperCase())}</span><span>${esc(state.session?.username || "admin")}</span></span>
          <button class="btn ghost small" data-logout aria-label="退出登录">${icon("logout")}</button>
        </div>
      </header>
      <main class="content" id="content"><div id="page-content"></div></main>
    </div>
  </div>`;
  app.querySelectorAll("[data-route]").forEach((link) => link.addEventListener("click", () => {
    document.getElementById("sidebar").classList.remove("open");
  }));
  app.querySelector("[data-menu]").onclick = () => document.getElementById("sidebar").classList.toggle("open");
  app.querySelector("[data-theme-toggle]").onclick = () => applyTheme(document.documentElement.dataset.theme === "light" ? "dark" : "light");
  app.querySelector("[data-logout]").onclick = async () => {
    try { await api("/logout", { method: "POST" }); } catch (_) {}
    state.session = null;
    renderLogin();
  };
}

function setCrumbs(route) {
  const info = routes[route];
  const crumbs = document.getElementById("crumbs");
  if (!crumbs) return;
  crumbs.innerHTML = `<span>${info.group}</span><span>/</span><b>${info.label}</b>`;
  app.querySelectorAll("[data-route]").forEach((link) => {
    const active = link.dataset.route === route;
    link.classList.toggle("active", active);
    if (active) link.setAttribute("aria-current", "page"); else link.removeAttribute("aria-current");
  });
}

async function navigate(options = {}) {
  if (!state.session) return;
  if (pageCleanup) { try { pageCleanup(); } catch (_) {} pageCleanup = null; }
  const route = currentRoute();
  state.route = route;
  setCrumbs(route);
  const target = content();
  if (!target) return;
  const focus = options.keepFocus && document.activeElement && target.contains(document.activeElement)
    ? { placeholder: document.activeElement.placeholder, start: document.activeElement.selectionStart }
    : null;
  if (!options.silent) target.innerHTML = `<div class="stack"><div class="skeleton" style="height:72px"></div><div class="skeleton" style="height:300px"></div></div>`;
  const ctx = {
    reload: (opts = {}) => navigate(Object.assign({ keepFocus: true }, opts)),
    toast,
  };
  try {
    const view = await routes[route].page.render(new URLSearchParams((location.hash.split("?")[1] || "")), ctx);
    target.innerHTML = `<div class="page-head"><div><h1>${esc(view.title)}</h1>${view.subtitle ? `<p>${esc(view.subtitle)}</p>` : ""}</div>${view.actions ? `<div class="actions">${view.actions}</div>` : ""}</div>${view.html || ""}`;
    const cleanup = view.mount?.(target, ctx);
    if (typeof cleanup === "function") pageCleanup = cleanup;
    if (focus?.placeholder) {
      const input = [...target.querySelectorAll("input")].find((element) => element.placeholder === focus.placeholder);
      if (input) { input.focus(); try { input.setSelectionRange(focus.start, focus.start); } catch (_) {} }
    }
  } catch (error) {
    target.innerHTML = `<div class="error-banner">${esc(error.message)}<button class="btn small" data-retry>重试</button></div>`;
    target.querySelector("[data-retry]").onclick = () => navigate();
  }
}

window.addEventListener("hashchange", () => navigate());
window.addEventListener("rtflow:unauthorized", () => { state.session = null; renderLogin("登录已过期"); });
boot();
