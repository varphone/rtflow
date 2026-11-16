import { apiV1 } from "../api.js";
import { empty, esc, num, stateBadge } from "../format.js";
import { confirmDialog, openDialog, toast } from "../ui.js";
import { openProxyDrawer } from "../forms/proxy.js";

let query = "";
let filter = "all";
let sort = { field: "path", dir: 1 };

export async function render(params, ctx) {
  const linked = params?.get("q");
  if (linked !== null && linked !== undefined) query = linked;
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
      <span class="meta" data-proxy-count>${initial.length} 条</span></div>
      <section class="panel" data-proxy-panel>${initial.length ? table(initial) : empty("暂无代理", "新增一个 RTSP 拉流代理")}</section>`,
    mount(root) {
      const panel = root.querySelector("[data-proxy-panel]");
      const count = root.querySelector("[data-proxy-count]");
      const search = root.querySelector("[data-proxy-search]");
      let visible = initial;
      const paint = () => {
        visible = view(all);
        count.textContent = `${visible.length} 条`;
        root.querySelectorAll("[data-proxy-filter]").forEach((button) => button.classList.toggle("active", button.dataset.proxyFilter === filter));
        panel.innerHTML = visible.length ? table(visible) : empty("没有匹配的代理", "调整搜索或状态筛选");
      };
      search.oninput = () => { query = search.value; paint(); };
      root.querySelectorAll("[data-proxy-filter]").forEach((button) => button.onclick = () => { filter = button.dataset.proxyFilter; paint(); });
      root.querySelector("[data-action=refresh]").onclick = () => ctx.reload();
      root.querySelector("[data-action=create]").onclick = () => openProxyDrawer(null, () => ctx.reload());
      root.addEventListener("click", async (event) => {
        const sorter = event.target.closest("[data-sort]");
        if (sorter) {
          const field = sorter.dataset.sort;
          sort = { field, dir: sort.field === field ? -sort.dir : 1 };
          paint();
          return;
        }
        const target = event.target.closest("[data-action]");
        if (!target) return;
        const path = target.dataset.path;
        const item = visible.find((entry) => entry.path === path);
        if (!item) return;
        if (target.dataset.action === "edit") {
          openProxyDrawer(item, () => ctx.reload());
        } else if (target.dataset.action === "restart") {
          try { await apiV1("/proxies/restart?path=" + encodeURIComponent(path), { method: "POST" }); toast("重启已提交", "ok"); ctx.reload(); }
          catch (error) { toast(error.message, "bad"); }
        } else if (target.dataset.action === "probe") {
          try {
            const result = await apiV1("/proxies/probe?path=" + encodeURIComponent(path), { method: "POST" });
            openDialog({ title: "上游探测", body: `<div class="note">${esc(result.session_name || "")}</div><pre class="code">${esc(JSON.stringify(result.tracks || [], null, 2))}</pre>`, footer: `<button class="btn" data-close-dialog>关闭</button>` });
          } catch (error) { toast(error.message, "bad"); }
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
    ${items.map((item) => `<tr><td class="mono">${esc(item.path)}</td><td>${stateBadge(item.state)}</td><td class="mono">${esc(item.upstream || "—")}</td>
      <td class="mono">${num(item.subscribers)}</td><td class="mono">${num(item.tracks)}</td><td class="mono">${num(item.attempts)}</td><td class="cell-sub">${esc(item.last_error || "—")}</td>
      <td><div class="row-actions"><button class="btn small" data-action="edit" data-path="${esc(item.path)}">编辑</button>
        <button class="btn small ghost" data-action="restart" data-path="${esc(item.path)}">重启</button>
        <button class="btn small ghost" data-action="probe" data-path="${esc(item.path)}">探测</button>
        <button class="btn small danger" data-action="delete" data-path="${esc(item.path)}">删除</button></div></td></tr>`).join("")}
  </tbody></table></div>`;
}
