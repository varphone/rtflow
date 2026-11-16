import { api, apiV1, setCsrf } from "./js/api.js";
import { icon } from "./js/icons.js";
import { esc } from "./js/format.js";
import { openCommandPalette, toast } from "./js/ui.js";
import * as overview from "./js/pages/overview.js";
import * as live from "./js/pages/live.js";
import * as vod from "./js/pages/vod.js";
import * as paths from "./js/pages/paths.js";
import * as keys from "./js/pages/keys.js";
import * as proxies from "./js/pages/proxies.js";
import * as srt from "./js/pages/srt.js";
import * as recordings from "./js/pages/recordings.js";
import * as system from "./js/pages/system.js";
import * as audit from "./js/pages/audit.js";

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
  audit: { page: audit, group: "系统", label: "审计日志" },
};
const state = { session: null, route: "overview", loading: false, counts: { live: 0, proxies: 0, srt: 0 } };
let pageCleanup = null;
let pageController = null;
let navSeq = 0;

const navGroups = [
  ["运行", [["overview", "概览"], ["live", "直播流"], ["vod", "点播"]]],
  ["配置", [["config/paths", "路径"], ["config/keys", "推流密钥"], ["config/proxies", "RTSP 代理"], ["config/srt", "SRT"], ["config/recordings", "录制"]]],
  ["系统", [["system", "系统"], ["audit", "审计日志"]]],
];
const navIcons = { overview: "overview", live: "live", vod: "vod", "config/paths": "paths", "config/keys": "keys", "config/proxies": "proxy", "config/srt": "srt", "config/recordings": "record", system: "system", audit: "audit" };

async function refreshCounts() {
  try {
    const [overview, proxies, pulls, pushes] = await Promise.all([
      api("/overview"), apiV1("/proxies"), apiV1("/srt/pulls"), apiV1("/srt/pushes"),
    ]);
    state.counts = {
      live: Number(overview.mounts || 0),
      proxies: (proxies.proxies || []).length,
      srt: (pulls.items || []).length + (pushes.items || []).length,
    };
    for (const [key, value] of Object.entries(state.counts)) {
      const node = document.querySelector(`[data-nav-count="${key}"]`);
      if (node) node.textContent = value || "";
    }
  } catch (_) { /* counts are best effort */ }
}

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
function density() { return localStorage.getItem("rtflow-density") || "comfortable"; }
function applyDensity(value) {
  document.documentElement.dataset.density = value;
  localStorage.setItem("rtflow-density", value);
}
applyDensity(density());

