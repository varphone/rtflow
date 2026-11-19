import { api } from "../api.js";
import { esc, uptime } from "../format.js";
import { closeDialog, copyText, openDialog, toast } from "../ui.js";

const KINDS = {
  rtmp: { label: "RTMP 推流", detail: "推送到本机 RTMP 服务", defaultMount: "rtmp" },
  rtsp_tcp: { label: "RTSP TCP 推流", detail: "推送到本机 RTSP 服务（TCP）", defaultMount: "rtsp" },
  rtsp_udp: { label: "RTSP UDP 推流", detail: "推送到本机 RTSP 服务（UDP）", defaultMount: "rtsp" },
  srt: { label: "SRT 推流", detail: "推送到本机 SRT 监听", defaultMount: "srt" },
};

function stateBadge(state) {
  if (state === "running" || state === "starting") return `<span class="badge ok">${esc(state)}</span>`;
  if (state === "stopping") return `<span class="badge warn">stopping</span>`;
  return `<span class="badge bad">${esc(state)}</span>`;
}

function absolute(path) {
  if (!path) return "";
  if (/^[a-z]+:\/\//i.test(path)) return path;
  return location.origin + path;
}

function jobRow(job) {
  const urls = job.urls || {};
  const hls = absolute(urls.hls);
  const dash = absolute(urls.dash);
  const whep = absolute(urls.whep);
  const live = `#/live?q=${encodeURIComponent(job.mount)}`;
  const stderr = (job.stderr || []).slice(-8).join("\n");
  return `<tr>
    <td class="mono">${esc(job.kind)}<div class="cell-sub mono">${esc(job.mount)}</div></td>
    <td>${stateBadge(job.state)}${job.exit_code !== null && job.exit_code !== undefined ? `<div class="cell-sub">exit ${esc(String(job.exit_code))}</div>` : ""}</td>
    <td class="mono">${job.pid ? esc(String(job.pid)) : "—"}<div class="cell-sub">${esc(uptime(Math.max(0, Math.round((Date.now() - Number(job.started_ms || Date.now())) / 1000))))}</div></td>
    <td class="mono">${esc(String(job.duration_secs || 0))}s</td>
    <td><div class="row-actions">
      <button class="btn small" data-test-live="${esc(job.mount)}">打开直播</button>
      <button class="btn small ghost" data-test-copy data-copy="${esc(job.mount)}">复制 mount</button>
      <button class="btn small ghost" data-test-copy data-copy="${esc(urls.rtsp || "")}">复制 RTSP</button>
      ${job.state === "running" || job.state === "starting" || job.state === "stopping" ? `<button class="btn small danger" data-test-stop="${esc(String(job.id))}">停止</button>` : ""}
    </div><div class="cell-sub mono">${esc(urls.rtsp || "")}</div>${stderr ? `<details class="cell-sub"><summary>stderr</summary><pre class="code">${esc(stderr)}</pre></details>` : ""}</td>
  </tr>`;
}

export async function render() {
  const data = await api("/testing");
  const enabled = data.enabled === true;
  const ffmpeg = data.ffmpeg || {};
  const jobs = data.jobs || [];
  const active = jobs.filter((job) => ["starting", "running", "stopping"].includes(job.state)).length;
  const cards = Object.entries(KINDS).map(([kind, info]) => `<button class="panel test-card" data-test-kind="${kind}" ${enabled ? "" : "disabled"}>
      <span class="badge">${esc(info.label)}</span>
      <b>${esc(info.detail)}</b>
      <span class="note">testsrc2 · H.264 · AAC</span>
    </button>`).join("");
  const body = enabled
    ? `<div class="stack">
      <div class="two-col">
        <section class="panel"><div class="panel-head"><h2>快速测试推流</h2><span class="sub">固定源 · 不读用户文件</span><span class="badge ${active >= data.max_jobs ? "warn" : "ok"}">${active}/${data.max_jobs}</span></div>
          <div class="panel-body"><p class="note">启动后 1–3 秒内直播页会出现对应 <code>${esc("/live/test-*")}</code>，可用 HLS/DASH/WebRTC 播放，或直接复制 RTSP 地址用 VLC/ffplay 拉流。</p><div class="test-grid">${cards}</div></div></section>
        <section class="panel"><div class="panel-head"><h2>运行环境</h2></div><div class="panel-body"><dl class="kv">
          <dt>ffmpeg</dt><dd>${ffmpeg.available ? `<span class="badge ok">可用</span><div class="mono cell-sub">${esc(ffmpeg.version || "")}</div>` : `<span class="badge bad">不可用</span><div class="note">${esc(ffmpeg.error || "")}</div>`}</dd>
          <dt>任务上限</dt><dd class="mono">${esc(String(data.max_jobs))}</dd>
          <dt>默认时长</dt><dd class="mono">${esc(String(data.default_duration_secs))}s</dd>
          <dt>最大时长</dt><dd class="mono">${esc(String(data.max_duration_secs))}s</dd>
          <dt>RTSP 服务</dt><dd>${jobs.find((job) => job.urls && job.urls.rtsp)?.urls.rtsp ? `<span class="mono">${esc(jobs.find((job) => job.urls && job.urls.rtsp).urls.rtsp)}</span>` : "启动测试流后生成"}</dd>
        </dl><p class="note" style="margin-top:10px">测试推流会占用 CPU 与带宽；这是单路体验工具，不是压测工具。</p></div></section>
      </div>
      <section class="panel"><div class="panel-head"><h2>测试任务</h2><span class="sub">含 stderr 末尾</span></div>
        <div class="panel-body">${jobs.length ? `<div class="table-wrap"><table class="data"><caption class="sr-only">测试任务</caption><thead><tr><th>类型 / mount</th><th>状态</th><th>PID / 运行</th><th>时长</th><th>操作</th></tr></thead><tbody>${jobs.map(jobRow).join("")}</tbody></table></div>` : `<div class="empty"><b>暂无测试任务</b><span>从上方选择一种推流方式开始。</span></div>`}</div>
      </section>
      <section class="panel"><div class="panel-head"><h2>播放与验证</h2></div><div class="panel-body"><div class="test-links">
        <div><b>RTSP</b><span>测试任务启动后，将行内 RTSP 地址复制到 VLC/ffplay；rtflow 自身即 RTSP 服务端。</span></div>
        <div><b>HLS / DASH</b><span>在“直播流”页找到测试 mount，点 HLS/DASH 播放。</span></div>
        <div><b>WebRTC</b><span>行内 mount 进入直播详情，点 WebRTC 预览；延迟最低。</span></div>
      </div></div></section>
    </div>`
    : `<section class="panel"><div class="panel-head"><h2>测试功能未启用</h2></div><div class="panel-body"><p class="note">在 <code>rtflow.toml</code> 中显式开启；默认关闭以避免管理台启动进程：</p><pre class="code">[testing]
enabled = true
ffmpeg = "ffmpeg"
max_jobs = 2</pre><p class="note">启用后仅管理员可操作，且只允许固定测试源与本机目标地址。</p></div></section>`;

  return {
    title: "测试体验",
    subtitle: "启动合成测试流，验证 RTMP/RTSP/SRT 接入与 HLS/DASH/WebRTC 播放。",
    actions: enabled ? `<button class="btn small" data-test-refresh>刷新</button>` : "",
    html: body,
    mount(root, ctx) {
      let timer = null;
      root.querySelectorAll("[data-test-kind]").forEach((button) => {
        button.onclick = () => openStart(button.dataset.testKind, ctx, data.default_duration_secs, data.max_duration_secs);
      });
      root.querySelectorAll("[data-test-stop]").forEach((button) => {
        button.onclick = async () => {
          button.disabled = true;
          try {
            await api("/testing/stop", { method: "POST", body: { id: Number(button.dataset.testStop) } });
            toast("已发送停止请求", "ok");
            ctx.reload();
          } catch (error) {
            toast(error.message, "bad");
            button.disabled = false;
          }
        };
      });
      root.querySelectorAll("[data-test-copy]").forEach((button) => {
        button.onclick = async () => {
          try { await copyText(button.dataset.copy); toast("地址已复制", "ok"); } catch (_) { toast("复制失败", "bad"); }
        };
      });
      root.querySelectorAll("[data-test-live]").forEach((button) => {
        button.onclick = () => { location.hash = `#/live?q=${encodeURIComponent(button.dataset.testLive)}`; };
      });
      root.querySelector("[data-test-refresh]")?.addEventListener("click", () => ctx.reload());
      if (jobs.some((job) => ["starting", "running", "stopping"].includes(job.state))) {
        timer = setInterval(() => ctx.reload({ silent: true }), 2000);
      }
      return () => { if (timer) clearInterval(timer); };
    },
  };
}

function openStart(kind, ctx, defaultDuration = 60, maxDuration = 31536000) {
  const info = KINDS[kind];
  openDialog({
    title: info.label,
    body: `<form id="test-start-form" class="stack">
      <p class="note">${esc(info.detail)}；输出为 testsrc2 测试画面 + 1 kHz 音频。</p>
      <div class="field"><label for="test-width">分辨率</label><div class="inline-fields">
        <input class="input" id="test-width" name="width" type="number" min="320" max="1920" step="2" value="1280" required>
        <span>×</span>
        <input class="input" id="test-height" name="height" type="number" min="180" max="1080" step="2" value="720" required>
      </div></div>
      <div class="field"><label for="test-fps">帧率</label><input class="input" id="test-fps" name="fps" type="number" min="1" max="60" value="30" required></div>
      <div class="field"><label for="test-bitrate">视频码率 (kbps)</label><input class="input" id="test-bitrate" name="bitrate" type="number" min="64" max="12000" value="2500" required></div>
      <div class="field"><label for="test-duration">时长（秒）</label><input class="input" id="test-duration" name="duration" type="number" min="1" max="${esc(String(maxDuration || 31536000))}" value="${esc(String(defaultDuration || 60))}" required></div>
      <label class="check"><input type="checkbox" name="audio" checked> 包含 AAC 音频</label>
      <div class="form-error" data-test-error hidden></div>
    </form>`,
    footer: `<button class="btn" data-close-dialog>取消</button><button class="btn primary" data-test-submit>开始推流</button>`,
    onMount(root) {
      const form = root.querySelector("#test-start-form");
      const error = root.querySelector("[data-test-error]");
      const submit = root.querySelector("[data-test-submit]");
      submit.onclick = async () => {
        error.hidden = true;
        if (!form.reportValidity()) return;
        submit.disabled = true;
        submit.classList.add("loading");
        try {
          const body = {
            kind,
            width: Number(form.width.value),
            height: Number(form.height.value),
            fps: Number(form.fps.value),
            bitrate_kbps: Number(form.bitrate.value),
            audio: form.audio.checked,
            duration_secs: Number(form.duration.value),
          };
          await api("/testing/start", { method: "POST", body });
          closeDialog();
          toast("测试推流已启动", "ok");
          ctx.reload();
        } catch (errorValue) {
          error.textContent = errorValue.message;
          error.hidden = false;
          submit.disabled = false;
        } finally {
          submit.classList.remove("loading");
        }
      };
    },
  });
}
