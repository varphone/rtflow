import { api, apiV1 } from "../api.js";
import { bytes, dateTime, esc, iconSvg, num, uptime } from "../format.js";

const actionNames = {
  "path.create": "创建路径", "path.update": "更新路径", "path.delete": "删除路径", "path.kick": "断开路径",
  "key.issue": "签发密钥", "key.revoke": "吊销密钥", "proxy.upsert": "保存代理", "proxy.delete": "删除代理",
  "proxy.restart": "重启代理", "srt.upsert": "保存 SRT", "srt.delete": "删除 SRT", "vod.refresh": "刷新点播缓存",
  "record.start": "开始录制", "record.stop": "停止录制",
};

async function load() {
  const [overview, streamsPayload, proxiesPayload, pulls, pushes, recordings, system, audit] = await Promise.all([
    api("/overview"), api("/streams"), apiV1("/proxies"), apiV1("/srt/pulls"), apiV1("/srt/pushes"), api("/recordings"), api("/system"), api("/audit?limit=100"),
  ]);
  return {
    overview,
    streams: streamsPayload.items || streamsPayload,
    proxies: proxiesPayload.proxies || proxiesPayload || [],
    pulls: pulls.items || [],
    pushes: pushes.items || [],
    recordings,
    system,
    audit: audit.events || [],
  };
}

function derive(data) {
  const attention = [];
  for (const proxy of data.proxies) {
    if (proxy.state === "backoff") attention.push({ kind: "代理", name: proxy.path, detail: proxy.last_error || "上游不可达", level: "bad", href: `#/config/proxies?q=${encodeURIComponent(proxy.path)}` });
  }
  for (const endpoint of [...data.pulls, ...data.pushes]) {
    if (endpoint.enabled && !endpoint.connected) attention.push({ kind: "SRT", name: endpoint.name || endpoint.id, detail: endpoint.last_error || "未连接", level: "warn", href: `#/config/srt?q=${encodeURIComponent(endpoint.name || endpoint.id)}` });
  }
  for (const rec of data.recordings) {
    if (rec.last_error) attention.push({ kind: "录制", name: rec.path, detail: rec.last_error, level: "warn", href: `#/config/recordings?q=${encodeURIComponent(rec.path)}` });
  }
  const active = data.streams.filter((stream) => stream.publisher).slice(0, 6);
  const changes = data.audit.filter((event) => !String(event.action).startsWith("auth.")).slice(0, 4);
  return { attention, active, changes };
}

