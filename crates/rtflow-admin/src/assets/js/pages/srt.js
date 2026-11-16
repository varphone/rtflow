import { apiV1 } from "../api.js";
import { bytes, empty, esc, num } from "../format.js";
import { confirmDialog, openDrawer, toast } from "../ui.js";
import { openSrtDrawer } from "../forms/srt.js";

let tab = "pull";
let query = "";

export async function render(params, ctx) {
  const linked = params?.get("q");
  if (linked !== null && linked !== undefined) query = linked;
  const [pulls, pushes] = await Promise.all([apiV1("/srt/pulls"), apiV1("/srt/pushes")]);
  const all = { pull: pulls.items || [], push: pushes.items || [] };
  const initial = view(all);
  return {
    title: "SRT",
    subtitle: "SRT 拉流与推流端点，保存后单端点热更新。",
    actions: `<button class="btn small primary" data-action="create">${tab === "push" ? "新增推流" : "新增拉流"}</button><button class="btn small" data-action="refresh">刷新</button>`,
    html: `<div class="tabs"><button class="tab ${tab === "pull" ? "active" : ""}" data-srt-tab="pull">拉流</button><button class="tab ${tab === "push" ? "active" : ""}" data-srt-tab="push">推流</button></div>
      <div class="toolbar"><div class="search"><input class="input" data-srt-search value="${esc(query)}" placeholder="搜索名称、ID 或远端地址"></div><span class="meta" data-srt-count>${initial.length} 条</span></div>
      <section class="panel" data-srt-panel>${initial.length ? table(initial) : empty(`暂无 SRT ${tab === "push" ? "推流" : "拉流"}`, "点击右上角创建第一个端点")}</section>`,
    mount(root, ctx2) {
      const panel = root.querySelector("[data-srt-panel]");
      const count = root.querySelector("[data-srt-count]");
      const search = root.querySelector("[data-srt-search]");
      const create = root.querySelector("[data-action=create]");
      let visible = initial;
      const paint = () => {
        visible = view(all);
        count.textContent = `${visible.length} 条`;
        panel.innerHTML = visible.length ? table(visible) : empty("没有匹配的 SRT 端点", "调整搜索条件或切换推拉流");
        root.querySelectorAll("[data-srt-tab]").forEach((button) => button.classList.toggle("active", button.dataset.srtTab === tab));
        create.textContent = tab === "push" ? "新增推流" : "新增拉流";
      };
      search.oninput = () => { query = search.value; paint(); };
      root.querySelectorAll("[data-srt-tab]").forEach((button) => button.onclick = () => { tab = button.dataset.srtTab; paint(); });
      root.querySelector("[data-action=refresh]").onclick = () => ctx2.reload();
      create.onclick = () => openSrtDrawer(tab, null, () => ctx2.reload());
      root.addEventListener("click", async (event) => {
        const target = event.target.closest("[data-action]");
        if (!target || target === create) return;
        const item = visible.find((entry) => entry.id === target.dataset.id);
        if (!item) return;
        if (target.dataset.action === "detail") return openSrtDetail(item, tab, ctx2);
        if (target.dataset.action === "edit") return openSrtDrawer(tab, item, () => ctx2.reload());
        if (target.dataset.action === "toggle") {
          const body = { name: item.name, enabled: !item.enabled, remote: item.remote, stream_id: item.stream_id || null,
            mount: item.mount || null, pbkeylen: item.pbkeylen || 0, latency_ms: item.latency_ms || null };
          try { await apiV1(`/srt/${tab === "push" ? "pushes" : "pulls"}/${encodeURIComponent(item.id)}`, { method: "PATCH", body }); toast("状态已更新", "ok"); ctx2.reload(); }
          catch (error) { toast(error.message, "bad"); }
        }
        if (target.dataset.action === "delete") {
          if (!await confirmDialog({ title: "删除 SRT 端点", message: `删除 ${item.id}？`, confirmLabel: "删除", danger: true })) return;
          try { await apiV1(`/srt/${tab === "push" ? "pushes" : "pulls"}/${encodeURIComponent(item.id)}`, { method: "DELETE" }); toast("已删除", "ok"); ctx2.reload(); }
          catch (error) { toast(error.message, "bad"); }
        }
      });
    },
  };
}

function view(all) {
  const q = query.trim().toLowerCase();
  return (all[tab] || []).filter((item) => !q || `${item.name || ""} ${item.id} ${item.remote || ""}`.toLowerCase().includes(q));
}

