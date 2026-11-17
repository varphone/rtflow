import { apiV1 } from "../api.js";
import { bytes, empty, esc, num, stateBadge } from "../format.js";
import { confirmDialog, openDialog, openDrawer, syncSortAria, toast } from "../ui.js";
import { openProxyDrawer } from "../forms/proxy.js";

let query = "";
let filter = "all";
let sort = { field: "path", dir: 1 };

export async function render(params, ctx) {
  query = params?.get("q") || "";
  const linkedState = params?.get("state");
  filter = ["all", "streaming", "idle", "backoff", "describing"].includes(linkedState) ? linkedState : "all";
  const field = params?.get("sort");
  if (["path", "state", "attempts"].includes(field)) {
    sort = { field, dir: params?.get("dir") === "-1" ? -1 : 1 };
  } else {
    sort = { field: "path", dir: 1 };
  }
  const [status, config] = await Promise.all([apiV1("/proxies"), apiV1("/proxies/config")]);
  const statuses = status.proxies || [];
  const configs = config.mounts || [];
  const all = statuses.map((entry) => Object.assign({}, configs.find((mount) => mount.path === entry.path) || {}, entry));
  const initial = view(all);
  return {
    title: "RTSP 代理",
    subtitle: "拉流代理配置与运行状态。",
    actions: `<button class="btn small primary" data-action="create">新增代理</button><button class="btn small" data-action="refresh">刷新</button>`,
    html: `<div class="toolbar"><div class="search"><input class="input" data-proxy-search value="${esc(query)}" placeholder="搜索路径或上游"></div>
      <div class="seg">${["all", "streaming", "idle", "backoff", "describing"].map((state) => `<button data-proxy-filter="${state}" class="${filter === state ? "active" : ""}">${state === "all" ? "全部" : state}</button>`).join("")}</div>
      <span class="meta" data-proxy-count>${initial.length} 条 · ${initial.filter((item) => item.state === "streaming").length} streaming</span></div>
      <section class="panel" data-proxy-panel>${initial.length ? table(initial) : empty("暂无代理", "新增一个 RTSP 拉流代理", "proxy", `<a class="btn small" href="#/config/proxies?new=1">新增代理</a>`)}</section>`,
    mount(root) {
      const panel = root.querySelector("[data-proxy-panel]");
      const count = root.querySelector("[data-proxy-count]");
      const search = root.querySelector("[data-proxy-search]");
      syncSortAria(root.querySelector(".panel"), sort);
      let visible = initial;
      const paint = () => {
        visible = view(all);
        count.textContent = `${visible.length} 条 · ${visible.filter((item) => item.state === "streaming").length} streaming`;
        root.querySelectorAll("[data-proxy-filter]").forEach((button) => button.classList.toggle("active", button.dataset.proxyFilter === filter));
        panel.innerHTML = visible.length ? table(visible) : empty("没有匹配的代理", "调整搜索或状态筛选", "proxy");
        syncSortAria(panel, sort);
      };
      const syncUrl = () => {
        const params = new URLSearchParams();
        if (query.trim()) params.set("q", query.trim());
        if (filter !== "all") params.set("state", filter);
        if (sort.field !== "path" || sort.dir !== 1) params.set("sort", sort.field);
        if (sort.dir === -1) params.set("dir", "-1");
        const qs = params.toString();
        history.replaceState(null, "", `#/config/proxies${qs ? `?${qs}` : ""}`);
      };
      search.oninput = () => { query = search.value; paint(); syncUrl(); };
      root.querySelectorAll("[data-proxy-filter]").forEach((button) => button.onclick = () => { filter = button.dataset.proxyFilter; paint(); syncUrl(); });
      root.querySelector("[data-action=refresh]").onclick = () => ctx.reload();
      root.querySelector("[data-action=create]").onclick = () => openProxyDrawer(null, () => ctx.reload());
      if (params?.get("new") === "1") {
        history.replaceState(null, "", "#/config/proxies");
        openProxyDrawer(null, () => ctx.reload());
      }
      root.addEventListener("click", async (event) => {
        const sorter = event.target.closest("[data-sort]");
        if (sorter) {
          const field = sorter.dataset.sort;
          sort = { field, dir: sort.field === field ? -sort.dir : 1 };
          paint();
          syncUrl();
          return;
        }
        const target = event.target.closest("[data-action]");
        if (!target) return;
        const path = target.dataset.path;
        const item = visible.find((entry) => entry.path === path);
        if (!item) return;
        if (target.dataset.action === "detail") {
          openProxyDetail(item, ctx);
        } else if (target.dataset.action === "edit") {
          openProxyDrawer(item, () => ctx.reload());
        } else if (target.dataset.action === "restart") {
          try { await apiV1("/proxies/restart?path=" + encodeURIComponent(path), { method: "POST" }); toast("重启已提交", "ok"); ctx.reload(); }
          catch (error) { toast(error.message, "bad"); }
        } else if (target.dataset.action === "probe") {
          await probeProxy(path);
        } else if (target.dataset.action === "delete") {
          if (!await confirmDialog({ title: "删除代理", message: `删除 ${path}？`, confirmLabel: "删除", danger: true })) return;
          try { await apiV1("/proxies?path=" + encodeURIComponent(path), { method: "DELETE" }); toast("代理已删除", "ok"); ctx.reload(); }
          catch (error) { toast(error.message, "bad"); }
        }
      });
    },
  };
}

function view(all) {
  const q = query.trim().toLowerCase();
  const items = all.filter((item) => {
    const match = !q || item.path.toLowerCase().includes(q) || (item.upstream || "").toLowerCase().includes(q);
    return match && (filter === "all" || item.state === filter);
  });
  const value = (item) => sort.field === "state" ? String(item.state || "") : sort.field === "attempts" ? Number(item.attempts || 0) : String(item.path || "");
  return items.sort((left, right) => {
    const a = value(left), b = value(right);
    return (a < b ? -1 : a > b ? 1 : 0) * sort.dir;
  });
}

