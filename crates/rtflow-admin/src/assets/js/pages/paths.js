import { api, apiV1 } from "../api.js";
import { dateTime, empty, esc } from "../format.js";
import { confirmDialog, openDrawer, toast } from "../ui.js";
import { openPathDrawer } from "../forms/path.js";

let query = "";
let sort = { field: "name", dir: 1 };

export async function render(params, ctx) {
  query = params?.get("q") || "";
  const field = params?.get("sort");
  if (["name", "source", "max_readers", "updated_at"].includes(field)) {
    sort = { field, dir: params?.get("dir") === "-1" ? -1 : 1 };
  } else {
    sort = { field: "name", dir: 1 };
  }
  const [paths, system] = await Promise.all([apiV1("/paths/config"), api("/system")]);
  const all = Array.isArray(paths) ? paths : paths.items || [];
  const initial = view(all);
  return {
    title: "路径",
    subtitle: "路径策略、发布/观看开关与录制。",
    actions: `<button class="btn small primary" data-action="create">新建路径</button><button class="btn small" data-action="refresh">刷新</button>`,
    html: `<div class="toolbar"><div class="search"><input class="input" data-path-search value="${esc(query)}" placeholder="搜索路径"></div><span class="meta" data-path-count>${initial.length} 条 · ${initial.filter((item) => item.enabled).length} 启用</span></div>
      <section class="panel" data-path-panel>${initial.length ? table(initial) : empty("暂无路径记录", "新建路径或通过配置提供 glob 规则")}</section>`,
    mount(root) {
      const panel = root.querySelector("[data-path-panel]");
      const count = root.querySelector("[data-path-count]");
      const search = root.querySelector("[data-path-search]");
      let visible = initial;
      const paint = () => {
        visible = view(all);
        count.textContent = `${visible.length} 条 · ${visible.filter((item) => item.enabled).length} 启用`;
        panel.innerHTML = visible.length ? table(visible) : empty("没有匹配的路径", "调整搜索条件或新建路径");
      };
      const syncUrl = () => {
        const params = new URLSearchParams();
        if (query.trim()) params.set("q", query.trim());
        if (sort.field !== "name" || sort.dir !== 1) params.set("sort", sort.field);
        if (sort.dir === -1) params.set("dir", "-1");
        const qs = params.toString();
        history.replaceState(null, "", `#/config/paths${qs ? `?${qs}` : ""}`);
      };
      search.oninput = () => { query = search.value; paint(); syncUrl(); };
      root.querySelector("[data-action=refresh]").onclick = () => ctx.reload();
      root.querySelector("[data-action=create]").onclick = () => openPathDrawer(null, () => ctx.reload());
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
        if (!target || !["detail", "edit", "kick", "delete"].includes(target.dataset.action)) return;
        const item = visible.find((entry) => entry.name === target.dataset.name);
        if (!item) return;
        if (target.dataset.action === "detail") return openPathDetail(item, system, ctx);
        if (target.dataset.action === "edit") return openPathDrawer(item, () => ctx.reload());
        if (target.dataset.action === "kick") {
          try { await apiV1("/kick?path=" + encodeURIComponent(item.name), { method: "POST" }); toast("已断开", "ok"); ctx.reload(); }
          catch (error) { toast(error.message, "bad"); }
        }
        if (target.dataset.action === "delete") {
          if (!await confirmDialog({ title: "删除路径", message: `删除 ${item.name}？`, confirmLabel: "删除", danger: true })) return;
          try { await apiV1("/paths/" + encodeURIComponent(item.name), { method: "DELETE" }); toast("路径已删除", "ok"); ctx.reload(); }
          catch (error) { toast(error.message, "bad"); }
        }
      });
    },
  };
}

function view(all) {
  const q = query.trim().toLowerCase();
  const items = all.filter((item) => !q || item.name.toLowerCase().includes(q));
  const value = (item) => {
    if (sort.field === "name") return item.name.toLowerCase();
    if (sort.field === "updated_at") return Number(item.updated_at || 0);
    if (sort.field === "max_readers") return Number(item.max_readers || Number.MAX_SAFE_INTEGER);
    return String(item.source || "");
  };
  return items.sort((left, right) => {
    const a = value(left), b = value(right);
    return (a < b ? -1 : a > b ? 1 : 0) * sort.dir;
  });
}