export async function render() {
  const data = await load();
  const d = derive(data);
  return {
    title: "概览",
    subtitle: "运行状态、异常与活跃流。",
    actions: `<span class="meta" data-ov-updated>更新于 --</span><button class="btn small ghost active" data-overview-auto>自动刷新：开</button><button class="btn small" data-overview-refresh>刷新</button>`,
    html: `
      <div class="stat-grid">
        <div class="stat"><div class="label">在线流</div><div class="value" data-ov="mounts">${num(data.overview.mounts)}</div><div class="hint"><span data-ov="publishers">${num(data.overview.publishers)}</span> 个发布中</div></div>
        <div class="stat"><div class="label">观看连接</div><div class="value" data-ov="subscribers">${num(data.overview.subscribers)}</div><div class="hint">RTP <span data-ov="rtp">${bytes(data.overview.rtp_octets)}</span></div></div>
        <div class="stat"><div class="label">代理 streaming</div><div class="value" data-ov="proxy-streaming">${num(data.proxies.filter((item) => item.state === "streaming").length)}</div><div class="hint">共 <span data-ov="proxy-total">${num(data.proxies.length)}</span> 个代理</div></div>
        <div class="stat ${d.attention.length ? "attention" : ""}"><div class="label">需要关注</div><div class="value ${d.attention.length ? "warn" : ""}" data-ov="attention-count">${num(d.attention.length)}</div><div class="hint">代理 / SRT / 录制异常</div></div>
      </div>
      <div class="quick-actions">
        <a class="quick-action" href="#/config/paths?new=1"><span class="kicker">配置</span><strong>新建路径</strong><small>注册路径与发布策略</small></a>
        <a class="quick-action" href="#/config/keys?new=1"><span class="kicker">安全</span><strong>签发密钥</strong><small>一次性 secret 与权限范围</small></a>
        <a class="quick-action" href="#/config/proxies?new=1"><span class="kicker">回源</span><strong>新增代理</strong><small>拉取 RTSP/RTSPS 上游</small></a>
        <a class="quick-action" href="#/system"><span class="kicker">诊断</span><strong>服务与存储</strong><small>TOML、DB schema、版本</small></a>
      </div>
      <section class="panel traffic-panel">
        <div class="panel-head"><h2 data-trend-title>接收帧速率</h2><div class="seg" style="margin-left:8px"><button data-trend-metric="frames" class="active">帧率</button><button data-trend-metric="subscribers">订阅</button></div><span class="meta"><span data-trend-rate>—</span> · 近 2 分钟</span><span class="meta" data-trend-peak style="margin-left:auto"></span></div>
        <div class="traffic-chart"><svg viewBox="0 0 1000 90" preserveAspectRatio="none" aria-hidden="true"><path class="traffic-area" data-trend-area></path><path class="traffic-line" data-trend-line></path></svg><span class="traffic-empty" data-trend-empty>等待流量…</span></div>
      </section>
      <div class="two-col">
        <section class="panel">
          <div class="panel-head"><h2>活跃流</h2><span class="sub" data-ov="active-count">${d.active.length} 条</span><div style="margin-left:auto"><a class="btn small ghost" href="#/live">查看全部</a></div></div>
          <div data-ov-active>${activeHtml(d.active, data.system)}</div>
        </section>
        <div class="stack">
          <section class="panel"><div class="panel-head"><h2>需要关注</h2><span class="sub" data-ov="attention-list-count">${d.attention.length} 项</span></div>
            <div class="panel-body" data-ov-attention>${attentionHtml(d.attention)}</div>
          </section>
          <section class="panel"><div class="panel-head"><h2>运行</h2></div><div class="panel-body">
            <dl class="kv"><dt>运行时长</dt><dd data-ov-runtime>${esc(uptime(data.overview.uptime_seconds))}</dd>
            <dt>点播资源</dt><dd data-ov-vods>${num(data.overview.vods)}</dd><dt>接收帧</dt><dd class="mono" data-ov-frames>${num(data.overview.accepted_frames)}</dd>
            <dt>RTP 包</dt><dd class="mono" data-ov-packets>${num(data.overview.rtp_packets)}</dd></dl>
          </div></section>
          <section class="panel"><div class="panel-head"><h2>最近变更</h2><span class="sub" data-ov="changes-count">${d.changes.length} 项</span><div style="margin-left:auto"><a class="btn small ghost" href="#/audit">审计</a></div></div>
            <div class="panel-body" data-ov-changes>${changesHtml(d.changes)}</div>
          </section>
        </div>
      </div>`,
    mount(root, ctx) {
      let current = data;
      let automatic = localStorage.getItem("rtflow-auto-overview") !== "off";
      let timer = null;
      let samples = [];
      let metric = "frames";
      let previous = { frames: Number(data.overview.accepted_frames || 0), at: Date.now() };
      const drawTrend = () => {
        const line = root.querySelector("[data-trend-line]");
        const area = root.querySelector("[data-trend-area]");
        const rateLabel = root.querySelector("[data-trend-rate]");
        const peakLabel = root.querySelector("[data-trend-peak]");
        const empty = root.querySelector("[data-trend-empty]");
        const title = root.querySelector("[data-trend-title]");
        const values = samples.map((sample) => sample[metric]);
        const suffix = metric === "frames" ? " fps" : "";
        if (rateLabel) rateLabel.textContent = `${(values[values.length - 1] || 0).toFixed(0)}${suffix}`;
        if (peakLabel) peakLabel.textContent = `峰值 ${Math.max(...values, 0).toFixed(0)}${suffix}`;
        if (title) title.textContent = metric === "frames" ? "接收帧速率" : "订阅连接";
        if (empty) {
          empty.textContent = metric === "frames" ? "等待流量…" : "等待订阅…";
          empty.hidden = values.some((value) => value > 0);
        }
        if (!line || !area || values.length < 2) { line?.setAttribute("d", ""); area?.setAttribute("d", ""); return; }
        const max = Math.max(...values, 0);
        const peak = Math.max(max * 1.25, metric === "subscribers" ? 2 : 1);
        const points = values.map((value, index) => {
          const x = (index / (values.length - 1)) * 1000;
          const y = 82 - (value / peak) * 68;
          return `${x.toFixed(1)},${y.toFixed(1)}`;
        });
        line.setAttribute("d", `M${points.join(" L")}`);
        area.setAttribute("d", `M0,90 L${points.join(" L")} L1000,90 Z`);
      };
      const pushSample = (overview) => {
        const now = Date.now();
        const frames = Number(overview.accepted_frames || 0);
        const seconds = Math.max(1, (now - previous.at) / 1000);
        const rate = Math.max(0, (frames - previous.frames) / seconds);
        previous = { frames, at: now };
        samples.push({ frames: rate, subscribers: Number(overview.subscribers || 0) });
        if (samples.length > 15) samples.shift();
        drawTrend();
      };
      root.querySelectorAll("[data-trend-metric]").forEach((button) => button.onclick = () => {
        metric = button.dataset.trendMetric;
        root.querySelectorAll("[data-trend-metric]").forEach((item) => item.classList.toggle("active", item === button));
        drawTrend();
      });
      const markUpdated = () => { const node = root.querySelector("[data-ov-updated]"); if (node) node.textContent = `更新于 ${new Date().toLocaleTimeString("zh-CN", { hour12: false })}`; };
      markUpdated();
      pushSample(data.overview);
      const refresh = async () => {
        let next;
        try { next = await load(); } catch (_) { return; }
        current = next;
        const view = derive(next);
        const set = (key, value) => { const node = root.querySelector(`[data-ov="${key}"]`); if (node) node.textContent = value; };
        set("mounts", num(next.overview.mounts));
        set("publishers", num(next.overview.publishers));
        set("subscribers", num(next.overview.subscribers));
        set("rtp", bytes(next.overview.rtp_octets));
        set("proxy-streaming", num(next.proxies.filter((item) => item.state === "streaming").length));
        set("proxy-total", num(next.proxies.length));
        set("attention-count", num(view.attention.length));
        const attentionValue = root.querySelector('[data-ov="attention-count"]');
        attentionValue?.classList.toggle("warn", view.attention.length > 0);
        attentionValue?.closest(".stat")?.classList.toggle("attention", view.attention.length > 0);
        set("attention-list-count", `${view.attention.length} 项`);
        set("runtime", uptime(next.overview.uptime_seconds));
        set("vods", num(next.overview.vods));
        set("frames", num(next.overview.accepted_frames));
        set("packets", num(next.overview.rtp_packets));
        set("active-count", `${view.active.length} 条`);
        set("changes-count", `${view.changes.length} 项`);
        markUpdated();
        pushSample(next.overview);
        root.querySelector("[data-ov-active]").innerHTML = activeHtml(view.active, next.system);
        root.querySelector("[data-ov-attention]").innerHTML = attentionHtml(view.attention);
        root.querySelector("[data-ov-changes]").innerHTML = changesHtml(view.changes);
      };
      const setAuto = (enabled) => {
        automatic = enabled;
        localStorage.setItem("rtflow-auto-overview", enabled ? "on" : "off");
        const button = root.querySelector("[data-overview-auto]");
        button.textContent = enabled ? "自动刷新：开" : "自动刷新：关";
        button.classList.toggle("active", enabled);
        if (timer) { clearInterval(timer); timer = null; }
        if (enabled) timer = setInterval(() => { if (!document.hidden) refresh(); }, 8000);
      };
      root.querySelector("[data-overview-auto]").onclick = () => setAuto(!automatic);
      root.querySelector("[data-overview-refresh]").onclick = () => refresh();
      root.addEventListener("click", async (event) => {
        if (!event.target.closest("[data-copy-push]")) return;
        const value = `${current.system.rtmp_base || `rtmp://${location.hostname}:1935`}/live/your-stream`;
        try { await navigator.clipboard.writeText(value); ctx.toast("推流地址已复制", "ok"); }
        catch (_) { ctx.toast("复制失败", "bad"); }
      });
      setAuto(automatic);
      return () => { if (timer) clearInterval(timer); };
    },
  };
}