function table(items) {
  const arrow = (field) => sort.field === field ? (sort.dir > 0 ? " ↑" : " ↓") : " ↕";
  return `<div class="table-wrap"><table class="data"><thead><tr>
    <th><button class="sort-btn" data-sort="path">路径${arrow("path")}</button></th>
    <th><button class="sort-btn" data-sort="state">状态${arrow("state")}</button></th><th>上游</th><th>订阅</th><th>轨道</th>
    <th><button class="sort-btn" data-sort="attempts">重试${arrow("attempts")}</button></th><th>错误</th><th></th></tr></thead><tbody>
    ${items.map((item) => `<tr><td><button class="link-cell mono" data-action="detail" data-path="${esc(item.path)}">${esc(item.path)}</button></td><td>${stateBadge(item.state)}</td><td class="mono">${esc(item.upstream || "—")}</td>
      <td class="mono">${num(item.subscribers)}</td><td class="mono">${num(item.tracks)}</td><td class="mono">${num(item.attempts)}</td><td class="cell-sub">${esc(item.last_error || "—")}</td>
      <td><div class="row-actions"><button class="btn small" data-action="edit" data-path="${esc(item.path)}">编辑</button>
        <button class="btn small ghost" data-action="restart" data-path="${esc(item.path)}">重启</button>
        <button class="btn small ghost" data-action="probe" data-path="${esc(item.path)}">探测</button>
        <button class="btn small danger" data-action="delete" data-path="${esc(item.path)}">删除</button></div></td></tr>`).join("")}
  </tbody></table></div>`;
}


async function probeProxy(path) {
  try {
    const result = await apiV1("/proxies/probe?path=" + encodeURIComponent(path), { method: "POST" });
    openDialog({ title: "上游探测", body: `<div class="note">${esc(result.session_name || "")}</div><pre class="code">${esc(JSON.stringify(result.tracks || [], null, 2))}</pre>`, footer: `<button class="btn" data-close-dialog>关闭</button>` });
  } catch (error) { toast(error.message, "bad"); }
}

function openProxyDetail(item, ctx) {
  const upstreams = item.upstreams || [];
  openDrawer({
    title: item.path,
    description: "RTSP 代理运行状态与上游配置。",
    showSubmit: false,
    body: `<div class="stack">
      <div class="detail-grid">
        <div class="detail-item"><span>状态</span><strong>${esc(item.state || "—")}</strong></div>
        <div class="detail-item"><span>模式</span><strong>${esc(item.mode || "—")}</strong></div>
        <div class="detail-item"><span>订阅连接</span><strong class="mono">${num(item.subscribers)}</strong></div>
        <div class="detail-item"><span>轨道</span><strong class="mono">${num(item.tracks)}</strong></div>
        <div class="detail-item"><span>重试次数</span><strong class="mono">${num(item.attempts)}</strong></div>
        <div class="detail-item"><span>接收流量</span><strong class="mono">${bytes(item.bytes || 0)}</strong></div>
      </div>
      <section class="drawer-section"><h3>上游列表</h3>${upstreams.length ? upstreams.map((upstream, index) => `
        <div class="endpoint"><span class="endpoint-label">#${index + 1}</span><code>${esc(upstream.url || "—")}</code><span class="cell-sub">${esc(upstream.transport || "auto")}</span></div>`).join("") : `<div class="note">未配置上游。</div>`}
        ${upstreams.length ? `<div class="actions" style="margin-top:8px"><button class="btn small ghost" data-proxy-copy-all>复制上游列表</button></div>` : ""}</section>
      ${item.last_error ? `<div class="error-banner">${esc(item.last_error)}</div>` : ""}
      <div class="actions"><button class="btn small" data-pd-edit>编辑</button><button class="btn small" data-pd-restart>重启</button><button class="btn small ghost" data-pd-probe>探测上游</button><button class="btn small ghost" data-pd-live>查看直播</button><button class="btn small danger" data-pd-delete>删除</button></div>
    </div>`,
    onMount(root) {
      const close = () => root.querySelector("[data-close-drawer]")?.click();
      root.querySelector("[data-proxy-copy-all]")?.addEventListener("click", async () => {
        try { await navigator.clipboard.writeText(upstreams.map((upstream) => upstream.url || "").filter(Boolean).join("\n")); toast("上游列表已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
      });
      root.querySelector("[data-pd-edit]").onclick = () => { close(); openProxyDrawer(item, () => ctx.reload()); };
      root.querySelector("[data-pd-restart]").onclick = async () => {
        try { await apiV1("/proxies/restart?path=" + encodeURIComponent(item.path), { method: "POST" }); toast("重启已提交", "ok"); close(); ctx.reload(); }
        catch (error) { toast(error.message, "bad"); }
      };
      root.querySelector("[data-pd-probe]").onclick = () => probeProxy(item.path);
      root.querySelector("[data-pd-live]").onclick = () => { close(); location.hash = `#/live?q=${encodeURIComponent(item.path)}`; };
      root.querySelector("[data-pd-delete]").onclick = async () => {
        if (!await confirmDialog({ title: "删除代理", message: `删除 ${item.path}？`, confirmLabel: "删除", danger: true })) return;
        try { await apiV1("/proxies?path=" + encodeURIComponent(item.path), { method: "DELETE" }); toast("代理已删除", "ok"); close(); ctx.reload(); }
        catch (error) { toast(error.message, "bad"); }
      };
    },
  });
}
