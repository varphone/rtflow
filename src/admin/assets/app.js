"use strict";
(() => {
  const BASE = window.__ADMIN_BASE__ || "/admin";
  const API = BASE + "/api";
  const app = document.getElementById("app");
  const modalRoot = document.getElementById("modal-root");
  const toastRoot = document.getElementById("toast-root");

  const state = {
    session: null,
    page: "dashboard",
    streams: [],
    vods: [],
    overview: {},
    system: {},
    query: "",
    status: "all",
    error: "",
    loading: true,
    modal: null,
    timer: null,
    hls: null,
    hlsLoading: null,
  };

  const NAV = [
    ["dashboard", "总览", "grid"],
    ["live", "直播管理", "broadcast"],
    ["vod", "点播管理", "film"],
    ["system", "系统信息", "server"],
  ];

  const ICONS = {
    grid: '<rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/>',
    broadcast: '<circle cx="12" cy="12" r="2.2"/><path d="M7.8 7.8a6 6 0 0 0 0 8.4M16.2 7.8a6 6 0 0 1 0 8.4M4.6 4.6a10.5 10.5 0 0 0 0 14.8M19.4 4.6a10.5 10.5 0 0 1 0 14.8"/>',
    film: '<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M7 4v16M17 4v16M3 9h4M17 9h4M3 15h4M17 15h4"/>',
    server: '<rect x="3" y="4" width="18" height="6" rx="1.5"/><rect x="3" y="14" width="18" height="6" rx="1.5"/><path d="M7 7h.01M7 17h.01"/>',
    refresh: '<path d="M20 11a8 8 0 1 0-2.3 5.7"/><path d="M20 4v7h-7"/>',
    external: '<path d="M14 4h6v6M20 4l-9 9"/><path d="M20 14v4a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h4"/>',
    play: '<path d="M8 5v14l11-7z"/>',
    power: '<path d="M12 3v9"/><path d="M6.4 6.4a8 8 0 1 0 11.2 0"/>',
    copy: '<rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V5a2 2 0 0 1 2-2h8"/>',
    close: '<path d="M6 6l12 12M18 6L6 18"/>',
    search: '<circle cx="11" cy="11" r="7"/><path d="M20 20l-3.5-3.5"/>',
    menu: '<path d="M4 6h16M4 12h16M4 18h16"/>',
    filmSmall: '<rect x="3" y="4" width="18" height="16" rx="2"/><path d="M7 4v16M17 4v16"/>',
  };

  const icon = (name, cls = "") =>
    `<span class="icon ${cls}"><svg viewBox="0 0 24 24" aria-hidden="true">${ICONS[name] || ""}</svg></span>`;
  const esc = (value) =>
    String(value == null ? "" : value)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#39;");
  const num = (value) => {
    const n = Number(value || 0);
    return n.toLocaleString("zh-CN");
  };
  const bytes = (value) => {
    const n = Number(value || 0);
    const units = ["B", "KB", "MB", "GB", "TB"];
    let i = 0;
    while (n >= 1024 && i < units.length - 1) { i++; }
    return (n / Math.pow(1024, i)).toFixed(i === 0 ? 0 : 1) + " " + units[i];
  };
  const uptime = (seconds) => {
    const s = Math.max(0, Number(seconds || 0));
    const d = Math.floor(s / 86400), h = Math.floor((s % 86400) / 3600), m = Math.floor((s % 3600) / 60);
    if (d) return `${d} 天 ${h} 小时`;
    if (h) return `${h} 小时 ${m} 分`;
    return `${m} 分钟`;
  };
  const resolution = (track) =>
    track && track.width && track.height ? `${track.width}×${track.height}` : "—";
  const fps = (track) => (track && track.frame_rate ? `${track.frame_rate} fps` : "");
  const audioLabel = (track) => {
    if (!track) return "—";
    return `${track.codec}${track.channels ? " · " + track.channels + "ch" : ""}${track.sample_rate ? " · " + Math.round(track.sample_rate / 1000) + "kHz" : ""}`;
  };

  function toast(message, kind = "") {
    const el = document.createElement("div");
    el.className = "toast " + kind;
    el.textContent = message;
    toastRoot.appendChild(el);
    setTimeout(() => el.remove(), 3600);
  }

  async function api(path, options = {}) {
    const headers = Object.assign({ Accept: "application/json" }, options.headers || {});
    if (options.body && !headers["Content-Type"]) headers["Content-Type"] = "application/json";
    if (options.method && options.method !== "GET" && state.session && state.session.csrf) {
      headers["X-CSRF-Token"] = state.session.csrf;
    }
    const response = await fetch(API + path, Object.assign({ credentials: "same-origin" }, options, { headers }));
    const text = await response.text();
    let body = {};
    try { body = text ? JSON.parse(text) : {}; } catch (_) { body = { message: text }; }
    if (response.status === 401 && path !== "/session" && path !== "/login") {
      state.session = { authenticated: false };
      renderLogin();
    }
    if (!response.ok) {
      const error = new Error(body.message || body.error || `请求失败 (${response.status})`);
      error.status = response.status;
      throw error;
    }
    return body;
  }

  async function loadAll() {
    const [streams, overview, vods, system] = await Promise.all([
      api("/streams"),
      api("/overview"),
      api("/vods"),
      api("/system"),
    ]);
    state.streams = streams.items || [];
    state.overview = overview || {};
    state.vods = vods.items || [];
    state.system = system || {};
    state.error = "";
  }

  function startPolling() {
    stopPolling();
    state.timer = setInterval(async () => {
      if (!state.session || !state.session.authenticated) return;
      if (state.modal) return;
      if (document.activeElement && document.activeElement.matches("[data-search]")) return;
      try { await loadAll(); render(); } catch (error) { /* keep last good snapshot */ }
    }, 5000);
  }
  function stopPolling() { if (state.timer) clearInterval(state.timer); state.timer = null; }

  function renderLogin() {
    stopPolling();
    closeModal();
    const title = (state.session && state.session.title) || "rtflow 管理控制台";
    document.title = title;
    app.innerHTML = `
      <div class="login-wrap">
        <section class="login-brand">
          <div>
            <div class="brand-mark"><span class="brand-logo">R</span><span>rtflow</span></div>
            <h1>流媒体控制台<br>为生产环境而设计</h1>
            <p>实时监控 RTMP / RTSP / HLS 流，管理点播资源与运行状态。所有操作集中在一个安全、可审计的界面。</p>
            <div class="brand-points">
              <span>${icon("broadcast")} <b>实时直播</b> 发布状态、订阅数与码流统计</span>
              <span>${icon("film")} <b>点播资源</b> HLS 播放与缓存刷新</span>
              <span>${icon("server")} <b>运行信息</b> 版本、特性与容量一屏掌握</span>
            </div>
          </div>
          <div class="muted" style="font-size:12px">rtflow · ${esc(state.system.version || "0.3")}</div>
        </section>
        <section class="login-panel">
          <form class="login-card" id="login-form" autocomplete="off">
            <h2>登录控制台</h2>
            <p class="sub">使用管理员账号访问</p>
            <div id="login-error"></div>
            <div class="field">
              <label for="login-user">用户名</label>
              <input class="input" id="login-user" name="username" autocomplete="username" required autofocus>
            </div>
            <div class="field">
              <label for="login-pass">密码</label>
              <input class="input" id="login-pass" name="password" type="password" autocomplete="current-password" required>
            </div>
            <button class="btn primary block" id="login-submit" type="submit">${icon("power")} 登录</button>
            <p class="note" style="margin:16px 0 0">请通过 HTTPS 或受信任的内网访问；登录状态 8 小时内有效。</p>
          </form>
        </section>
      </div>`;
    const form = document.getElementById("login-form");
    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      const button = document.getElementById("login-submit");
      const errorBox = document.getElementById("login-error");
      button.disabled = true;
      errorBox.innerHTML = "";
      try {
        await api("/login", {
          method: "POST",
          body: JSON.stringify({
            username: document.getElementById("login-user").value,
            password: document.getElementById("login-pass").value,
          }),
        });
        state.session = await api("/session");
        state.loading = true;
        render();
        await loadAll();
        state.loading = false;
        render();
        startPolling();
      } catch (error) {
        errorBox.innerHTML = `<div class="form-error">${esc(error.message)}</div>`;
        button.disabled = false;
      }
    });
  }

  function shell(pageHtml, pageTitle, subtitle, actions) {
    const s = state.session || {};
    return `
      <div class="layout">
        <aside class="sidebar" id="sidebar">
          <div class="brand"><div class="brand-mark"><span class="brand-logo">R</span><span>rtflow</span></div></div>
          <nav class="nav">
            ${NAV.map(([id, label, ico]) => `
              <div class="nav-item ${state.page === id ? "active" : ""}" data-nav="${id}">
                ${icon(ico)} <span>${label}</span>
                ${id === "live" ? `<span class="badge-count">${state.streams.length}</span>` : ""}
                ${id === "vod" ? `<span class="badge-count">${state.vods.length}</span>` : ""}
              </div>`).join("")}
          </nav>
          <div class="sidebar-foot">
            <span>rtflow ${esc(state.system.version || "")}</span>
            <span>${s.readonly ? "只读模式" : "管理模式"}</span>
          </div>
        </aside>
        <div class="main">
          <header class="topbar">
            <button class="btn ghost small menu-btn" data-action="menu" aria-label="菜单">${icon("menu")}</button>
            <h1>${esc(pageTitle)}</h1>
            <span class="meta">运行 ${esc(uptime(state.overview.uptime_seconds))}</span>
            <div class="user-chip"><span class="avatar">${esc((s.username || "A").slice(0, 1).toUpperCase())}</span><span>${esc(s.username || "admin")}</span></div>
            <button class="btn ghost small" data-action="logout" title="退出登录">退出</button>
          </header>
          <main class="content">
            ${subtitle ? `<div class="page-head"><div><h2>${esc(pageTitle)}</h2><p>${esc(subtitle)}</p></div><div class="actions">${actions || ""}</div></div>` : ""}
            <div id="page-content">${pageHtml}</div>
          </main>
        </div>
      </div>`;
  }

  function render() {
    if (!state.session || !state.session.authenticated) { renderLogin(); return; }
    document.title = (state.session.title || "rtflow") + " · " + (NAV.find((n) => n[0] === state.page) || ["", ""])[1];
    let html = "";
    if (state.loading) {
      html = `<div class="stack"><div class="skeleton" style="height:96px"></div><div class="skeleton" style="height:340px"></div></div>`;
      app.innerHTML = shell(html, "加载中", "", "");
      return;
    }
    if (state.error) {
      html += `<div class="error-banner">${icon("server")}<span>${esc(state.error)}</span><button class="btn small" data-action="retry">重试</button></div>`;
    }
    if (state.page === "dashboard") html += dashboardPage();
    else if (state.page === "live") html += livePage();
    else if (state.page === "vod") html += vodPage();
    else html += systemPage();
    const meta = {
      dashboard: ["总览", "流媒体运行状态与关键指标", ""],
      live: ["直播管理", "实时发布状态、播放与流控制", `<button class="btn small" data-action="refresh">${icon("refresh")} 刷新</button>`],
      vod: ["点播管理", "HLS 点播资源与缓存", `<button class="btn small" data-action="refresh">${icon("refresh")} 刷新</button>`],
      system: ["系统信息", "版本、构建特性与运行配置", ""],
    }[state.page];
    app.innerHTML = shell(html, meta[0], meta[1], meta[2]);
  }

  function statCard(label, value, hint, cls = "") {
    return `<div class="stat ${cls}"><div class="label">${label}</div><div class="value">${value}</div><div class="hint">${hint}</div></div>`;
  }

  function dashboardPage() {
    const o = state.overview || {};
    const live = state.streams.filter((s) => s.publisher);
    const recent = state.streams.slice(0, 6);
    return `
      <div class="stat-grid">
        ${statCard(`${icon("broadcast")} 在线流`, num(o.mounts), `${num(o.publishers)} 个发布中`, o.publishers ? "ok" : "")}
        ${statCard(`${icon("grid")} 观看连接`, num(o.subscribers), "当前订阅总数")}
        ${statCard(`${icon("film")} 点播资源`, num(o.vods), "HLS VOD 条目")}
        ${statCard(`${icon("server")} 运行时长`, esc(uptime(o.uptime_seconds)), "自进程启动")}
      </div>
      <div class="two-col">
        <div class="panel">
          <div class="panel-head"><h3>实时流</h3><span class="sub">最近 ${recent.length} 条</span><div class="actions"><button class="btn small ghost" data-nav="live">查看全部 ${icon("external")}</button></div></div>
          ${recent.length ? streamsTable(recent, true) : empty("还没有直播流", "通过 RTMP 或 RTSP 推流后会自动出现在这里")}
        </div>
        <div class="stack">
          <div class="panel">
            <div class="panel-head"><h3>发布状态</h3><span class="sub">${live.length} 在线</span></div>
            <div class="panel-body">
              ${live.length ? `<div class="quick-list">${live.slice(0, 5).map((s) => `
                <div class="quick-item"><span class="dot live"></span><div class="grow"><div class="name">${esc(s.name)}</div><div class="sub mono">${esc(s.path)}</div></div><span class="badge codec">${esc((s.video && s.video.codec) || "—")}</span></div>`).join("")}</div>`
                : `<div class="empty" style="padding:22px 8px"><h4>暂无发布者</h4><span class="faint">等待 RTMP/RTSP 推流</span></div>`}
            </div>
          </div>
          <div class="panel">
            <div class="panel-head"><h3>累计指标</h3></div>
            <div class="panel-body"><div class="kv">
              <dt>接收帧</dt><dd class="mono">${num(o.accepted_frames)}</dd>
              <dt>丢弃帧</dt><dd class="mono">${num(o.dropped_frames)}</dd>
              <dt>RTP 包</dt><dd class="mono">${num(o.rtp_packets)}</dd>
              <dt>RTP 字节</dt><dd class="mono">${bytes(o.rtp_octets)}</dd>
            </div></div>
          </div>
        </div>
      </div>`;
  }

  function statusBadge(stream) {
    return stream.publisher ? `<span class="badge live">● 直播中</span>` : `<span class="badge off">待机</span>`;
  }

  function streamsTable(items, compact = false) {
    return `<div class="table-wrap"><table class="data">
      <thead><tr><th>流</th><th>状态</th><th>视频</th><th>音频</th><th>订阅</th>${compact ? "" : "<th>接收 / 丢弃</th><th></th>"}</tr></thead>
      <tbody>${items.map((s) => `
        <tr>
          <td><div class="cell-main">${esc(s.name)}</div><div class="cell-sub mono">${esc(s.path)}</div></td>
          <td>${statusBadge(s)}</td>
          <td><span class="badge codec">${esc((s.video && s.video.codec) || "—")}</span><div class="cell-sub">${esc(resolution(s.video))} ${esc(fps(s.video))}</div></td>
          <td><span class="faint">${esc(audioLabel(s.audio))}</span></td>
          <td class="mono">${num(s.subscribers)}</td>
          ${compact ? "" : `<td class="mono"><span class="ok">${num(s.accepted_frames)}</span> / <span class="bad">${num(s.dropped_frames)}</span></td>
          <td><div class="row-actions">
            <button class="btn small" data-action="play" data-path="${esc(s.path)}" data-kind="live">${icon("play")} 播放</button>
            <button class="btn small ghost" data-action="copy" data-copy="${esc(s.hls_url)}" title="复制 HLS 地址">${icon("copy")}</button>
            ${state.session.readonly ? "" : `<button class="btn small danger" data-action="kick" data-path="${esc(s.path)}" title="断开并移除流">${icon("power")}</button>`}
          </div></td>`}
        </tr>`).join("")}
      </tbody></table></div>`;
  }

  function empty(title, hint) {
    return `<div class="empty"><div class="empty-icon">${icon("filmSmall")}</div><h4>${esc(title)}</h4><div class="faint">${esc(hint)}</div></div>`;
  }

  function livePage() {
    const q = state.query.trim().toLowerCase();
    const items = state.streams.filter((s) => {
      const match = !q || s.path.toLowerCase().includes(q) || s.name.toLowerCase().includes(q) || ((s.video && s.video.codec) || "").toLowerCase().includes(q);
      const status = state.status === "all" || (state.status === "live" ? s.publisher : !s.publisher);
      return match && status;
    });
    return `
      <div class="toolbar">
        <div class="search" style="position:relative">${icon("search")}<input class="input" data-search placeholder="搜索名称、路径或编码" value="${esc(state.query)}" style="padding-left:38px"></div>
        <div class="seg">
          <button class="${state.status === "all" ? "active" : ""}" data-status="all">全部</button>
          <button class="${state.status === "live" ? "active" : ""}" data-status="live">直播中</button>
          <button class="${state.status === "idle" ? "active" : ""}" data-status="idle">待机</button>
        </div>
        <span class="muted" style="margin-left:auto;font-size:12px">${items.length} 条</span>
      </div>
      <div class="panel">${items.length ? streamsTable(items) : empty("没有匹配的流", "调整筛选条件或等待推流")}</div>`;
  }

  function vodPage() {
    const q = state.query.trim().toLowerCase();
    const items = state.vods.filter((v) => !q || v.title.toLowerCase().includes(q) || v.id.toLowerCase().includes(q));
    return `
      <div class="toolbar">
        <div class="search" style="position:relative">${icon("search")}<input class="input" data-search placeholder="搜索点播名称或 ID" value="${esc(state.query)}" style="padding-left:38px"></div>
        <span class="muted" style="margin-left:auto;font-size:12px">${items.length} 条</span>
      </div>
      ${items.length ? `<div class="vod-grid">${items.map((v) => `
        <article class="vod-card">
          <div class="vod-thumb">${icon("film")}</div>
          <div><h4>${esc(v.title)}</h4><div class="cell-sub mono">${esc(v.id)}</div></div>
          <div class="vod-actions">
            <button class="btn small primary" data-action="play" data-id="${esc(v.id)}" data-kind="vod">${icon("play")} 播放</button>
            <button class="btn small ghost" data-action="copy" data-copy="${esc(v.playlist)}">${icon("copy")} 地址</button>
            ${state.session.readonly ? "" : `<button class="btn small ghost" data-action="refresh-vod" data-id="${esc(v.id)}" title="清除缓存并重新生成">${icon("refresh")}</button>`}
          </div>
        </article>`).join("")}</div>` : empty("暂无可用的点播资源", "在服务端配置录制目录或文件 VOD 后显示")}`;
  }

  function systemPage() {
    const s = state.system || {};
    const features = (state.session.features || []).map((f) => `<span class="badge">${esc(f)}</span>`).join("");
    return `<div class="two-col">
      <div class="panel"><div class="panel-head"><h3>运行时</h3><span class="sub">只读</span></div><div class="panel-body"><dl class="kv">
        <dt>版本</dt><dd class="mono">${esc(s.version || "—")}</dd>
        <dt>运行时长</dt><dd>${esc(uptime(s.uptime_seconds))}</dd>
        <dt>当前流</dt><dd class="mono">${num(s.mounts)}</dd>
        <dt>控制台路径</dt><dd class="mono">${esc(s.base_path || BASE)}</dd>
        <dt>HLS 前缀</dt><dd class="mono">${esc(s.hls_prefix || "/hls")}</dd>
        <dt>RTMP 基础</dt><dd class="mono">${esc(s.rtmp_base || "未配置")}</dd>
      </dl></div></div>
      <div class="stack">
        <div class="panel"><div class="panel-head"><h3>构建特性</h3></div><div class="panel-body"><div class="feature-list">${features || '<span class="faint">无</span>'}</div></div></div>
        <div class="panel"><div class="panel-head"><h3>安全提示</h3></div><div class="panel-body note">
          控制台通过 HttpOnly + SameSite=Strict 会话 Cookie 认证，写操作额外校验 CSRF Token。生产部署请置于 HTTPS 反向代理之后，并设置 <code>secure_cookies</code>。
        </div></div>
      </div>
    </div>`;
  }

  function openModal(title, bodyHtml) {
    state.modal = title;
    modalRoot.innerHTML = `<div class="modal-backdrop" data-action="modal-backdrop"><div class="modal" role="dialog" aria-modal="true" aria-label="${esc(title)}">
      <div class="modal-head"><h3>${esc(title)}</h3><button class="btn small ghost" data-action="close-modal" aria-label="关闭">${icon("close")}</button></div>
      <div class="modal-body">${bodyHtml}</div></div></div>`;
  }

  function closeModal() {
    state.modal = null;
    modalRoot.innerHTML = "";
    if (state.hls) { try { state.hls.destroy(); } catch (_) {} state.hls = null; }
  }

  function loadHls() {
    if (window.Hls) return Promise.resolve(window.Hls);
    if (state.hlsLoading) return state.hlsLoading;
    state.hlsLoading = new Promise((resolve, reject) => {
      const script = document.createElement("script");
      script.src = BASE + "/assets/hls.min.js";
      script.onload = () => resolve(window.Hls);
      script.onerror = () => {
        // Fallback for deployments that serve an older asset cache; the
        // console remains usable with the bundled copy in normal builds.
        const fallback = document.createElement("script");
        fallback.src = "https://cdn.jsdelivr.net/npm/hls.js@1.5.13/dist/hls.min.js";
        fallback.crossOrigin = "anonymous";
        fallback.onload = () => resolve(window.Hls);
        fallback.onerror = () => reject(new Error("无法加载 HLS 播放器"));
        document.head.appendChild(fallback);
      };
      document.head.appendChild(script);
    });
    return state.hlsLoading;
  }

  async function playUrl(url, title) {
    openModal(title, `<video id="admin-video" controls autoplay muted playsinline></video><div class="player-status" id="player-status">正在加载…</div>`);
    const video = document.getElementById("admin-video");
    const status = document.getElementById("player-status");
    // Chromium/Edge may report a truthy canPlayType("application/vnd.apple.mpegurl")
    // while its native HLS path cannot decode our fMP4 (or is not enabled).
    // Prefer MSE/hls.js whenever supported; use native HLS only on Safari/iOS
    // where MediaSource HLS is unavailable.
    const native = video.canPlayType("application/vnd.apple.mpegurl");
    try {
      const Hls = await loadHls().catch(() => null);
      if (Hls && Hls.isSupported()) {
        state.hls = new Hls({
          liveDurationInfinity: true,
          enableWorker: false,
          // Keep more of the live window buffered than hls.js's 3-target
          // default; high-fps streams produce short segments and can hit
          // the edge before the next segment finishes.
          liveSyncDurationCount: 3,
          maxBufferLength: 10,
          liveMaxLatencyDurationCount: 6,
          backBufferLength: 10,
        });
        state.hls.on(Hls.Events.ERROR, (_event, data) => {
          if (data && data.fatal) {
            status.textContent = "播放失败：" + (data.details || "未知错误") +
              (data.reason ? " (" + data.reason + ")" : "") +
              (data.error && data.error.message ? " · " + data.error.message : "");
          }
        });
        state.hls.loadSource(url);
        state.hls.attachMedia(video);
        status.textContent = "hls.js 播放";
      } else if (native) {
        video.src = url;
        status.textContent = "使用浏览器原生 HLS 播放";
      } else {
        throw new Error("当前浏览器不支持 HLS 播放");
      }
      video.play().catch(() => { status.textContent += "（点击播放开始）"; });
    } catch (error) {
      status.textContent = error.message;
    }
  }

  async function copyText(value) {
    try {
      await navigator.clipboard.writeText(value);
      toast("已复制到剪贴板", "ok");
    } catch (_) {
      const area = document.createElement("textarea");
      area.value = value; area.style.position = "fixed"; area.style.opacity = "0";
      document.body.appendChild(area); area.select();
      document.execCommand("copy"); area.remove();
      toast("已复制到剪贴板", "ok");
    }
  }

  async function kickStream(path) {
    if (!confirm(`确定断开并移除 ${path} 吗？该操作会关闭当前发布者与所有观看连接。`)) return;
    try { await api("/streams?path=" + encodeURIComponent(path), { method: "DELETE" }); toast("流已移除", "ok"); await loadAll(); render(); }
    catch (error) { toast(error.message, "bad"); }
  }

  async function refreshVod(id) {
    if (!confirm(`清除 ${id} 的 VOD 缓存并在下次访问时重新生成？`)) return;
    try { await api("/vods/" + encodeURIComponent(id) + "/refresh", { method: "POST" }); toast("缓存已清除，下次播放将重新生成", "ok"); await loadAll(); render(); }
    catch (error) { toast(error.message, "bad"); }
  }

  app.addEventListener("click", async (event) => {
    const nav = event.target.closest("[data-nav]");
    if (nav) {
      state.page = nav.dataset.nav; state.query = ""; state.status = "all"; closeModal(); render(); return;
    }
    const status = event.target.closest("[data-status]");
    if (status) { state.status = status.dataset.status; render(); return; }
    const actionEl = event.target.closest("[data-action]");
    if (!actionEl) return;
    const action = actionEl.dataset.action;
    if (action === "menu") { const sidebar = document.getElementById("sidebar"); if (sidebar) sidebar.classList.toggle("open"); return; }
    if (action === "logout") { try { await api("/logout", { method: "POST" }); } catch (_) {} state.session = { authenticated: false }; stopPolling(); renderLogin(); return; }
    if (action === "retry" || action === "refresh") { state.loading = true; state.error = ""; render(); try { await loadAll(); } catch (error) { state.error = error.message; } state.loading = false; render(); return; }
    if (action === "close-modal" || action === "modal-backdrop") { if (event.target === actionEl) closeModal(); return; }
    if (action === "play") {
      const kind = actionEl.dataset.kind;
      if (kind === "live") {
        const stream = state.streams.find((s) => s.path === actionEl.dataset.path);
        if (stream) await playUrl(stream.hls_url, stream.name + " · 直播");
      } else {
        const vod = state.vods.find((v) => v.id === actionEl.dataset.id);
        if (vod) await playUrl(vod.playlist, vod.title + " · 点播");
      }
      return;
    }
    if (action === "copy") return copyText(actionEl.dataset.copy || "");
    if (action === "kick") return kickStream(actionEl.dataset.path);
    if (action === "refresh-vod") return refreshVod(actionEl.dataset.id);
  });

  app.addEventListener("input", (event) => {
    if (event.target.matches("[data-search]")) {
      state.query = event.target.value;
      const page = state.page;
      render();
      if (page === state.page) {
        const input = document.querySelector("[data-search]");
        if (input) { input.focus(); input.setSelectionRange(input.value.length, input.value.length); }
      }
    }
  });

  modalRoot.addEventListener("click", (event) => {
    if (event.target.matches("[data-action=modal-backdrop]")) closeModal();
    if (event.target.closest("[data-action=close-modal]")) closeModal();
  });

  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && state.modal) closeModal();
  });

  (async function boot() {
    app.innerHTML = `<div class="login-wrap"><div class="login-panel" style="grid-column:1/-1"><div class="login-card"><h2>rtflow</h2><p class="sub">正在检查登录状态…</p><div class="skeleton" style="height:38px;margin-top:18px"></div></div></div></div>`;
    try {
      state.session = await api("/session");
      if (!state.session.authenticated) { renderLogin(); return; }
      try { await loadAll(); } catch (error) { state.error = error.message; }
      state.loading = false;
      render();
      startPolling();
    } catch (error) {
      state.loading = false;
      state.session = { authenticated: false };
      renderLogin();
      toast(error.message, "bad");
    }
  })();
})();