function activeHtml(active, system) {
  if (active.length) {
    return `<div class="table-wrap"><table class="data"><thead><tr><th>流</th><th>编码</th><th>订阅</th><th>接收 / 丢弃</th></tr></thead><tbody>
      ${active.map((stream) => `<tr><td><a class="link-cell" href="#/live?q=${encodeURIComponent(stream.path)}">${esc(stream.name)}</a><div class="cell-sub mono">${esc(stream.path)}</div></td>
        <td><span class="badge codec">${esc(stream.video?.codec || "—")}</span></td><td class="mono">${num(stream.subscribers)}</td>
        <td class="mono"><span style="color:var(--ok)">${num(stream.accepted_frames)}</span> / <span style="color:var(--bad)">${num(stream.dropped_frames)}</span></td></tr>`).join("")}
    </tbody></table></div>`;
  }
  return `<div class="empty">${iconSvg("inbox")}<h3>暂无直播流</h3>
    <div class="faint">使用 RTMP / RTSP / SRT 推流后出现在这里；也可以先注册一条路径。</div>
    <div class="actions" style="justify-content:center;margin-top:14px"><a class="btn small" href="#/config/paths?new=1">新建路径</a>
      <button class="btn small ghost" data-copy-push>复制推流地址</button></div>
    <pre class="code" style="text-align:left;max-width:520px;margin:14px auto 0">${esc(system.rtmp_base || `rtmp://${location.hostname}:1935`)}/live/your-stream</pre>
  </div>`;
}

function attentionHtml(attention) {
  if (!attention.length) return `<div class="note">当前没有需要处理的异常。</div>`;
  return `<div class="stack" style="gap:8px">${attention.slice(0, 8).map((item) => `
    <div class="attention-item"><div><div style="display:flex;gap:7px;align-items:center"><span class="badge ${item.level}">${esc(item.kind)}</span><b>${esc(item.name)}</b></div>
    <div class="cell-sub">${esc(item.detail)}</div></div><a class="btn small ghost" href="${item.href}">查看</a></div>`).join("")}</div>`;
}

function changesHtml(changes) {
  if (!changes.length) return `<div class="note">暂无管理变更。</div>`;
  return `<div class="stack" style="gap:6px">${changes.map((event) => `
    <div class="event-row"><span class="badge">${esc(actionNames[event.action] || event.action)}</span><div class="event-main"><b>${esc(event.path || event.key_id || event.detail || "—")}</b><span class="cell-sub">${esc(event.user || event.role || "admin")} · ${esc(dateTime(event.ts))}</span></div></div>`).join("")}</div>`;
}