function table(items) {
  return `<div class="table-wrap"><table class="data"><thead><tr><th>名称</th><th>状态</th><th>远端</th><th>Mount</th><th>连接</th><th>流量</th><th>错误</th><th></th></tr></thead><tbody>
    ${items.map((item) => `<tr><td><button class="link-cell" data-action="detail" data-id="${esc(item.id)}">${esc(item.name || item.id)}</button><div class="cell-sub mono">${esc(item.id)}</div></td>
      <td>${item.enabled ? '<span class="badge ok">启用</span>' : '<span class="badge">停用</span>'}</td><td class="mono">${esc(item.remote)}</td>
      <td class="mono">${esc(item.mount || "—")}</td><td>${item.connected ? '<span class="badge ok">已连接</span>' : "—"}</td><td class="mono">${bytes(item.bytes || 0)}</td>
      <td class="cell-sub">${esc(item.last_error || "—")}</td><td><div class="row-actions">
        <button class="btn small" data-action="edit" data-id="${esc(item.id)}">编辑</button>
        <button class="btn small ghost" data-action="toggle" data-id="${esc(item.id)}">${item.enabled ? "停用" : "启用"}</button>
        <button class="btn small danger" data-action="delete" data-id="${esc(item.id)}">删除</button></div></td></tr>`).join("")}
  </tbody></table></div>`;
}


function openSrtDetail(item, tab, ctx) {
  const direction = tab === "push" ? "推流" : "拉流";
  openDrawer({
    title: item.name || item.id,
    description: `SRT ${direction}端点`,
    showSubmit: false,
    body: `<div class="stack">
      <div class="detail-grid">
        <div class="detail-item"><span>状态</span><strong>${item.enabled ? "启用" : "停用"}</strong></div>
        <div class="detail-item"><span>连接</span><strong>${item.connected ? "已连接" : "未连接"}</strong></div>
        <div class="detail-item"><span>远端</span><strong class="mono">${esc(item.remote || "—")}</strong></div>
        <div class="detail-item"><span>Mount</span><strong class="mono">${esc(item.mount || "—")}</strong></div>
        <div class="detail-item"><span>Stream ID</span><strong class="mono">${esc(item.stream_id || "—")}</strong></div>
        <div class="detail-item"><span>已传输</span><strong class="mono">${bytes(item.bytes || 0)}</strong></div>
        <div class="detail-item"><span>延迟</span><strong class="mono">${item.latency_ms ? `${num(item.latency_ms)} ms` : "默认"}</strong></div>
        <div class="detail-item"><span>加密</span><strong>${item.pbkeylen ? `AES-${item.pbkeylen * 8}` : "未启用"}</strong></div>
      </div>
      ${item.last_error ? `<div class="error-banner">${esc(item.last_error)}</div>` : ""}
      <div class="actions"><button class="btn small" data-srt-edit>编辑</button><button class="btn small ghost" data-srt-toggle>${item.enabled ? "停用" : "启用"}</button>
        <button class="btn small ghost" data-srt-copy>复制远端</button>${item.mount ? `<button class="btn small ghost" data-srt-live>查看 Mount</button>` : ""}<button class="btn small danger" data-srt-delete>删除</button></div>
    </div>`,
    onMount(root) {
      const close = () => root.querySelector("[data-close-drawer]")?.click();
      root.querySelector("[data-srt-edit]").onclick = () => { close(); openSrtDrawer(tab, item, () => ctx.reload()); };
      root.querySelector("[data-srt-copy]").onclick = async () => {
        try { await navigator.clipboard.writeText(item.remote || ""); toast("远端地址已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
      };
      root.querySelector("[data-srt-live]")?.addEventListener("click", () => {
        close();
        location.hash = `#/live?q=${encodeURIComponent(item.mount || "")}`;
      });
      root.querySelector("[data-srt-toggle]").onclick = async () => {
        const body = { name: item.name, enabled: !item.enabled, remote: item.remote, stream_id: item.stream_id || null,
          mount: item.mount || null, pbkeylen: item.pbkeylen || 0, latency_ms: item.latency_ms || null };
        try { await apiV1(`/srt/${tab === "push" ? "pushes" : "pulls"}/${encodeURIComponent(item.id)}`, { method: "PATCH", body }); toast("状态已更新", "ok"); close(); ctx.reload(); }
        catch (error) { toast(error.message, "bad"); }
      };
      root.querySelector("[data-srt-delete]").onclick = async () => {
        if (!await confirmDialog({ title: "删除 SRT 端点", message: `删除 ${item.id}？`, confirmLabel: "删除", danger: true })) return;
        try { await apiV1(`/srt/${tab === "push" ? "pushes" : "pulls"}/${encodeURIComponent(item.id)}`, { method: "DELETE" }); toast("已删除", "ok"); close(); ctx.reload(); }
        catch (error) { toast(error.message, "bad"); }
      };
    },
  });
}
