import { api, apiV1, setCsrf } from "./js/api.js";
import { icon } from "./js/icons.js";
import { esc } from "./js/format.js";
import { closeDialog, confirmDialog, openCommandPalette, openDialog, toast } from "./js/ui.js";
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
function sidebarCollapsed() { return localStorage.getItem("rtflow-sidebar") === "collapsed"; }
function applySidebar(collapsed) {
  document.documentElement.dataset.sidebar = collapsed ? "collapsed" : "open";
  localStorage.setItem("rtflow-sidebar", collapsed ? "collapsed" : "open");
  const button = document.querySelector("[data-sidebar-toggle]");
  if (button) {
    button.innerHTML = `<span>${collapsed ? "›" : "‹"}</span><em>${collapsed ? "" : "收起"}</em>`;
    button.setAttribute("aria-label", collapsed ? "展开侧栏" : "收起侧栏");
    button.title = collapsed ? "展开侧栏" : "收起侧栏";
  }
}
applySidebar(sidebarCollapsed());

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
  document.title = `登录 · ${state.session?.title || "rtflow"}`;
  app.innerHTML = `<main class="login-wrap"><div class="login-shell">
    <section class="login-aside">
      <div class="brand"><span class="brand-logo">R</span><span>${esc(state.session?.title || "rtflow")}</span></div>
      <span class="login-kicker">CONTROL PLANE</span>
      <h2>流媒体控制台</h2>
      <p>从一个界面管理发布、播放、录制与回源，掌握每条流的运行状态。</p>
      <ul class="login-points">
        <li><b>多协议</b><span>RTSP · RTMP · SRT · HLS · WebRTC</span></li>
        <li><b>运行可观测</b><span>实时吞吐、异常与审计记录</span></li>
        <li><b>安全可控</b><span>推流密钥、路径策略与只读模式</span></li>
      </ul>
      <div class="login-aside-foot">SELF-HOSTED · ${esc(location.host)}</div>
    </section>
    <section class="login-card">
      <div class="brand login-mobile-brand"><span class="brand-logo">R</span><span>${esc(state.session?.title || "rtflow")}</span></div>
      <h1>登录</h1><p class="sub">使用管理员账号进入控制台。</p>
      <form id="login-form" class="stack">
        ${message ? `<div class="form-error">${esc(message)}</div>` : ""}
        <div class="field"><label for="login-user">用户名</label><input class="input" id="login-user" name="username" autocomplete="username" required autofocus></div>
        <div class="field"><label for="login-pass">密码</label><div class="password-field"><input class="input" id="login-pass" name="password" type="password" autocomplete="current-password" required><button type="button" class="password-toggle" data-toggle-password aria-label="显示密码">${icon("eye")}</button></div></div>
        <button class="btn primary block" type="submit">登录</button>
      </form>
      <div class="login-foot">rtflow admin</div>
    </section>
  </div></main>`;
  const form = document.getElementById("login-form");
  const password = form.querySelector("#login-pass");
  const toggle = form.querySelector("[data-toggle-password]");
  toggle.onclick = () => {
    const show = password.type === "password";
    password.type = show ? "text" : "password";
    toggle.innerHTML = icon(show ? "eyeOff" : "eye");
    toggle.setAttribute("aria-label", show ? "隐藏密码" : "显示密码");
    password.focus();
  };
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const button = form.querySelector("button");
    button.disabled = true;
    button.classList.add("loading");
    button.textContent = "登录中…";
    button.setAttribute("aria-busy", "true");
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
      <a class="brand" href="#/overview" aria-label="返回概览"><span class="brand-logo">R</span><span>rtflow</span></a>
      ${navGroups.map(([label, items]) => `<nav class="nav-group" aria-label="${label}">
        <div class="nav-label">${label}</div>
        ${items.map(([route, text]) => `<a class="nav-item" data-route="${route}" href="#/${route}" title="${esc(text)}">${icon(navIcons[route] || "overview")}<span>${text}</span>${navCount(route)}</a>`).join("")}
      </nav>`).join("")}
      <button class="sidebar-collapse" data-sidebar-toggle aria-label="收起侧栏" title="收起侧栏"></button>
      <div class="sidebar-foot"><span>rtflow ${esc(state.session?.version || "")}</span><span>${state.session?.readonly ? "只读模式" : "管理模式"}</span></div>
    </aside>
    <button class="sidebar-backdrop" data-sidebar-backdrop aria-label="关闭菜单"></button>
    <div class="main">
      <header class="topbar">
        <button class="btn ghost small menu-btn" data-menu aria-label="打开菜单" aria-controls="sidebar" aria-expanded="false">${icon("menu")}</button>
        <div class="crumbs" id="crumbs"></div>
        <div class="top-actions">
          <button class="command-trigger" data-command-open aria-label="打开命令面板">${icon("search")}<span>搜索</span><kbd>Ctrl K</kbd></button>
          <a class="health" href="#/system" title="查看系统状态"><span class="dot ok"></span>运行中</a>
          <button class="user-chip" data-user-menu aria-haspopup="menu" aria-expanded="false"><span class="avatar">${esc((state.session?.username || "A").slice(0, 1).toUpperCase())}</span><span>${esc(state.session?.username || "admin")}</span><span class="chev">▾</span></button>
          <div class="user-popover" data-user-popover role="menu" aria-label="用户菜单" hidden>
            <div class="user-popover-head"><b>${esc(state.session?.username || "admin")}</b><span>${state.session?.readonly ? "只读模式" : "管理员"}</span></div>
            <button role="menuitem" data-user-theme><span>外观</span><b data-label-theme></b></button>
            <button role="menuitem" data-user-density><span>表格密度</span><b data-label-density></b></button>
            <button role="menuitem" data-user-time><span>时间显示</span><b data-label-time></b></button>
            <button data-user-logout class="danger"><span>退出登录</span></button>
          </div>
        </div>
      </header>
      <main class="content" id="content"><div id="page-content"></div></main>
      <div class="sr-only" aria-live="polite" data-page-announcer></div>
    </div>
  </div>`;
  app.querySelectorAll("[data-route]").forEach((link) => link.addEventListener("click", () => {
    document.getElementById("sidebar").classList.remove("open");
    document.body.classList.remove("nav-open");
  }));
  const sidebar = document.getElementById("sidebar");
  const setSidebar = (open) => {
    sidebar.classList.toggle("open", open);
    app.querySelector("[data-menu]").setAttribute("aria-expanded", String(open));
    document.body.classList.toggle("nav-open", open);
  };
  app.querySelector("[data-menu]").onclick = () => setSidebar(!sidebar.classList.contains("open"));
  app.querySelector("[data-sidebar-backdrop]").onclick = () => setSidebar(false);
  document.addEventListener("keydown", (event) => { if (event.key === "Escape") setSidebar(false); });
  app.querySelector("[data-command-open]").onclick = () => openPalette();
  app.querySelector("[data-sidebar-toggle]").onclick = () => applySidebar(document.documentElement.dataset.sidebar !== "collapsed");
  const sidebarButton = app.querySelector("[data-sidebar-toggle]");
  sidebarButton.innerHTML = `<span>${sidebarCollapsed() ? "›" : "‹"}</span><em>${sidebarCollapsed() ? "" : "收起"}</em>`;
  sidebarButton.setAttribute("aria-label", sidebarCollapsed() ? "展开侧栏" : "收起侧栏");
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
  const closeMenu = (restoreFocus = false) => {
    const wasOpen = !popover.hidden;
    popover.hidden = true;
    userButton.setAttribute("aria-expanded", "false");
    if (wasOpen && restoreFocus) userButton.focus();
  };
  userButton.onclick = (event) => {
    event.stopPropagation();
    popover.hidden = !popover.hidden;
    userButton.setAttribute("aria-expanded", String(!popover.hidden));
    syncMenu();
    if (!popover.hidden) requestAnimationFrame(() => popover.querySelector('[role="menuitem"]')?.focus());
  };
  popover.onclick = (event) => event.stopPropagation();
  document.addEventListener("click", () => closeMenu());
  document.addEventListener("keydown", (event) => { if (event.key === "Escape") closeMenu(true); });
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
  document.title = `${info.label} · ${state.session?.title || "rtflow"}`;
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
  const contentNode = document.getElementById("content");
  contentNode?.setAttribute("aria-busy", "true");
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
    contentNode?.setAttribute("aria-busy", "false");
    if (routeChanged) {
      const announcer = document.querySelector("[data-page-announcer]");
      if (announcer) announcer.textContent = `${routes[route].label} 页面`;
    }
    const cleanup = view.mount?.(target, ctx);
    if (typeof cleanup === "function") pageCleanup = cleanup;
    if (focus?.placeholder) {
      const input = [...target.querySelectorAll("input")].find((element) => element.placeholder === focus.placeholder);
      if (input) { input.focus(); try { input.setSelectionRange(focus.start, focus.start); } catch (_) {} }
    }
  } catch (error) {
    contentNode?.setAttribute("aria-busy", "false");
    previous.innerHTML = `<div class="error-banner">${esc(error.message)}<button class="btn small" data-retry>重试</button></div>`;
    previous.querySelector("[data-retry]").onclick = () => navigate();
  }
}

function isTyping(event) {
  const target = event.target;
  return target instanceof HTMLElement
    && (target.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName));
}

function exportCurrentView() {
  const button = document.querySelector(
    "[data-action=export],[data-live-export],[data-vod-export],[data-rec-export],[data-proxy-export],[data-srt-export],[data-audit-export]"
  );
  if (button) { button.click(); return true; }
  toast("当前页面不支持导出", "bad");
  return false;
}

function focusPageSearch() {
  const input = document.querySelector(
    "[data-live-search],[data-path-search],[data-key-search],[data-proxy-search],[data-srt-search],[data-vod-search],[data-rec-search],[data-audit-search]"
  );
  if (input) { input.focus(); input.select?.(); }
}

function openShortcuts() {
  const row = (keys, description) => `<div class="shortcut-row"><span>${keys.map((key) => `<kbd>${esc(key)}</kbd>`).join(" ")}</span><span>${esc(description)}</span></div>`;
  openDialog({
    title: "键盘快捷键",
    body: `<div class="shortcut-list">
      ${row(["Ctrl", "K"], "打开命令面板")}
      ${row(["/"], "聚焦当前页面搜索")}
      ${row(["↑", "↓"], "命令面板中选择")}
      ${row(["Enter"], "执行命令")}
      ${row(["Ctrl", "E"], "导出当前视图")}
      ${row(["R"], "刷新当前页面")}
      ${row(["Esc"], "关闭对话框 / 抽屉 / 侧栏")}
      ${row(["?"], "打开本帮助")}
    </div>`,
    footer: `<button class="btn" data-close-dialog>关闭</button>`,
  });
}

function savedViews() {
  try { return JSON.parse(localStorage.getItem("rtflow-views") || "[]"); } catch (_) { return []; }
}

function manageViews() {
  const views = savedViews();
  openDialog({
    title: "保存的视图",
    body: views.length ? `<div class="view-list">${views.map((view, index) => `<div class="view-row">
      <div><strong>${esc(view.name)}</strong><div class="cell-sub mono">${esc(view.hash)}</div></div>
      <div class="row-actions"><button class="btn small ghost" data-rename-view="${index}">重命名</button><button class="btn small danger" data-delete-view="${index}">删除</button></div>
    </div>`).join("")}</div>` : `<div class="note">还没有保存的视图。在命令面板中选择“保存当前视图”。</div>`,
    footer: `${views.length ? `<button class="btn danger ghost" data-clear-views>清空全部</button>` : ""}<button class="btn" data-close-dialog>关闭</button>`,
    onMount(root) {
      root.querySelectorAll("[data-rename-view]").forEach((button) => button.onclick = () => {
        const list = savedViews();
        const index = Number(button.dataset.renameView);
        const current = list[index];
        if (!current) return;
        openDialog({
          title: "重命名视图",
          body: `<div class="field"><label for="view-rename">视图名称</label><input class="input" id="view-rename" maxlength="40" value="${esc(current.name)}" autofocus></div>`,
          footer: `<button class="btn" data-close-dialog>取消</button><button class="btn primary" data-rename-save>保存</button>`,
          onMount(dialog) {
            const input = dialog.querySelector("#view-rename");
            const save = () => {
              const name = input.value.trim();
              if (!name) { toast("请输入视图名称", "bad"); return; }
              if (savedViews().some((view, i) => i !== index && view.name === name)) { toast("视图名称已存在", "bad"); return; }
              const next = savedViews();
              next[index] = { ...next[index], name };
              localStorage.setItem("rtflow-views", JSON.stringify(next));
              closeDialog();
              toast("视图已重命名", "ok");
              manageViews();
            };
            dialog.querySelector("[data-rename-save]").onclick = save;
            input.addEventListener("keydown", (event) => { if (event.key === "Enter") { event.preventDefault(); save(); } });
            requestAnimationFrame(() => { input.focus(); input.select(); });
          },
        });
      });
      root.querySelectorAll("[data-delete-view]").forEach((button) => button.onclick = () => {
        const next = savedViews();
        next.splice(Number(button.dataset.deleteView), 1);
        localStorage.setItem("rtflow-views", JSON.stringify(next));
        toast("视图已删除", "ok");
        manageViews();
      });
      root.querySelector("[data-clear-views]")?.addEventListener("click", async () => {
        if (!await confirmDialog({ title: "清空视图", message: "删除全部保存的视图？", confirmLabel: "清空", danger: true })) return;
        localStorage.removeItem("rtflow-views");
        toast("视图已清空", "ok");
        manageViews();
      });
    },
  });
}

function saveCurrentView() {
  const route = currentRoute();
  const hash = location.hash || `#/${route}`;
  const suggested = `${routes[route]?.label || route} 视图`;
  openDialog({
    title: "保存当前视图",
    body: `<div class="field"><label for="view-name">视图名称</label><input class="input" id="view-name" maxlength="40" value="${esc(suggested)}" autofocus></div>
      <div class="note">保存当前页面与筛选条件：<span class="mono">${esc(hash)}</span></div>`,
    footer: `<button class="btn" data-close-dialog>取消</button><button class="btn primary" data-view-save>保存</button>`,
    onMount(root) {
      const input = root.querySelector("#view-name");
      const save = () => {
        const name = input.value.trim();
        if (!name) { toast("请输入视图名称", "bad"); input.focus(); return; }
        const views = savedViews().filter((view) => view.name !== name);
        views.unshift({ name, hash, route, ts: Date.now() });
        localStorage.setItem("rtflow-views", JSON.stringify(views.slice(0, 12)));
        closeDialog();
        toast(`视图“${name}”已保存`, "ok");
      };
      root.querySelector("[data-view-save]").onclick = save;
      input.addEventListener("keydown", (event) => { if (event.key === "Enter") { event.preventDefault(); save(); } });
      requestAnimationFrame(() => { input.focus(); input.select(); });
    },
  });
}

