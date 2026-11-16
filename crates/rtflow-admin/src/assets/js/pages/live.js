import { api } from "../api.js";
import { audioLabel, bytes, empty, esc, fps, iconSvg, num, resolution, statusBadge } from "../format.js";
import { confirmDialog, openDrawer, syncSortAria, toast } from "../ui.js";
import { playUrl } from "../player.js";

let query = "";
let status = "all";
let sort = { field: "name", dir: 1 };

export async function render(params) {
  query = params?.get("q") || "";
  const linkedStatus = params?.get("status");
  status = ["all", "live", "idle"].includes(linkedStatus) ? linkedStatus : "all";
  const linkedSort = params?.get("sort");
  if (["name", "status", "subscribers", "frames"].includes(linkedSort)) {
    sort = { field: linkedSort, dir: params?.get("dir") === "-1" ? -1 : 1 };
  } else {
    sort = { field: "name", dir: 1 };
  }
  const payload = await api("/streams");
  const streams = payload.items || payload;
  return {
    title: "直播流",
    subtitle: "实时发布状态、播放与流控制。",
    actions: `<button class="btn small ghost active" data-live-auto>自动刷新：开</button><button class="btn small" data-live-refresh>刷新</button>`,
    html: `<div class="toolbar">
      <div class="search">${iconSvg("search")}<input class="input" data-live-search value="${esc(query)}" placeholder="搜索名称、路径或编码"></div>
      <div class="seg"><button data-live-status="all">全部</button><button data-live-status="live">直播中</button><button data-live-status="idle">待机</button></div>
      <span class="meta" data-live-count></span><span class="meta" data-live-updated></span>
      <button class="btn small danger" data-bulk-kick disabled>断开所选</button></div>
      <section class="panel" data-live-panel></section>`,
    mount(root, ctx) {
      const panel = root.querySelector("[data-live-panel]");
      const count = root.querySelector("[data-live-count]");
      const bulk = root.querySelector("[data-bulk-kick]");
      const search = root.querySelector("[data-live-search]");
      const selected = new Set();
      let liveStreams = streams;
      let visible = [];
      const syncBulk = () => {
        bulk.disabled = selected.size === 0;
        bulk.textContent = selected.size ? `断开所选 (${selected.size})` : "断开所选";
      };
      const syncStatus = () => root.querySelectorAll("[data-live-status]").forEach((button) => button.classList.toggle("active", button.dataset.liveStatus === status));
      const view = () => {
        const q = query.trim().toLowerCase();
        const items = liveStreams.filter((stream) => {
          const match = !q || stream.path.toLowerCase().includes(q) || stream.name.toLowerCase().includes(q) || (stream.video?.codec || "").toLowerCase().includes(q);
          const state = status === "all" || (status === "live" ? stream.publisher : !stream.publisher);
          return match && state;
        });
        const value = (stream) => {
          if (sort.field === "status") return stream.publisher ? 0 : 1;
          if (sort.field === "subscribers") return Number(stream.subscribers || 0);
          if (sort.field === "frames") return Number(stream.accepted_frames || 0);
          return stream.name.toLowerCase();
        };
        return items.sort((left, right) => {
          const a = value(left), b = value(right);
          return (a < b ? -1 : a > b ? 1 : 0) * sort.dir;
        });
      };
      const markUpdated = () => { const node = root.querySelector("[data-live-updated]"); if (node) node.textContent = `更新于 ${new Date().toLocaleTimeString("zh-CN", { hour12: false })}`; };
      const paint = () => {
        visible = view();
        markUpdated();
        count.textContent = `${visible.length} 条 · ${visible.filter((stream) => stream.publisher).length} 直播中`;
        panel.innerHTML = visible.length ? table(visible, selected) : empty("没有匹配的流", "调整筛选条件或等待推流");
        syncSortAria(panel, sort);
        const all = panel.querySelector("[data-select-all]");
        if (all) {
          const picked = visible.filter((stream) => selected.has(stream.path)).length;
          all.checked = visible.length > 0 && picked === visible.length;
          all.indeterminate = picked > 0 && picked < visible.length;
        }
        syncStatus();
      };
      const syncUrl = () => {
        const params = new URLSearchParams();
        if (query.trim()) params.set("q", query.trim());
        if (status !== "all") params.set("status", status);
        if (sort.field !== "name" || sort.dir !== 1) params.set("sort", sort.field);
        if (sort.dir === -1) params.set("dir", "-1");
        const qs = params.toString();
        history.replaceState(null, "", `#/live${qs ? `?${qs}` : ""}`);
      };
      search.oninput = () => { query = search.value; paint(); syncUrl(); };
      root.querySelectorAll("[data-live-status]").forEach((button) => button.onclick = () => { status = button.dataset.liveStatus; paint(); syncUrl(); });
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
      const refresh = async () => {
        let next;
        try { next = (await api("/streams")).items || []; } catch (_) { return; }
        liveStreams = next;
        const filtered = view();
        const existing = [...panel.querySelectorAll("tr[data-stream]")].map((row) => row.dataset.stream);
        if (existing.length !== filtered.length || existing.some((path, index) => path !== filtered[index].path)) {
          visible = filtered;
          paint();
          return;
        }
        for (const stream of filtered) {
          const row = panel.querySelector(`tr[data-stream="${CSS.escape(stream.path)}"]`);
          if (!row) continue;
          row.querySelector('[data-cell="status"]').innerHTML = statusBadge(stream);
          row.querySelector('[data-cell="video"]').innerHTML = `<span class="badge codec">${esc(stream.video?.codec || "—")}</span><div class="cell-sub">${esc(resolution(stream.video))} ${esc(fps(stream.video))}</div>`;
          row.querySelector('[data-cell="audio"]').textContent = audioLabel(stream.audio);
          row.querySelector('[data-cell="subscribers"]').textContent = num(stream.subscribers);
          row.querySelector('[data-cell="metrics"]').innerHTML = `<span style="color:var(--ok)">${num(stream.accepted_frames)}</span> / <span style="color:var(--bad)">${num(stream.dropped_frames)}</span>`;
        }
        visible = filtered;
        markUpdated();
      };
      const setAuto = (enabled) => {
        automatic = enabled;
        const button = root.querySelector("[data-live-auto]");
        button.textContent = enabled ? "自动刷新：开" : "自动刷新：关";
        button.classList.toggle("active", enabled);
        if (timer) { clearInterval(timer); timer = null; }
        if (enabled) timer = setInterval(() => { if (!document.hidden) refresh(); }, 5000);
      };
      root.querySelector("[data-live-auto]").onclick = () => setAuto(!automatic);
      root.querySelector("[data-live-refresh]").onclick = () => refresh();
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
      setAuto(true);
      return () => { if (timer) clearInterval(timer); };
    },
  };
}