async function boot() {
  try {
    const session = await api("/session");
    if (!session.authenticated) return renderLogin();
    state.session = session;
    setCsrf(session.csrf);
    renderShell();
    await navigate();
    refreshCounts();
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

function navCount(route) {
  if (route === "live") return `<span class="count" data-nav-count="live">${state.counts.live || ""}</span>`;
  if (route === "config/proxies") return `<span class="count" data-nav-count="proxies">${state.counts.proxies || ""}</span>`;
  if (route === "config/srt") return `<span class="count" data-nav-count="srt">${state.counts.srt || ""}</span>`;
  return "";
}

function renderShell() {
  app.innerHTML = `<div class="layout">
    <aside class="sidebar" id="sidebar">
      <div class="brand"><span class="brand-logo">R</span><span>rtflow</span></div>
      ${navGroups.map(([label, items]) => `<nav class="nav-group" aria-label="${label}">
        <div class="nav-label">${label}</div>
        ${items.map(([route, text]) => `<a class="nav-item" data-route="${route}" href="#/${route}">${icon(navIcons[route] || "overview")}<span>${text}</span>${navCount(route)}</a>`).join("")}
      </nav>`).join("")}
      <div class="sidebar-foot"><span>rtflow ${esc(state.session?.version || "")}</span><span>${state.session?.readonly ? "只读模式" : "管理模式"}</span></div>
    </aside>
    <div class="main">
      <header class="topbar">
        <button class="btn ghost small menu-btn" data-menu aria-label="打开菜单">${icon("menu")}</button>
        <div class="crumbs" id="crumbs"></div>
        <div class="top-actions">
          <button class="command-trigger" data-command-open aria-label="打开命令面板">${icon("search")}<span>搜索</span><kbd>Ctrl K</kbd></button>
          <span class="health"><span class="dot ok"></span>运行中</span>
          <button class="user-chip" data-user-menu aria-haspopup="menu"><span class="avatar">${esc((state.session?.username || "A").slice(0, 1).toUpperCase())}</span><span>${esc(state.session?.username || "admin")}</span><span class="chev">▾</span></button>
          <div class="user-popover" data-user-popover hidden>
            <div class="user-popover-head"><b>${esc(state.session?.username || "admin")}</b><span>${state.session?.readonly ? "只读模式" : "管理员"}</span></div>
            <button data-user-theme><span>外观</span><b data-label-theme></b></button>
            <button data-user-density><span>表格密度</span><b data-label-density></b></button>
            <button data-user-time><span>时间显示</span><b data-label-time></b></button>
            <button data-user-logout class="danger"><span>退出登录</span></button>
          </div>
        </div>
      </header>
      <main class="content" id="content"><div id="page-content"></div></main>
    </div>
  </div>`;
  app.querySelectorAll("[data-route]").forEach((link) => link.addEventListener("click", () => {
    document.getElementById("sidebar").classList.remove("open");
  }));
  app.querySelector("[data-menu]").onclick = () => document.getElementById("sidebar").classList.toggle("open");
  app.querySelector("[data-command-open]").onclick = () => openPalette();
  const popover = app.querySelector("[data-user-popover]");
  const userButton = app.querySelector("[data-user-menu]");
  const syncMenu = () => {
    const themeLabel = app.querySelector("[data-label-theme]");
    const densityLabel = app.querySelector("[data-label-density]");
    const timeLabel = app.querySelector("[data-label-time]");
    if (themeLabel) themeLabel.textContent = document.documentElement.dataset.theme === "light" ? "浅色" : "深色";
    if (densityLabel) densityLabel.textContent = document.documentElement.dataset.density === "compact" ? "紧凑" : "舒适";
    if (timeLabel) timeLabel.textContent = localStorage.getItem("rtflow-time") === "relative" ? "相对" : "绝对";
  };
  const closeMenu = () => { popover.hidden = true; userButton.setAttribute("aria-expanded", "false"); };
  userButton.onclick = (event) => { event.stopPropagation(); popover.hidden = !popover.hidden; userButton.setAttribute("aria-expanded", String(!popover.hidden)); syncMenu(); };
  popover.onclick = (event) => event.stopPropagation();
  document.addEventListener("click", () => closeMenu());
  document.addEventListener("keydown", (event) => { if (event.key === "Escape") closeMenu(); });
  app.querySelector("[data-user-theme]").onclick = () => { applyTheme(document.documentElement.dataset.theme === "light" ? "dark" : "light"); syncMenu(); };
  app.querySelector("[data-user-density]").onclick = () => { applyDensity(document.documentElement.dataset.density === "compact" ? "comfortable" : "compact"); syncMenu(); };
  app.querySelector("[data-user-time]").onclick = () => { localStorage.setItem("rtflow-time", localStorage.getItem("rtflow-time") === "relative" ? "absolute" : "relative"); closeMenu(); navigate({ silent: true }); };
  app.querySelector("[data-user-logout]").onclick = async () => {
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
  pageController?.abort();
  pageController = new AbortController();
  const sequence = ++navSeq;
  const route = currentRoute();
  const routeChanged = state.route !== route;
  state.route = route;
  setCrumbs(route);
  const previous = content();
  if (!previous) return;
  const focus = options.keepFocus && document.activeElement && previous.contains(document.activeElement)
    ? { placeholder: document.activeElement.placeholder, start: document.activeElement.selectionStart }
    : null;
  if (!options.silent && (routeChanged || !previous.querySelector("[data-page]"))) {
    previous.innerHTML = `<div class="stack"><div class="skeleton" style="height:72px"></div><div class="skeleton" style="height:300px"></div></div>`;
  }
  const scrollTop = previous.scrollTop;
  const ctx = {
    reload: (opts = {}) => navigate(Object.assign({ keepFocus: true, silent: true }, opts)),
    signal: pageController.signal,
    toast,
  };
  try {
    const view = await routes[route].page.render(new URLSearchParams((location.hash.split("?")[1] || "")), ctx);
    if (sequence !== navSeq) return;
    const target = document.createElement("div");
    target.id = "page-content";
    target.innerHTML = `<div data-page><div class="page-head"><div><h1>${esc(view.title)}</h1>${view.subtitle ? `<p>${esc(view.subtitle)}</p>` : ""}</div>${view.actions ? `<div class="actions">${view.actions}</div>` : ""}</div>${view.html || ""}</div>`;
    if (previous.isConnected) previous.replaceWith(target);
    else document.getElementById("content")?.appendChild(target);
    target.scrollTop = scrollTop;
    const cleanup = view.mount?.(target, ctx);
    if (typeof cleanup === "function") pageCleanup = cleanup;
    if (focus?.placeholder) {
      const input = [...target.querySelectorAll("input")].find((element) => element.placeholder === focus.placeholder);
      if (input) { input.focus(); try { input.setSelectionRange(focus.start, focus.start); } catch (_) {} }
    }
  } catch (error) {
    previous.innerHTML = `<div class="error-banner">${esc(error.message)}<button class="btn small" data-retry>重试</button></div>`;
    previous.querySelector("[data-retry]").onclick = () => navigate();
  }
}

function openPalette() {
  const commands = [
    ...Object.entries(routes).map(([route, info]) => ({
      label: info.label, hint: `${info.group} / ${info.label}`, icon: "→",
      keywords: route,
      run: () => { location.hash = `#/${route}`; },
    })),
    { label: "刷新当前页面", hint: "重新加载当前数据", icon: "↻", shortcut: "R", run: () => navigate({ silent: true }) },
    { label: "切换深浅主题", hint: "在深色与浅色之间切换", icon: "◐", run: () => applyTheme(document.documentElement.dataset.theme === "light" ? "dark" : "light") },
    { label: "切换表格密度", hint: "舒适 / 紧凑", icon: "≡", run: () => applyDensity(document.documentElement.dataset.density === "compact" ? "comfortable" : "compact") },
    { label: "切换时间显示", hint: localStorage.getItem("rtflow-time") === "relative" ? "当前：相对时间" : "当前：绝对时间", icon: "◷", run: () => {
      localStorage.setItem("rtflow-time", localStorage.getItem("rtflow-time") === "relative" ? "absolute" : "relative");
      navigate({ silent: true });
    } },
    { label: "复制当前页面地址", hint: "复制带 hash 的链接", icon: "⧉", run: async () => {
      try { await navigator.clipboard.writeText(location.href); toast("链接已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
    } },
    { label: "退出登录", hint: "结束当前管理员会话", icon: "⏻", run: async () => {
      try { await api("/logout", { method: "POST" }); } catch (_) {}
      state.session = null; renderLogin();
    } },
  ];
  openCommandPalette(commands);
}

window.addEventListener("keydown", (event) => {
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k") {
    event.preventDefault();
    if (state.session) openPalette();
  }
});
window.addEventListener("hashchange", () => navigate());
setInterval(() => { if (state.session) refreshCounts(); }, 10000);
window.addEventListener("rtflow:unauthorized", () => { state.session = null; renderLogin("登录已过期"); });
boot();
