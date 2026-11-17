import { apiV1 } from "../api.js";
import { bytes, empty, esc, exportCsv, num } from "../format.js";
import { confirmDialog, openDrawer, toast } from "../ui.js";
import { openSrtDrawer } from "../forms/srt.js";

let tab = "pull";
let query = "";
let connection = "all";

export async function render(params, ctx) {
  query = params?.get("q") || "";
  const linkedTab = params?.get("tab");
  tab = ["pull", "push"].includes(linkedTab) ? linkedTab : "pull";
  const linkedConn = params?.get("conn");
  connection = ["connected", "offline"].includes(linkedConn) ? linkedConn : "all";
  const [pulls, pushes] = await Promise.all([apiV1("/srt/pulls"), apiV1("/srt/pushes")]);
  const all = { pull: pulls.items || [], push: pushes.items || [] };
  const initial = view(all);
  return {
    title: "SRT",
    subtitle: "SRT 拉流与推流端点，保存后单端点热更新。",
    actions: `<button class="btn small ghost" data-srt-export>导出</button><button class="btn small primary" data-action="create">${tab === "push" ? "新增推流" : "新增拉流"}</button><button class="btn small" data-action="refresh">刷新</button>`,
    html: `<div class="tabs"><button class="tab ${tab === "pull" ? "active" : ""}" data-srt-tab="pull">拉流</button><button class="tab ${tab === "push" ? "active" : ""}" data-srt-tab="push">推流</button></div>
      <div class="toolbar"><div class="search"><input class="input" data-srt-search value="${esc(query)}" placeholder="搜索名称、ID 或远端地址"></div><div class="seg"><button data-srt-conn="all" class="${connection === "all" ? "active" : ""}">全部</button><button data-srt-conn="connected" class="${connection === "connected" ? "active" : ""}">已连接</button><button data-srt-conn="offline" class="${connection === "offline" ? "active" : ""}">未连接</button></div><span class="meta" data-srt-count>${initial.length} 条 · ${initial.filter((item) => item.connected).length} 已连接</span></div>
      <section class="panel" data-srt-panel>${initial.length ? table(initial) : empty(`暂无 SRT ${tab === "push" ? "推流" : "拉流"}`, "创建第一个端点开始推流或拉流", "srt", `<a class="btn small" href="#/config/srt?new=1&tab=${tab}">新增${tab === "push" ? "推流" : "拉流"}</a>`)}</section>`,
    mount(root, ctx2) {
      const panel = root.querySelector("[data-srt-panel]");
      const count = root.querySelector("[data-srt-count]");
      const search = root.querySelector("[data-srt-search]");
      const create = root.querySelector("[data-action=create]");
      let visible = initial;
      const paint = () => {
        visible = view(all);
        count.textContent = `${visible.length} 条 · ${visible.filter((item) => item.connected).length} 已连接`;
        panel.innerHTML = visible.length ? table(visible) : empty("没有匹配的 SRT 端点", "调整搜索条件或切换推拉流", "srt", `<a class="btn small" href="#/config/srt?new=1&tab=${tab}">新增${tab === "push" ? "推流" : "拉流"}</a>`);
        root.querySelectorAll("[data-srt-tab]").forEach((button) => button.classList.toggle("active", button.dataset.srtTab === tab));
        create.textContent = tab === "push" ? "新增推流" : "新增拉流";
      };
      const syncUrl = () => {
        const params = new URLSearchParams();
        if (tab !== "pull") params.set("tab", tab);
        if (query.trim()) params.set("q", query.trim());
        if (connection !== "all") params.set("conn", connection);
        const qs = params.toString();
        history.replaceState(null, "", `#/config/srt${qs ? `?${qs}` : ""}`);
      };
      search.oninput = () => { query = search.value; paint(); syncUrl(); };
      root.querySelectorAll("[data-srt-tab]").forEach((button) => button.onclick = () => { tab = button.dataset.srtTab; paint(); syncUrl(); });
      root.querySelectorAll("[data-srt-conn]").forEach((button) => button.onclick = () => {
        connection = button.dataset.srtConn;
        root.querySelectorAll("[data-srt-conn]").forEach((item) => item.classList.toggle("active", item === button));
        paint(); syncUrl();
      });
      root.querySelector("[data-action=refresh]").onclick = () => ctx2.reload();
      root.querySelector("[data-srt-export]").onclick = () => {
        exportCsv(`srt-${tab}-${new Date().toISOString().slice(0, 10)}.csv`,
          ["direction", "id", "name", "enabled", "connected", "remote", "mount", "stream_id", "latency_ms", "pbkeylen", "bytes", "last_error"],
          visible.map((item) => [tab, item.id, item.name || "", item.enabled, item.connected, item.remote || "", item.mount || "", item.stream_id || "", item.latency_ms || "", item.pbkeylen || 0, item.bytes || 0, item.last_error || ""]));
        toast(`已导出 ${visible.length} 个 SRT 端点`, "ok");
      };
      create.onclick = () => openSrtDrawer(tab, null, () => ctx2.reload());
      if (params?.get("new") === "1") {
        history.replaceState(null, "", `#/config/srt${tab === "pull" ? "" : "?tab=push"}`);
        openSrtDrawer(tab, null, () => ctx2.reload());
      }
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
  let timer = null;
  let enabled = item.enabled;
  openDrawer({
    title: item.name || item.id,
    description: `SRT ${direction}端点`,
    showSubmit: false,
    body: `<div class="stack">
      <div class="detail-grid">
        <div class="detail-item"><span>状态</span><strong data-srt-detail="status">${item.enabled ? "启用" : "停用"}</strong></div>
        <div class="detail-item"><span>连接</span><strong data-srt-detail="connected">${item.connected ? "已连接" : "未连接"}</strong></div>
        <div class="detail-item"><span>远端</span><strong class="mono">${esc(item.remote || "—")}</strong></div>
        <div class="detail-item"><span>Mount</span><strong class="mono">${esc(item.mount || "—")}</strong></div>
        <div class="detail-item"><span>Stream ID</span><strong class="mono">${esc(item.stream_id || "—")}</strong></div>
        <div class="detail-item"><span>已传输</span><strong class="mono" data-srt-detail="bytes">${bytes(item.bytes || 0)}</strong></div>
        <div class="detail-item"><span>延迟</span><strong class="mono">${item.latency_ms ? `${num(item.latency_ms)} ms` : "默认"}</strong></div>
        <div class="detail-item"><span>加密</span><strong>${item.pbkeylen ? `AES-${item.pbkeylen * 8}` : "未启用"}</strong></div>
      </div>
      <div data-srt-error>${item.last_error ? `<div class="error-banner">${esc(item.last_error)}</div>` : ""}</div>
      <div class="actions"><button class="btn small" data-srt-edit>编辑</button><button class="btn small ghost" data-srt-toggle>${item.enabled ? "停用" : "启用"}</button>
        <button class="btn small ghost" data-srt-copy>复制远端</button>${item.mount ? `<button class="btn small ghost" data-srt-live>查看 Mount</button>` : ""}<button class="btn small danger" data-srt-delete>删除</button></div>
    </div>`,
    onClose() { if (timer) clearInterval(timer); },
    onMount(root) {
      const close = () => root.querySelector("[data-close-drawer]")?.click();
      const update = async () => {
        if (document.hidden) return;
        let current;
        try {
          const payload = await apiV1(`/srt/${tab === "push" ? "pushes" : "pulls"}`);
          current = (payload.items || []).find((entry) => entry.id === item.id);
        } catch (_) { return; }
        const set = (name, value) => { const node = root.querySelector(`[data-srt-detail="${name}"]`); if (node) node.textContent = value; };
        if (!current) { set("status", "已删除"); set("connected", "—"); return; }
        enabled = current.enabled;
        set("status", current.enabled ? "启用" : "停用");
        set("connected", current.connected ? "已连接" : "未连接");
        set("bytes", bytes(current.bytes || 0));
        root.querySelector("[data-srt-error]").innerHTML = current.last_error ? `<div class="error-banner">${esc(current.last_error)}</div>` : "";
        root.querySelector("[data-srt-toggle]").textContent = current.enabled ? "停用" : "启用";
      };
      timer = setInterval(update, 2000);
      root.querySelector("[data-srt-edit]").onclick = () => { close(); openSrtDrawer(tab, item, () => ctx.reload()); };
      root.querySelector("[data-srt-copy]").onclick = async () => {
        try { await navigator.clipboard.writeText(item.remote || ""); toast("远端地址已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
      };
      root.querySelector("[data-srt-live]")?.addEventListener("click", () => {
        close();
        location.hash = `#/live?q=${encodeURIComponent(item.mount || "")}`;
      });
      root.querySelector("[data-srt-toggle]").onclick = async () => {
        const body = { name: item.name, enabled: !enabled, remote: item.remote, stream_id: item.stream_id || null,
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
