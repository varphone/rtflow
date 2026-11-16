import { api } from "../api.js";
import { empty, esc, iconSvg } from "../format.js";
import { toast } from "../ui.js";
import { playUrl } from "../player.js";

let query = "";
export async function render() {
  const payload = await api("/vods");
  const vods = payload.items || payload;
  const items = vods.filter((item) => !query || item.title.toLowerCase().includes(query.toLowerCase()) || item.id.toLowerCase().includes(query.toLowerCase()));
  return {
    title: "点播",
    subtitle: "录制目录与文件 VOD 资源。",
    actions: `<button class="btn small" data-vod-refresh>刷新</button>`,
    html: `<div class="toolbar"><div class="search"><input class="input" data-vod-search value="${esc(query)}" placeholder="搜索点播名称或 ID"></div><span class="meta">${items.length} 条</span></div>
      ${items.length ? `<div class="vod-grid">${items.map((vod) => `<article class="vod-card">
        <div class="vod-thumb">${iconSvg("play")}</div><div><h3>${esc(vod.title)}</h3><div class="cell-sub mono">${esc(vod.id)}</div></div>
        <div class="vod-actions"><button class="btn small primary" data-action="play" data-id="${esc(vod.id)}">播放</button>
          <button class="btn small" data-action="copy" data-url="${esc(vod.playlist)}">复制地址</button>
          <button class="btn small ghost" data-action="refresh-vod" data-id="${esc(vod.id)}">刷新缓存</button></div>
      </article>`).join("")}</div>` : empty("暂无可用的点播资源", "在服务端配置录制目录或文件 VOD 后显示")}`,
    mount(root, ctx) {
      const search = root.querySelector("[data-vod-search]");
      search.oninput = () => { query = search.value; ctx.reload({ keepFocus: true }); };
      root.querySelector("[data-vod-refresh]").onclick = () => ctx.reload();
      root.addEventListener("click", async (event) => {
        const target = event.target.closest("[data-action]");
        if (!target) return;
        if (target.dataset.action === "play") {
          const vod = items.find((item) => item.id === target.dataset.id);
          if (vod) playUrl(vod.playlist, vod.title, { vod: true });
        } else if (target.dataset.action === "copy") {
          try { await navigator.clipboard.writeText(target.dataset.url); toast("地址已复制", "ok"); } catch (_) {}
        } else if (target.dataset.action === "refresh-vod") {
          try { await api(`/vods/${encodeURIComponent(target.dataset.id)}/refresh`, { method: "POST" }); toast("缓存已清除", "ok"); ctx.reload(); }
          catch (error) { toast(error.message, "bad"); }
        }
      });
    },
  };
}