function table(items) {
  const arrow = (field) => sort.field === field ? (sort.dir > 0 ? " ↑" : " ↓") : " ↕";
  return `<div class="table-wrap"><table class="data"><caption>共 ${items.length} 条路径</caption><thead><tr>
    <th><button class="sort-btn" data-sort="name">路径${arrow("name")}</button></th>
    <th><button class="sort-btn" data-sort="source">来源${arrow("source")}</button></th>
    <th>发布 / 观看</th><th>录制</th>
    <th><button class="sort-btn" data-sort="max_readers">最大读者${arrow("max_readers")}</button></th>
    <th><button class="sort-btn" data-sort="updated_at">更新时间${arrow("updated_at")}</button></th><th></th></tr></thead><tbody>
    ${items.map((item) => `<tr><td><button class="link-cell mono" data-action="detail" data-name="${esc(item.name)}">/${esc(item.name)}</button><div class="cell-sub">${item.enabled ? "启用" : "停用"}</div></td>
      <td><span class="badge">${esc(item.source)}</span></td><td>${item.publish ? '<span class="badge ok">发布</span>' : ""} ${item.read ? '<span class="badge ok">观看</span>' : '<span class="badge bad">不可读</span>'}</td>
      <td>${item.record ? '<span class="badge ok">开启</span>' : "—"}</td><td class="mono">${item.max_readers || "∞"}</td><td>${dateTime(item.updated_at)}</td>
      <td><div class="row-actions"><button class="btn small" data-action="edit" data-name="${esc(item.name)}">编辑</button>
        <button class="btn small ghost" data-action="kick" data-name="${esc(item.name)}">断开</button>
        <button class="btn small danger" data-action="delete" data-name="${esc(item.name)}">删除</button></div></td></tr>`).join("")}
  </tbody></table></div>`;
}


function openPathDetail(item, system, ctx) {
  const base = String(system.rtmp_base || `rtmp://${location.hostname}:1935`).replace(/\/$/, "");
  const hls = `${location.origin}${system.hls_prefix || "/hls"}/${item.name}/index.m3u8`;
  const endpoint = (label, url) => `<div class="endpoint"><span class="endpoint-label">${label}</span><code>${esc(url)}</code><button class="btn small ghost" data-copy="${esc(url)}">复制</button></div>`;
  const flags = [item.publish && "发布", item.read && "观看", item.record && "录制"].filter(Boolean).join(" · ") || "无";
  const protocols = [item.hls && "HLS", item.dash && "DASH", item.flv && "HTTP-FLV", item.webrtc && "WebRTC"].filter(Boolean).join(" · ") || "未开启输出";
  openDrawer({
    title: `/${item.name}`,
    description: "路径策略、输出协议与关联资源。",
    showSubmit: false,
    body: `<div class="stack">
      <div class="detail-grid">
        <div class="detail-item"><span>状态</span><strong>${item.enabled ? "启用" : "停用"}</strong></div>
        <div class="detail-item"><span>来源</span><strong>${esc(item.source)}</strong></div>
        <div class="detail-item"><span>允许操作</span><strong>${esc(flags)}</strong></div>
        <div class="detail-item"><span>最大读者</span><strong class="mono">${item.max_readers || "∞"}</strong></div>
        <div class="detail-item"><span>租户</span><strong>${esc(item.tenant || "—")}</strong></div>
        <div class="detail-item"><span>修订</span><strong class="mono">${esc(item.revision || 1)}</strong></div>
      </div>
      <section class="drawer-section"><h3>输出协议</h3><div class="feature-list">${protocols.split(" · ").map((value) => `<span class="badge">${esc(value)}</span>`).join("")}</div></section>
      <section class="drawer-section"><h3>时间信息</h3><div class="detail-grid">
        <div class="detail-item"><span>创建</span><strong>${esc(dateTime(item.created_at))}</strong></div>
        <div class="detail-item"><span>更新</span><strong>${esc(dateTime(item.updated_at))}</strong></div>
      </div></section>
      <section class="drawer-section"><h3>访问地址</h3>${endpoint("HLS", hls)}${endpoint("RTMP", `${base}/${item.name}`)}</section>
      <section class="drawer-section"><h3>关联资源</h3><div class="actions">
        <button class="btn small" data-goto="#/live?q=${encodeURIComponent(item.name)}">查看直播</button>
        <button class="btn small" data-goto="#/vod?q=${encodeURIComponent(item.name)}">查看点播</button>
        <button class="btn small" data-goto="#/config/recordings?q=${encodeURIComponent(item.name)}">查看录制</button>
      </div></section>
      <div class="actions"><button class="btn small" data-detail-edit>编辑策略</button><button class="btn small danger" data-detail-kick>断开该路径</button></div>
    </div>`,
    onMount(root) {
      root.querySelectorAll("[data-copy]").forEach((button) => button.onclick = async () => {
        try { await navigator.clipboard.writeText(button.dataset.copy); toast("地址已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
      });
      root.querySelectorAll("[data-goto]").forEach((button) => button.onclick = () => {
        root.querySelector("[data-close-drawer]")?.click();
        location.hash = button.dataset.goto;
      });
      root.querySelector("[data-detail-edit]").onclick = () => {
        root.querySelector("[data-close-drawer]")?.click();
        openPathDrawer(item, () => ctx.reload());
      };
      root.querySelector("[data-detail-kick]").onclick = async () => {
        try { await apiV1("/kick?path=" + encodeURIComponent(item.name), { method: "POST" }); toast("已断开", "ok"); root.querySelector("[data-close-drawer]")?.click(); ctx.reload(); }
        catch (error) { toast(error.message, "bad"); }
      };
    },
  });
}
