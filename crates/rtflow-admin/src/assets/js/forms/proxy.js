import { openDrawer, field, textInput, toast } from "../ui.js";
import { apiV1 } from "../api.js";
import { esc, uniqueName } from "../format.js";

const SCHEDULE_DAYS = [
  ["mon", "周一"],
  ["tue", "周二"],
  ["wed", "周三"],
  ["thu", "周四"],
  ["fri", "周五"],
  ["sat", "周六"],
  ["sun", "周日"],
];

const NEW_OUTPUTS = {
  read: true,
  hls: true,
  dash: false,
  flv: true,
  webrtc: false,
  record: false,
  max_readers: 0,
};

function upstreamRow(upstream = {}, index = 0, prefillPassword = false) {
  return `<div class="form-section" data-upstream-row>
    <div style="display:flex;align-items:center;gap:8px"><h3 style="margin:0">上游 ${index + 1}</h3>
      <button type="button" class="btn ghost small" data-remove-upstream style="margin-left:auto">移除</button></div>
    ${field("RTSP URL", textInput("url", upstream.url || "", `required placeholder="rtsp://192.0.2.10/live"`))}
    <div class="field-row">
      ${field("传输", `<select class="input" name="transport">
        <option value="tcp" ${upstream.transport === "tcp" ? "selected" : ""}>TCP</option>
        <option value="udp" ${upstream.transport === "udp" ? "selected" : ""}>UDP</option>
        <option value="auto" ${upstream.transport === "auto" ? "selected" : ""}>Auto</option>
      </select>`)}
      ${field("用户名", textInput("username", upstream.username || ""))}
    </div>
    ${field("密码", `<input class="input" type="password" name="password" autocomplete="new-password" spellcheck="false" value="${prefillPassword ? esc(upstream.password || "") : ""}" placeholder="${upstream.password && !prefillPassword ? "留空保持不变" : ""}">`)}
  </div>`;
}

function outputChecks(outputs) {
  const item = (name, label, checked) => `<label class="checkline"><input type="checkbox" name="${name}" ${checked ? "checked" : ""}> ${label}</label>`;
  return `${item("output_hls", "HLS", outputs.hls)}${item("output_flv", "HTTP-FLV", outputs.flv)}${item("output_dash", "DASH", outputs.dash)}${item("output_webrtc", "WebRTC", outputs.webrtc)}`;
}