function table(items, selected) {
  const arrow = (field) => sort.field === field ? (sort.dir > 0 ? " ↑" : " ↓") : " ↕";
  return `<div class="table-wrap"><table class="data live-table"><thead><tr><th class="select-col"><input type="checkbox" data-select-all aria-label="选择全部"></th>
    <th><button class="sort-btn" data-sort="name">流${arrow("name")}</button></th>
    <th><button class="sort-btn" data-sort="status">状态${arrow("status")}</button></th><th>视频</th><th>音频</th>
    <th><button class="sort-btn" data-sort="subscribers">订阅${arrow("subscribers")}</button></th>
    <th><button class="sort-btn" data-sort="frames">接收 / 丢弃${arrow("frames")}</button></th><th></th></tr></thead><tbody>
    ${items.map((stream) => `<tr data-stream="${esc(stream.path)}" class="${selected.has(stream.path) ? "selected" : ""}">
      <td class="select-col"><input type="checkbox" data-select="${esc(stream.path)}" ${selected.has(stream.path) ? "checked" : ""} aria-label="选择 ${esc(stream.name)}"></td>
      <td><button class="link-cell" data-action="detail" data-path="${esc(stream.path)}">${esc(stream.name)}</button><div class="cell-sub mono">${esc(stream.path)}</div></td>
      <td data-cell="status">${statusBadge(stream)}</td>
      <td data-cell="video"><span class="badge codec">${esc(stream.video?.codec || "—")}</span><div class="cell-sub">${esc(resolution(stream.video))} ${esc(fps(stream.video))}</div></td>
      <td class="faint" data-cell="audio">${esc(audioLabel(stream.audio))}</td>
      <td class="mono" data-cell="subscribers">${num(stream.subscribers)}</td>
      <td class="mono" data-cell="metrics"><span style="color:var(--ok)">${num(stream.accepted_frames)}</span> / <span style="color:var(--bad)">${num(stream.dropped_frames)}</span></td>
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
      <div class="detail-item"><span>状态</span><strong data-detail="status">${stream.publisher ? "发布中" : "待机"}</strong></div>
      <div class="detail-item"><span>路径</span><strong class="mono">${esc(stream.path)}</strong></div>
      <div class="detail-item"><span>协议</span><strong>${esc((stream.protocols || []).join(" · ") || "—")}</strong></div>
      <div class="detail-item"><span>订阅连接</span><strong class="mono" data-detail="subscribers">${num(stream.subscribers)}</strong></div>
    </div>
    <section class="drawer-section"><h3>媒体轨道</h3><div class="detail-grid">
      <div class="detail-item"><span>视频</span><strong>${esc(stream.video?.codec || "—")} · ${esc(resolution(stream.video))}</strong><small>${esc(fps(stream.video))}</small></div>
      <div class="detail-item"><span>音频</span><strong>${esc(audioLabel(stream.audio))}</strong><small>${stream.audio?.sample_rate ? `${num(stream.audio.sample_rate)} Hz` : "—"}</small></div>
      <div class="detail-item"><span>接收帧</span><strong class="mono" data-detail="accepted">${num(stream.accepted_frames)}</strong></div>
      <div class="detail-item"><span>丢弃帧</span><strong class="mono" data-detail="dropped" style="color:${stream.dropped_frames ? "var(--bad)" : "inherit"}">${num(stream.dropped_frames)}</strong></div>
      <div class="detail-item"><span>RTP 包</span><strong class="mono" data-detail="packets">${num(stream.rtp_packets)}</strong></div>
      <div class="detail-item"><span>RTP 流量</span><strong class="mono" data-detail="octets">${bytes(stream.rtp_octets)}</strong></div>
    </div></section>
    <section class="drawer-section"><h3>播放地址</h3>${endpoint("HLS", stream.hls_url)}${endpoint("LL-HLS", stream.ll_hls_url)}${endpoint("RTMP", stream.rtmp_url)}</section>
    <div class="actions"><button class="btn small" data-detail-play="hls">预览 HLS</button><button class="btn small" data-detail-play="ll">预览 LL-HLS</button><button class="btn small danger" data-detail-kick>断开此流</button></div>
  </div>`;
}

function openDetail(stream, ctx) {
  let timer = null;
  openDrawer({
    title: stream.name,
    description: stream.path,
    body: detail(stream),
    showSubmit: false,
    onClose() { if (timer) clearInterval(timer); },
    onMount(root) {
      const update = async () => {
        if (document.hidden) return;
        let current;
        try {
          const payload = await api("/streams");
          current = (payload.items || payload).find((item) => item.path === stream.path);
        } catch (_) { return; }
        if (!current) {
          const status = root.querySelector('[data-detail="status"]');
          if (status) status.textContent = "已离线";
          return;
        }
        const set = (name, value) => { const node = root.querySelector(`[data-detail="${name}"]`); if (node) node.textContent = value; };
        set("status", current.publisher ? "发布中" : "待机");
        set("subscribers", num(current.subscribers));
        set("accepted", num(current.accepted_frames));
        set("dropped", num(current.dropped_frames));
        set("packets", num(current.rtp_packets));
        set("octets", bytes(current.rtp_octets));
      };
      timer = setInterval(update, 3000);
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
