import { api } from "../api.js";
import { bytes, dateTime, duration, empty, esc, exportCsv, iconSvg } from "../format.js";
import { openDrawer } from "../ui.js";
import { confirmDialog, toast } from "../ui.js";
import { playUrl } from "../player.js";

let query = "";
let sort = "recent";
let cacheFilter = "all";

export async function render(params) {
  query = params?.get("q") || "";
  const linkedSort = params?.get("sort");
  sort = ["recent", "duration", "cached"].includes(linkedSort) ? linkedSort : "recent";
  const linkedCache = params?.get("cache");
  cacheFilter = ["cached", "uncached"].includes(linkedCache) ? linkedCache : "all";
  const payload = await api("/vods");
  const vods = payload.items || payload;
  const initial = filter(vods, query);
  return {
    title: "点播",
    subtitle: "录制目录与文件 VOD 资源。",
    actions: `<button class="btn small ghost" data-vod-export>导出</button><button class="btn small" data-vod-refresh>刷新</button>`,
    html: `<div class="toolbar"><label class="checkline"><input type="checkbox" data-vod-select-all> 全选</label>
      <div class="search">${iconSvg("search")}<input class="input" type="search" data-vod-search value="${esc(query)}" placeholder="搜索点播名称或 ID"></div>
      <select class="input" data-vod-sort aria-label="点播排序" style="width:auto"><option value="recent" ${sort === "recent" ? "selected" : ""}>最新录制</option><option value="duration" ${sort === "duration" ? "selected" : ""}>按时长</option><option value="cached" ${sort === "cached" ? "selected" : ""}>已缓存优先</option></select>
      <div class="seg"><button data-vod-cache-filter="all" class="${cacheFilter === "all" ? "active" : ""}">全部</button><button data-vod-cache-filter="cached" class="${cacheFilter === "cached" ? "active" : ""}">已缓存</button><button data-vod-cache-filter="uncached" class="${cacheFilter === "uncached" ? "active" : ""}">未缓存</button></div>
      <span class="meta" data-vod-count>${initial.length} 条 · ${initial.filter((vod) => vod.cached).length} 已缓存</span>
      <button class="btn small ghost" data-vod-reset ${query.trim() || cacheFilter !== "all" ? "" : "hidden"}>重置</button>
      <button class="btn small ghost" data-vod-copy-bulk disabled>复制所选地址</button>
      <button class="btn small ghost" data-vod-bulk disabled>刷新所选缓存</button></div>
      <div data-vod-results>${initial.length ? cards(initial, new Set()) : empty("暂无可用的点播资源", "完成一次录制或配置文件 VOD 后显示", "vod", `<a class="btn small" href="#/config/recordings">查看录制任务</a>`)}</div>`,
    mount(root, ctx) {
      const search = root.querySelector("[data-vod-search]");
      const reset = root.querySelector("[data-vod-reset]");
      const results = root.querySelector("[data-vod-results]");
      const count = root.querySelector("[data-vod-count]");
      const selectAll = root.querySelector("[data-vod-select-all]");
      const sorter = root.querySelector("[data-vod-sort]");
      const bulk = root.querySelector("[data-vod-bulk]");
      const copyBulk = root.querySelector("[data-vod-copy-bulk]");
      const selected = new Set();
      let allVods = vods;
      let visible = initial;
      const sync = () => {
        bulk.disabled = selected.size === 0;
        bulk.textContent = selected.size ? `刷新所选缓存 (${selected.size})` : "刷新所选缓存";
        copyBulk.disabled = selected.size === 0;
        copyBulk.textContent = selected.size ? `复制所选地址 (${selected.size})` : "复制所选地址";
        const picked = visible.filter((item) => selected.has(item.id)).length;
        selectAll.checked = visible.length > 0 && picked === visible.length;
        selectAll.indeterminate = picked > 0 && picked < visible.length;
      };
      const paint = () => {
        visible = filter(allVods, search.value);
        count.textContent = `${visible.length} 条 · ${visible.filter((item) => item.cached).length} 已缓存`;
        reset.hidden = !(query.trim() || cacheFilter !== "all");
        results.innerHTML = visible.length ? cards(visible, selected) : empty("没有匹配的点播", "调整搜索条件", "vod");
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
      copyBulk.onclick = async () => {
        if (!selected.size) return;
        const urls = allVods.filter((item) => selected.has(item.id)).map((item) => location.origin + item.playlist);
        try { await navigator.clipboard.writeText(urls.join("\n")); toast(`已复制 ${urls.length} 个地址`, "ok"); }
        catch (_) { toast("复制失败", "bad"); }
      };
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
      const syncUrl = () => {
        const params = new URLSearchParams();
        if (query.trim()) params.set("q", query.trim());
        if (sort !== "recent") params.set("sort", sort);
        if (cacheFilter !== "all") params.set("cache", cacheFilter);
        const qs = params.toString();
        history.replaceState(null, "", `#/vod${qs ? `?${qs}` : ""}`);
      };
      search.oninput = () => { query = search.value; paint(); syncUrl(); };
      reset.onclick = () => {
        query = ""; cacheFilter = "all"; search.value = "";
        root.querySelectorAll("[data-vod-cache-filter]").forEach((item) => item.classList.toggle("active", item.dataset.vodCacheFilter === "all"));
        paint(); syncUrl();
      };
      sorter.onchange = () => { sort = sorter.value; paint(); syncUrl(); };
      root.querySelectorAll("[data-vod-cache-filter]").forEach((button) => button.onclick = () => {
        cacheFilter = button.dataset.vodCacheFilter;
        root.querySelectorAll("[data-vod-cache-filter]").forEach((item) => item.classList.toggle("active", item === button));
        paint(); syncUrl();
      });
      root.querySelector("[data-vod-export]").onclick = () => {
        exportCsv(`vod-${new Date().toISOString().slice(0, 10)}.csv`,
          ["id", "title", "duration_ms", "cached", "cache_bytes", "playlist"],
          visible.map((item) => [item.id, item.title, item.duration_ms, item.cached, item.cache_bytes || 0, item.playlist]));
        toast(`已导出 ${visible.length} 条点播`, "ok");
      };
      root.querySelector("[data-vod-refresh]").onclick = () => ctx.reload();
      root.addEventListener("click", async (event) => {
        const target = event.target.closest("[data-action]");
        if (!target) return;
        const vod = visible.find((item) => item.id === target.dataset.id);
        if (target.dataset.action === "detail" && vod) openVodDetail(vod, ctx);
        else if (target.dataset.action === "play" && vod) playUrl(vod.playlist, vod.title, { vod: true });
        else if (target.dataset.action === "copy") {
          try { await navigator.clipboard.writeText(target.dataset.url); toast("地址已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
        } else if (target.dataset.action === "refresh-vod" && vod) {
          try { await api(`/vods/${encodeURIComponent(vod.id)}/refresh`, { method: "POST" }); toast("缓存已清除，下次播放重新生成", "ok"); ctx.reload(); }
          catch (error) { toast(error.message, "bad"); }
        }
      });
      const cacheTimer = setInterval(async () => {
        if (document.hidden) return;
        let next;
        try { next = (await api("/vods")).items || []; } catch (_) { return; }
        const before = allVods.map((item) => item.id).join("|");
        const after = next.map((item) => item.id).join("|");
        allVods = next;
        if (before !== after) { paint(); return; }
        for (const item of next) {
          const card = results.querySelector(`[data-vod-card="${CSS.escape(item.id)}"]`);
          const badge = card?.querySelector("[data-vod-cache]");
          if (!badge) continue;
          badge.textContent = item.cached ? "已缓存" : "未缓存";
          badge.classList.toggle("ok", item.cached);
        }
      }, 10000);
      return () => clearInterval(cacheTimer);
    },
  };
}

function filter(vods, value) {
  const q = String(value || "").trim().toLowerCase();
  const items = vods.filter((item) => {
    const match = !q || `${item.title} ${item.id}`.toLowerCase().includes(q);
    const cache = cacheFilter === "all" || (cacheFilter === "cached" ? item.cached : !item.cached);
    return match && cache;
  });
  const stamp = (item) => Number(String(item.id).split("@").pop()) || 0;
  if (sort === "duration") items.sort((a, b) => Number(b.duration_ms || 0) - Number(a.duration_ms || 0));
  else if (sort === "cached") items.sort((a, b) => Number(Boolean(b.cached)) - Number(Boolean(a.cached)) || stamp(b) - stamp(a));
  else items.sort((a, b) => stamp(b) - stamp(a));
  return items;
}

function cards(items, selected) {
  return `<div class="vod-grid">${items.map((vod) => {
    const stamp = Number(String(vod.id).split("@").pop());
    return `<article class="vod-card ${selected.has(vod.id) ? "selected" : ""}" data-vod-card="${esc(vod.id)}">
      <label class="vod-check"><input type="checkbox" data-vod-select="${esc(vod.id)}" ${selected.has(vod.id) ? "checked" : ""} aria-label="选择 ${esc(vod.title)}"></label>
      <div class="vod-thumb" style="--hue:${String(vod.id).split("").reduce((hash, char) => (hash * 31 + char.charCodeAt(0)) % 360, 200)}"><span class="vod-play">${iconSvg("play")}</span><span class="vod-duration">${vod.duration_ms ? duration(vod.duration_ms) : "生成中"}</span></div>
      <div class="vod-title"><h3><button class="link-cell" data-action="detail" data-id="${esc(vod.id)}">${esc(vod.title)}</button></h3><div class="cell-sub mono">${esc(vod.id)}</div></div>
      <div class="vod-meta"><span class="badge codec">HLS</span><span class="badge ${vod.cached ? "ok" : ""}" data-vod-cache>${vod.cached ? "已缓存" : "未缓存"}</span>${stamp > 1e12 ? `<span class="cell-sub">${esc(dateTime(stamp / 1000))}</span>` : ""}</div>
      <div class="vod-actions"><button class="btn small primary" data-action="play" data-id="${esc(vod.id)}">播放</button>
        <button class="btn small ghost" data-action="detail" data-id="${esc(vod.id)}">详情</button>
        <button class="btn small" data-action="copy" data-url="${esc(location.origin + vod.playlist)}">复制地址</button>
        <button class="btn small ghost" data-action="refresh-vod" data-id="${esc(vod.id)}">刷新缓存</button></div>
    </article>`;
  }).join("")}</div>`;
}


function openVodDetail(vod, ctx) {
  const stamp = Number(String(vod.id).split("@").pop());
  const recorded = stamp > 1e12 ? dateTime(stamp / 1000) : "—";
  openDrawer({
    title: vod.title,
    description: "HLS 点播资源详情。",
    showSubmit: false,
    body: `<div class="stack">
      <div class="detail-grid">
        <div class="detail-item"><span>时长</span><strong class="mono">${vod.duration_ms ? duration(vod.duration_ms) : "生成中"}</strong></div>
        <div class="detail-item"><span>录制时间</span><strong>${esc(recorded)}</strong></div>
        <div class="detail-item"><span>格式</span><strong>HLS · MPEG-TS</strong></div>
        <div class="detail-item"><span>缓存</span><strong>${vod.cached ? `已生成 · ${bytes(vod.cache_bytes)}` : "未生成"}</strong></div>
        <div class="detail-item"><span>资源 ID</span><strong class="mono">${esc(vod.id)}</strong></div>
      </div>
      <section class="drawer-section"><h3>播放地址</h3><div class="endpoint"><span class="endpoint-label">HLS</span><code>${esc(location.origin + vod.playlist)}</code><button class="btn small ghost" data-vod-copy>复制</button></div></section>
      <div class="actions"><button class="btn small primary" data-vod-play>播放</button><button class="btn small ghost" data-vod-refresh>刷新生成缓存</button><button class="btn small ghost" data-vod-recording>查看录制</button></div>
    </div>`,
    onMount(root) {
      const close = () => root.querySelector("[data-close-drawer]")?.click();
      root.querySelector("[data-vod-copy]").onclick = async () => {
        try { await navigator.clipboard.writeText(location.origin + vod.playlist); toast("地址已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
      };
      root.querySelector("[data-vod-play]").onclick = () => { close(); playUrl(vod.playlist, vod.title, { vod: true }); };
      root.querySelector("[data-vod-refresh]").onclick = async () => {
        try { await api(`/vods/${encodeURIComponent(vod.id)}/refresh`, { method: "POST" }); toast("缓存已清除，下次播放重新生成", "ok"); close(); ctx.reload(); }
        catch (error) { toast(error.message, "bad"); }
      };
      root.querySelector("[data-vod-recording]").onclick = () => {
        const path = String(vod.title).split("/").slice(0, -1).join("/");
        close();
        location.hash = `#/config/recordings?q=${encodeURIComponent(path)}`;
      };
    },
  });
}
