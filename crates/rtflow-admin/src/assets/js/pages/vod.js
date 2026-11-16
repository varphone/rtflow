import { api } from "../api.js";
import { dateTime, duration, empty, esc, iconSvg } from "../format.js";
import { confirmDialog, toast } from "../ui.js";
import { playUrl } from "../player.js";

let query = "";

export async function render() {
  const payload = await api("/vods");
  const vods = payload.items || payload;
  const initial = filter(vods, query);
  return {
    title: "点播",
    subtitle: "录制目录与文件 VOD 资源。",
    actions: `<button class="btn small" data-vod-refresh>刷新</button>`,
    html: `<div class="toolbar"><label class="checkline"><input type="checkbox" data-vod-select-all> 全选</label>
      <div class="search">${iconSvg("search")}<input class="input" data-vod-search value="${esc(query)}" placeholder="搜索点播名称或 ID"></div>
      <span class="meta" data-vod-count>${initial.length} 条</span>
      <button class="btn small ghost" data-vod-bulk disabled>刷新所选缓存</button></div>
      <div data-vod-results>${initial.length ? cards(initial, new Set()) : empty("暂无可用的点播资源", "在服务端配置录制目录或文件 VOD 后显示")}</div>`,
    mount(root, ctx) {
      const search = root.querySelector("[data-vod-search]");
      const results = root.querySelector("[data-vod-results]");
      const count = root.querySelector("[data-vod-count]");
      const selectAll = root.querySelector("[data-vod-select-all]");
      const bulk = root.querySelector("[data-vod-bulk]");
      const selected = new Set();
      let visible = initial;
      const sync = () => {
        bulk.disabled = selected.size === 0;
        bulk.textContent = selected.size ? `刷新所选缓存 (${selected.size})` : "刷新所选缓存";
        const picked = visible.filter((item) => selected.has(item.id)).length;
        selectAll.checked = visible.length > 0 && picked === visible.length;
        selectAll.indeterminate = picked > 0 && picked < visible.length;
      };
      const paint = () => {
        visible = filter(vods, search.value);
        count.textContent = `${visible.length} 条`;
        results.innerHTML = visible.length ? cards(visible, selected) : empty("没有匹配的点播", "调整搜索条件");
        sync();
      };
      root.addEventListener("change", (event) => {
        const row = event.target.closest("[data-vod-select]");
        if (row) {
          if (row.checked) selected.add(row.dataset.vodSelect); else selected.delete(row.dataset.vodSelect);
          row.closest(".vod-card")?.classList.toggle("selected", row.checked);
          sync();
          return;
        }
        if (event.target === selectAll) {
          for (const item of visible) {
            if (selectAll.checked) selected.add(item.id); else selected.delete(item.id);
          }
          paint();
        }
      });
      bulk.onclick = async () => {
        if (!selected.size) return;
        if (!await confirmDialog({ title: "刷新点播缓存", message: `刷新所选 ${selected.size} 个点播的生成缓存？`, confirmLabel: "刷新", danger: false })) return;
        let failed = 0;
        for (const id of [...selected]) {
          try { await api(`/vods/${encodeURIComponent(id)}/refresh`, { method: "POST" }); }
          catch (_) { failed += 1; }
        }
        toast(failed ? `已刷新 ${selected.size - failed} 个，${failed} 个失败` : `已刷新 ${selected.size} 个缓存`, failed ? "bad" : "ok");
        selected.clear();
        ctx.reload();
      };
      search.oninput = () => { query = search.value; paint(); };
      root.querySelector("[data-vod-refresh]").onclick = () => ctx.reload();
      root.addEventListener("click", async (event) => {
        const target = event.target.closest("[data-action]");
        if (!target) return;
        const vod = visible.find((item) => item.id === target.dataset.id);
        if (target.dataset.action === "play" && vod) playUrl(vod.playlist, vod.title, { vod: true });
        else if (target.dataset.action === "copy") {
          try { await navigator.clipboard.writeText(target.dataset.url); toast("地址已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
        } else if (target.dataset.action === "refresh-vod" && vod) {
          try { await api(`/vods/${encodeURIComponent(vod.id)}/refresh`, { method: "POST" }); toast("缓存已清除，下次播放重新生成", "ok"); ctx.reload(); }
          catch (error) { toast(error.message, "bad"); }
        }
      });
    },
  };
}

function filter(vods, value) {
  const q = String(value || "").trim().toLowerCase();
  return !q ? vods : vods.filter((item) => `${item.title} ${item.id}`.toLowerCase().includes(q));
}

function cards(items, selected) {
  return `<div class="vod-grid">${items.map((vod) => {
    const stamp = Number(String(vod.id).split("@").pop());
    return `<article class="vod-card ${selected.has(vod.id) ? "selected" : ""}">
      <label class="vod-check"><input type="checkbox" data-vod-select="${esc(vod.id)}" ${selected.has(vod.id) ? "checked" : ""} aria-label="选择 ${esc(vod.title)}"></label>
      <div class="vod-thumb"><span class="vod-play">${iconSvg("play")}</span><span class="vod-duration">${vod.duration_ms ? duration(vod.duration_ms) : "生成中"}</span></div>
      <div class="vod-title"><h3>${esc(vod.title)}</h3><div class="cell-sub mono">${esc(vod.id)}</div></div>
      <div class="vod-meta"><span class="badge codec">HLS</span>${stamp > 1e12 ? `<span class="cell-sub">${esc(dateTime(stamp / 1000))}</span>` : ""}</div>
      <div class="vod-actions"><button class="btn small primary" data-action="play" data-id="${esc(vod.id)}">播放</button>
        <button class="btn small" data-action="copy" data-url="${esc(vod.playlist)}">复制地址</button>
        <button class="btn small ghost" data-action="refresh-vod" data-id="${esc(vod.id)}">刷新缓存</button></div>
    </article>`;
  }).join("")}</div>`;
}
