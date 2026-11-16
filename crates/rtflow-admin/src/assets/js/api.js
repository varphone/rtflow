export const BASE = window.__ADMIN_BASE__ || "/admin";
let csrf = "";

export function setCsrf(value) { csrf = value || ""; }

export class ApiError extends Error {
  constructor(message, status, body) {
    super(message);
    this.status = status;
    this.body = body || {};
  }
}

async function request(path, { method = "GET", body, headers = {}, v1 = false } = {}) {
  const merged = Object.assign({ Accept: "application/json" }, headers);
  if (body !== undefined) merged["Content-Type"] = "application/json";
  if (method !== "GET" && csrf) merged["X-CSRF-Token"] = csrf;
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 20000);
  let response;
  try {
    response = await fetch((v1 ? BASE + "/api/v1" : BASE + "/api") + path, {
      method,
      credentials: "same-origin",
      headers: merged,
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: controller.signal,
    });
  } catch (error) {
    if (error?.name === "AbortError") throw new ApiError("请求超时，请稍后重试", 0, {});
    throw new ApiError("网络连接失败，请检查服务状态", 0, {});
  } finally {
    clearTimeout(timeout);
  }
  const text = await response.text();
  let payload = {};
  try { payload = text ? JSON.parse(text) : {}; } catch (_) { payload = { message: text }; }
  if (response.status === 401) {
    window.dispatchEvent(new CustomEvent("rtflow:unauthorized"));
  }
  if (!response.ok) {
    throw new ApiError(payload.error || payload.message || `请求失败 (${response.status})`, response.status, payload);
  }
  return payload;
}

export const api = (path, options) => request(path, options || {});
export const apiV1 = (path, options) => request(path, Object.assign({}, options, { v1: true }));
