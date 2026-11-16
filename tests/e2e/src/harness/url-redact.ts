// 与 aisix-core::redact_url_for_log 相同：配置 URL 的展示副本只保留 scheme/host/port。
export function redactUrlForLog(raw: string): string {
  try {
    const url = new URL(raw);
    if (!url.host) return "<invalid URL>";
    url.username = "";
    url.password = "";
    url.pathname = "/";
    url.search = "";
    url.hash = "";
    return url.toString();
  } catch (error) {
    if (error instanceof TypeError) return "<invalid URL>";
    throw error;
  }
}