export function openProxyDrawer(existing, onSaved, options = {}) {
  const clone = options.clone === true && Boolean(existing);
  const source = existing || options.draft || { path: "", mode: "on-demand", idle_grace_ms: null, upstreams: [{ url: "", transport: "tcp" }] };
  const mount = clone ? { ...source, path: uniqueName(`${source.path}-copy`, options.taken || []) } : source;
  const editing = Boolean(existing) && !clone;
  const upstreams = mount.upstreams?.length ? mount.upstreams : [{ url: "", transport: "tcp" }];
  const prefillPassword = !existing && Boolean(options.draft);
  const managed = Boolean(existing) && !clone ? Boolean(existing.managed) : true;
  const outputs = Object.assign({}, NEW_OUTPUTS, mount.outputs || {});
  const schedule = mount.schedule || { days: SCHEDULE_DAYS.map(([value]) => value), start: "08:00", end: "18:00" };
  const scheduleOn = Boolean(mount.schedule);
  openDrawer({
    title: clone ? "复制 RTSP 拉流" : (existing ? "编辑 RTSP 拉流" : "新增 RTSP 拉流"),
    description: clone ? "复制上游、模式与输出；路径不能与现有拉流源重复。" : "主动拉取第三方 RTSP 设备；密码只写不读。",
    body: `
      <div class="form-section">
        <h3>拉流</h3>
        ${field("路径", textInput("path", mount.path, `required ${editing ? "disabled" : ""} placeholder="/cameras/front"`), { hint: "作为直播、录制与点播的路径标识。" })}
        <div class="field-row">
          ${field("模式", `<select class="input" name="mode">
            <option value="on-demand" ${mount.mode === "on-demand" ? "selected" : ""}>按需</option>
            <option value="always" ${mount.mode === "always" ? "selected" : ""}>常连</option>
          </select>`, { hint: "常连会持续占用上游带宽。" })}
          ${field("空闲断开", textInput("idle_grace", mount.idle_grace_ms ? Math.round(mount.idle_grace_ms / 1000) + "s" : "", "placeholder=\"5s\""))}
        </div>
      </div>
      <div class="form-section">
        <h3>输出</h3>
        ${!managed ? `<label class="checkline"><input type="checkbox" data-manage-outputs> 由拉流源管理直播与录制输出</label>` : ""}
        <div data-outputs-fields ${!managed ? "hidden" : ""}>
          <div style="display:flex;flex-wrap:wrap;gap:10px 16px">${outputChecks(outputs)}</div>
          <label class="checkline"><input type="checkbox" name="output_record" ${outputs.record ? "checked" : ""}> 录制</label>
          <span class="hint">常连模式下无需观看即可持续录制；按需模式需要首次观看触发。</span>
          ${field("最大观看数", `<input class="input" type="number" min="0" name="output_max_readers" value="${Number(outputs.max_readers || 0)}">`, { hint: "0 表示不限制。" })}
        </div>
        ${!managed ? `<span class="hint">关闭时沿用“路径”页为该路径配置的输出策略。</span>` : ""}
      </div>
      <div class="form-section">
        <h3>定时</h3>
        <label class="checkline"><input type="checkbox" data-schedule-enabled ${scheduleOn ? "checked" : ""}> 启用定时拉流</label>
        <div data-schedule-fields ${scheduleOn ? "" : "hidden"}>
          <div style="display:flex;flex-wrap:wrap;gap:8px 14px">
            ${SCHEDULE_DAYS.map(([value, label]) => `<label class="checkline"><input type="checkbox" name="schedule_day_${value}" ${(schedule.days || []).includes(value) ? "checked" : ""}> ${label}</label>`).join("")}
          </div>
          <div class="field-row">
            ${field("开始", `<input class="input" type="time" name="schedule_start" value="${esc(schedule.start || "08:00")}">`)}
            ${field("结束", `<input class="input" type="time" name="schedule_end" value="${esc(schedule.end || "18:00")}">`)}
          </div>
          <span class="hint">按服务器本地时间执行；结束早于开始表示跨天。窗口外停止拉流并结束录制。</span>
        </div>
      </div>
      <div id="upstreams" class="form-list">${upstreams.map((upstream, index) => upstreamRow(upstream, index, prefillPassword)).join("")}</div>
      <button type="button" class="btn small" data-add-upstream>添加上游</button>`,
    submitLabel: editing ? "保存修改" : (clone ? "创建副本" : "创建拉流"),
    async onSubmit(data, { close }) {
      const rows = [...document.querySelectorAll("#upstreams [data-upstream-row]")];
      const urls = rows.map((row) => ({
        url: row.querySelector("[name=url]").value,
        transport: row.querySelector("[name=transport]").value,
        username: row.querySelector("[name=username]").value || null,
        password: row.querySelector("[name=password]").value || null,
      }));
      const manage = document.querySelector("[data-manage-outputs]");
      const manageOutputs = !manage || manage.checked;
      const body = {
        path: editing ? mount.path : data.get("path"),
        enabled: true,
        mode: data.get("mode"),
        idle_grace: data.get("idle_grace") || null,
        upstreams: urls,
      };
      if (manageOutputs) {
        body.outputs = {
          read: true,
          hls: data.get("output_hls") === "on",
          flv: data.get("output_flv") === "on",
          dash: data.get("output_dash") === "on",
          webrtc: data.get("output_webrtc") === "on",
          record: data.get("output_record") === "on",
          max_readers: Number(data.get("output_max_readers") || 0),
        };
      }
      const scheduleOn = document.querySelector("[data-schedule-enabled]")?.checked;
      if (scheduleOn) {
        const days = SCHEDULE_DAYS.filter(([value]) => data.get(`schedule_day_${value}`) === "on").map(([value]) => value);
        if (!days.length) throw new Error("定时拉流至少选择一天");
        body.schedule = {
          days,
          start: data.get("schedule_start"),
          end: data.get("schedule_end"),
        };
      } else {
        body.schedule = null;
      }
      await apiV1("/proxies", { method: editing ? "PATCH" : "POST", body });
      close();
      toast(editing ? "拉流源已保存" : "拉流源已创建", "ok");
      onSaved();
    },
    onMount(root) {
      const takeover = root.querySelector("[data-manage-outputs]");
      const fields = root.querySelector("[data-outputs-fields]");
      const record = root.querySelector('[name="output_record"]');
      const mode = root.querySelector('[name="mode"]');
      const scheduleToggle = root.querySelector("[data-schedule-enabled]");
      const scheduleFields = root.querySelector("[data-schedule-fields]");
      const syncSchedule = () => {
        if (scheduleToggle && scheduleFields) {
          scheduleFields.hidden = !scheduleToggle.checked;
          scheduleFields.querySelectorAll("input").forEach((input) => { input.disabled = !scheduleToggle.checked; });
        }
      };
      scheduleToggle?.addEventListener("change", syncSchedule);
      syncSchedule();
      const sync = () => {
        if (takeover && fields) {
          fields.hidden = !takeover.checked;
          fields.querySelectorAll("input, select").forEach((input) => { input.disabled = !takeover.checked; });
        }
      };
      takeover?.addEventListener("change", sync);
      record?.addEventListener("change", () => { if (record.checked && mode) mode.value = "always"; });
      sync();
      root.querySelector("[data-add-upstream]").onclick = () => {
        const container = root.querySelector("#upstreams");
        container.insertAdjacentHTML("beforeend", upstreamRow({}, container.children.length));
      };
      root.addEventListener("click", (event) => {
        if (event.target.closest("[data-remove-upstream]")) {
          if (root.querySelectorAll("[data-upstream-row]").length > 1) event.target.closest("[data-upstream-row]").remove();
        }
      });
    },
  });
}
