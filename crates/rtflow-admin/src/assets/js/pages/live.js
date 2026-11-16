import { api } from "../api.js";
import { audioLabel, bytes, empty, esc, fps, iconSvg, num, resolution, statusBadge } from "../format.js";
import { confirmDialog, toast } from "../ui.js";
import { playUrl } from "../player.js";

let query = "";
let status = "all";

export async function render() {
  const payload = await api("/streams");
  const streams = payload.items || payload;
  const items = streams.filter((stream) => {
    const q = query.trim().toLowerCase();
    const match = !q || stream.path.toLowerCase().includes(q) || stream.name.toLowerCase().includes(q) || (stream.video?.codec || "").toLowerCase().includes(q);
    const state = status === "all" || (status === "live" ? stream.publisher : !stream.publisher);
    return match && state;
  });
  return {
    title: "直播流",
    subtitle: "实时发布状态、播放与流控制。",
    actions: `<button class="btn small" data-live-refresh>刷新</button>`,
    html: `<div class="toolbar">
      <div class="search">${iconSvg("search")}<input class="input" data-live-search value="${esc(query)}" placeholder="搜索名称、路径或编码"></div>
      <div class="seg"><button data-live-status="all" class="${status === "all" ? "active" : ""}">全部</button><button data-live-status="live" class="${status === "live" ? "active" : ""}">直播中</button><button data-live-status="idle" class="${status === "idle" ? "active" : ""}">待机</button></div>
      <span class="meta">${items.length} 条</span></div>
      <section class="panel">${items.length ? table(items) : empty("没有匹配的流", "调整筛选条件或等待推流")}</section>`,
    mount(root, ctx) {
      const search = root.querySelector("[data-live-search]");
      search.oninput = () => { query = search.value; ctx.reload({ keepFocus: true }); };
      root.querySelectorAll("[data-live-status]").forEach((button) => button.onclick = () => { status = button.dataset.liveStatus; ctx.reload(); });
      root.querySelector("[data-live-refresh]").onclick = () => ctx.reload();
      root.addEventListener("click", async (event) => {
        const target = event.target.closest("[data-action]");
        if (!target) return;
        const action = target.dataset.action;
        if (action === "play" || action === "play-ll") {
          const stream = items.find((item) => item.path === target.dataset.path);
          if (stream) playUrl(action === "play" ? stream.hls_url : stream.ll_hls_url, `${stream.name} · ${action === "play" ? "HLS" : "LL-HLS"}`);
        } else if (action === "copy") {
          try { await navigator.clipboard.writeText(target.dataset.url); toast("地址已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
        } else if (action === "kick") {
          if (!await confirmDialog({ title: "断开流", message: `断开 ${target.dataset.path} 并关闭全部订阅？`, confirmLabel: "断开", danger: true })) return;
          try { await api("/streams?path=" + encodeURIComponent(target.dataset.path), { method: "DELETE" }); toast("流已断开", "ok"); ctx.reload(); }
          catch (error) { toast(error.message, "bad"); }
        }
      });
    },
  };
}

function table(items) {
  return `<div class="table-wrap"><table class="data"><thead><tr><th>流</th><th>状态</th><th>视频</th><th>音频</th><th>订阅</th><th>接收 / 丢弃</th><th></th></tr></thead><tbody>
    ${items.map((stream) => `<tr>
      <td><div class="cell-main">${esc(stream.name)}</div><div class="cell-sub mono">${esc(stream.path)}</div></td>
      <td>${statusBadge(stream)}</td>
      <td><span class="badge codec">${esc(stream.video?.codec || "—")}</span><div class="cell-sub">${esc(resolution(stream.video))} ${esc(fps(stream.video))}</div></td>
      <td class="faint">${esc(audioLabel(stream.audio))}</td>
      <td class="mono">${num(stream.subscribers)}</td>
      <td class="mono"><span style="color:var(--ok)">${num(stream.accepted_frames)}</span> / <span style="color:var(--bad)">${num(stream.dropped_frames)}</span></td>
      <td><div class="row-actions"><button class="btn small" data-action="play" data-path="${esc(stream.path)}">HLS</button>
        <button class="btn small ghost" data-action="play-ll" data-path="${esc(stream.path)}">LL</button>
        <button class="btn small ghost" data-action="copy" data-url="${esc(stream.hls_url)}">复制</button>
        <button class="btn small danger" data-action="kick" data-path="${esc(stream.path)}">断开</button></div></td>
    </tr>`).join("")}</tbody></table></div>`;
}