function openPalette() {
  const views = savedViews().map((view) => ({
    label: `视图 · ${view.name}`, hint: view.hash, icon: "★", group: "视图", keywords: "saved view 收藏",
    run: () => { location.hash = view.hash; },
  }));
  const pages = Object.entries(routes).map(([route, info]) => ({
    label: info.label, hint: `${info.group} / ${info.label}`, icon: "→", group: "页面",
    keywords: route,
    run: () => { location.hash = `#/${route}`; },
  }));
  const actions = [
    { label: "刷新当前页面", hint: "重新加载当前数据", icon: "↻", shortcut: "R", group: "操作", run: () => navigate({ silent: true }) },
    { label: "保存当前视图", hint: "保存页面与筛选条件到本地", icon: "★", group: "操作", run: () => saveCurrentView() },
    { label: "管理保存的视图", hint: "查看或删除本地视图", icon: "☆", group: "操作", run: () => manageViews() },
    { label: "导出当前视图", hint: "CSV（当前页面支持时）", icon: "⇩", group: "操作", run: () => exportCurrentView() },
    { label: "切换深浅主题", hint: "在深色与浅色之间切换", icon: "◐", group: "操作", run: () => applyTheme(document.documentElement.dataset.theme === "light" ? "dark" : "light") },
    { label: "切换表格密度", hint: "舒适 / 紧凑", icon: "≡", group: "操作", run: () => applyDensity(document.documentElement.dataset.density === "compact" ? "comfortable" : "compact") },
    { label: "切换时间显示", hint: localStorage.getItem("rtflow-time") === "relative" ? "当前：相对时间" : "当前：绝对时间", icon: "◷", group: "操作", run: () => {
      localStorage.setItem("rtflow-time", localStorage.getItem("rtflow-time") === "relative" ? "absolute" : "relative");
      navigate({ silent: true });
    } },
    { label: "键盘快捷键", hint: "查看可用快捷键", icon: "⌘", shortcut: "?", group: "操作", run: () => openShortcuts() },
    { label: "复制当前页面地址", hint: "复制带 hash 的链接", icon: "⧉", group: "操作", run: async () => {
      try { await navigator.clipboard.writeText(location.href); toast("链接已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
    } },
    { label: "退出登录", hint: "结束当前管理员会话", icon: "⏻", group: "操作", run: async () => {
      try { await api("/logout", { method: "POST" }); } catch (_) {}
      state.session = null; renderLogin();
    } },
  ];
  const palette = openCommandPalette([...pages, ...views, ...actions]);
  (async () => {
    let streams = [], vods = [], paths = [];
    try {
      [streams, vods, paths] = await Promise.all([
        api("/streams").then((payload) => payload.items || payload || []),
        api("/vods").then((payload) => payload.items || payload || []),
        apiV1("/paths/config").then((payload) => Array.isArray(payload) ? payload : payload.items || []),
      ]);
    } catch (_) { return; }
    const resources = [
      ...streams.slice(0, 8).map((stream) => ({
        label: `流 · ${stream.name}`, hint: `${stream.path} · ${stream.video?.codec || "媒体"}`, icon: "▷", group: "直播流", keywords: "live stream",
        run: () => { location.hash = `#/live?q=${encodeURIComponent(stream.path)}`; },
      })),
      ...paths.slice(0, 6).map((path) => ({
        label: `路径 · /${path.name}`, hint: "配置 / 路径", icon: "⌁", group: "路径", keywords: "path config",
        run: () => { location.hash = `#/config/paths?q=${encodeURIComponent(path.name)}`; },
      })),
      ...vods.slice(0, 6).map((vod) => ({
        label: `点播 · ${vod.title}`, hint: "运行 / 点播", icon: "▤", group: "点播", keywords: "vod recording",
        run: () => { location.hash = `#/vod?q=${encodeURIComponent(vod.title)}`; },
      })),
    ];
    palette.setCommands([...pages, ...views, ...resources, ...actions]);
  })();
}
window.addEventListener("keydown", (event) => {
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k") {
    event.preventDefault();
    if (state.session) openPalette();
    return;
  }
  if (!state.session) return;
  if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "e") {
    event.preventDefault();
    exportCurrentView();
    return;
  }
  if (isTyping(event) || event.ctrlKey || event.metaKey || event.altKey) return;
  if (event.key === "?") { event.preventDefault(); openShortcuts(); }
  else if (event.key === "/") { event.preventDefault(); focusPageSearch(); }
  else if (event.key.toLowerCase() === "r") { event.preventDefault(); navigate({ silent: true }); }
});
window.addEventListener("hashchange", () => navigate());
setInterval(() => { if (state.session && !document.hidden) refreshCounts(); }, 10000);
window.addEventListener("offline", () => { if (state.session) toast("网络连接已断开", "bad"); });
window.addEventListener("online", () => { if (state.session) toast("网络连接已恢复", "ok"); });
document.addEventListener("visibilitychange", () => { if (!document.hidden && state.session) refreshCounts(); });
window.addEventListener("rtflow:unauthorized", () => { state.session = null; renderLogin("登录已过期"); });
boot();
