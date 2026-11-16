import { api } from "../api.js";
import { audioLabel, bytes, empty, esc, fps, iconSvg, num, resolution, statusBadge } from "../format.js";
import { confirmDialog, openDrawer, toast } from "../ui.js";
import { playUrl } from "../player.js";

let query = "";
let status = "all";

export async function render() {
  const payload = await api("/streams");
  const streams = payload.items || payload;
  return {
    title: "直播流",
    subtitle: "实时发布状态、播放与流控制。",
    actions: `<button class="btn small" data-live-refresh>刷新</button>`,
    html: `<div class="toolbar">
      <div class="search">${iconSvg("search")}<input class="input" data-live-search value="${esc(query)}" placeholder="搜索名称、路径或编码"></div>
      <div class="seg"><button data-live-status="all">全部</button><button data-live-status="live">直播中</button><button data-live-status="idle">待机</button></div>
      <span class="meta" data-live-count></span>
      <button class="btn small danger" data-bulk-kick disabled>断开所选</button></div>
      <section class="panel" data-live-panel></section>`,
    mount(root, ctx) {
      const panel = root.querySelector("[data-live-panel]");
      const count = root.querySelector("[data-live-count]");
      const bulk = root.querySelector("[data-bulk-kick]");
      const search = root.querySelector("[data-live-search]");
      const selected = new Set();
      let visible = [];
      const syncBulk = () => {
        bulk.disabled = selected.size === 0;
        bulk.textContent = selected.size ? `断开所选 (${selected.size})` : "断开所选";
      };
      const syncStatus = () => root.querySelectorAll("[data-live-status]").forEach((button) => button.classList.toggle("active", button.dataset.liveStatus === status));
      const paint = () => {
        const q = query.trim().toLowerCase();
        visible = streams.filter((stream) => {
          const match = !q || stream.path.toLowerCase().includes(q) || stream.name.toLowerCase().includes(q) || (stream.video?.codec || "").toLowerCase().includes(q);
          const state = status === "all" || (status === "live" ? stream.publisher : !stream.publisher);
          return match && state;
        });
        count.textContent = `${visible.length} 条`;
        panel.innerHTML = visible.length ? table(visible, selected) : empty("没有匹配的流", "调整筛选条件或等待推流");
        const all = panel.querySelector("[data-select-all]");
        if (all) {
          const picked = visible.filter((stream) => selected.has(stream.path)).length;
          all.checked = visible.length > 0 && picked === visible.length;
          all.indeterminate = picked > 0 && picked < visible.length;
        }
        syncStatus();
      };
      search.oninput = () => { query = search.value; paint(); };
      root.querySelectorAll("[data-live-status]").forEach((button) => button.onclick = () => { status = button.dataset.liveStatus; paint(); });
      root.addEventListener("change", (event) => {
        const rowSelect = event.target.closest("[data-select]");
        if (rowSelect) {
          const path = rowSelect.dataset.select;
          if (rowSelect.checked) selected.add(path); else selected.delete(path);
          rowSelect.closest("tr").classList.toggle("selected", rowSelect.checked);
          const all = panel.querySelector("[data-select-all]");
          if (all) {
            const picked = visible.filter((stream) => selected.has(stream.path)).length;
            all.checked = picked === visible.length && visible.length > 0;
            all.indeterminate = picked > 0 && picked < visible.length;
          }
          syncBulk();
          return;
        }
        const all = event.target.closest("[data-select-all]");
        if (all) {
          for (const stream of visible) {
            if (all.checked) selected.add(stream.path); else selected.delete(stream.path);
          }
          paint();
          syncBulk();
        }
      });
      bulk.onclick = async () => {
        if (!selected.size) return;
        if (!await confirmDialog({ title: "批量断开", message: `断开所选 ${selected.size} 条流并关闭全部订阅？`, confirmLabel: "全部断开", danger: true })) return;
        let failed = 0;
        for (const path of [...selected]) {
          try { await api("/streams?path=" + encodeURIComponent(path), { method: "DELETE" }); }
          catch (_) { failed += 1; }
        }
        const total = selected.size;
        selected.clear();
        toast(failed ? `已断开 ${total - failed} 条，${failed} 条失败` : `已断开 ${total} 条流`, failed ? "bad" : "ok");
        ctx.reload();
      };
      root.querySelector("[data-live-refresh]").onclick = () => ctx.reload();
      root.addEventListener("click", async (event) => {
        const target = event.target.closest("[data-action]");
        if (!target) return;
        const action = target.dataset.action;
        const stream = visible.find((item) => item.path === target.dataset.path);
        if (!stream) return;
        if (action === "detail") openDetail(stream, ctx);
        else if (action === "play" || action === "play-ll") {
          playUrl(action === "play" ? stream.hls_url : stream.ll_hls_url, `${stream.name} · ${action === "play" ? "HLS" : "LL-HLS"}`);
        } else if (action === "copy") {
          try { await navigator.clipboard.writeText(target.dataset.url); toast("地址已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
        } else if (action === "kick") {
          if (!await confirmDialog({ title: "断开流", message: `断开 ${target.dataset.path} 并关闭全部订阅？`, confirmLabel: "断开", danger: true })) return;
          try { await api("/streams?path=" + encodeURIComponent(target.dataset.path), { method: "DELETE" }); selected.delete(target.dataset.path); toast("流已断开", "ok"); ctx.reload(); }
          catch (error) { toast(error.message, "bad"); }
        }
      });
      paint();
    },
  };
}

function table(items, selected) {
  return `<div class="table-wrap"><table class="data"><thead><tr><th class="select-col"><input type="checkbox" data-select-all aria-label="选择全部"></th><th>流</th><th>状态</th><th>视频</th><th>音频</th><th>订阅</th><th>接收 / 丢弃</th><th></th></tr></thead><tbody>
    ${items.map((stream) => `<tr class="${selected.has(stream.path) ? "selected" : ""}">
      <td class="select-col"><input type="checkbox" data-select="${esc(stream.path)}" ${selected.has(stream.path) ? "checked" : ""} aria-label="选择 ${esc(stream.name)}"></td>
      <td><button class="link-cell" data-action="detail" data-path="${esc(stream.path)}">${esc(stream.name)}</button><div class="cell-sub mono">${esc(stream.path)}</div></td>
      <td>${statusBadge(stream)}</td>
      <td><span class="badge codec">${esc(stream.video?.codec || "—")}</span><div class="cell-sub">${esc(resolution(stream.video))} ${esc(fps(stream.video))}</div></td>
      <td class="faint">${esc(audioLabel(stream.audio))}</td>
      <td class="mono">${num(stream.subscribers)}</td>
      <td class="mono"><span style="color:var(--ok)">${num(stream.accepted_frames)}</span> / <span style="color:var(--bad)">${num(stream.dropped_frames)}</span></td>
      <td><div class="row-actions"><button class="btn small ghost" data-action="detail" data-path="${esc(stream.path)}">详情</button>
        <button class="btn small" data-action="play" data-path="${esc(stream.path)}">HLS</button>
        <button class="btn small ghost" data-action="play-ll" data-path="${esc(stream.path)}">LL</button>
        <button class="btn small danger" data-action="kick" data-path="${esc(stream.path)}">断开</button></div></td>
    </tr>`).join("")}</tbody></table></div>`;
}

function detail(stream) {
  const endpoint = (label, url) => `<div class="endpoint"><span class="endpoint-label">${esc(label)}</span><code>${esc(url)}</code><button class="btn small ghost" data-copy="${esc(url)}">复制</button></div>`;
  return `<div class="stack">
    <div class="detail-grid">
      <div class="detail-item"><span>状态</span><strong>${stream.publisher ? "发布中" : "待机"}</strong></div>
      <div class="detail-item"><span>路径</span><strong class="mono">${esc(stream.path)}</strong></div>
      <div class="detail-item"><span>协议</span><strong>${esc((stream.protocols || []).join(" · ") || "—")}</strong></div>
      <div class="detail-item"><span>订阅连接</span><strong class="mono">${num(stream.subscribers)}</strong></div>
    </div>
    <section class="drawer-section"><h3>媒体轨道</h3><div class="detail-grid">
      <div class="detail-item"><span>视频</span><strong>${esc(stream.video?.codec || "—")} · ${esc(resolution(stream.video))}</strong><small>${esc(fps(stream.video))}</small></div>
      <div class="detail-item"><span>音频</span><strong>${esc(audioLabel(stream.audio))}</strong><small>${stream.audio?.sample_rate ? `${num(stream.audio.sample_rate)} Hz` : "—"}</small></div>
      <div class="detail-item"><span>接收帧</span><strong class="mono">${num(stream.accepted_frames)}</strong></div>
      <div class="detail-item"><span>丢弃帧</span><strong class="mono" style="color:${stream.dropped_frames ? "var(--bad)" : "inherit"}">${num(stream.dropped_frames)}</strong></div>
      <div class="detail-item"><span>RTP 包</span><strong class="mono">${num(stream.rtp_packets)}</strong></div>
      <div class="detail-item"><span>RTP 流量</span><strong class="mono">${bytes(stream.rtp_octets)}</strong></div>
    </div></section>
    <section class="drawer-section"><h3>播放地址</h3>${endpoint("HLS", stream.hls_url)}${endpoint("LL-HLS", stream.ll_hls_url)}${endpoint("RTMP", stream.rtmp_url)}</section>
    <div class="actions"><button class="btn small" data-detail-play="hls">预览 HLS</button><button class="btn small" data-detail-play="ll">预览 LL-HLS</button><button class="btn small danger" data-detail-kick>断开此流</button></div>
  </div>`;
}

function openDetail(stream, ctx) {
  openDrawer({
    title: stream.name,
    description: stream.path,
    body: detail(stream),
    showSubmit: false,
    onMount(root) {
      root.querySelectorAll("[data-copy]").forEach((button) => button.onclick = async () => {
        try { await navigator.clipboard.writeText(button.dataset.copy); toast("地址已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
      });
      root.querySelector("[data-detail-play=hls]").onclick = () => playUrl(stream.hls_url, `${stream.name} · HLS`);
      root.querySelector("[data-detail-play=ll]").onclick = () => playUrl(stream.ll_hls_url, `${stream.name} · LL-HLS`);
      root.querySelector("[data-detail-kick]").onclick = async () => {
        if (!await confirmDialog({ title: "断开流", message: `断开 ${stream.path} 并关闭全部订阅？`, confirmLabel: "断开", danger: true })) return;
        try {
          await api("/streams?path=" + encodeURIComponent(stream.path), { method: "DELETE" });
          toast("流已断开", "ok");
          root.querySelector("[data-close-drawer]")?.click();
          ctx.reload();
        } catch (error) { toast(error.message, "bad"); }
      };
    },
  });
}
