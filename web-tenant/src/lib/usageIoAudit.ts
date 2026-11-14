export interface TextMeasurement {
  utf8_bytes: number;
  unicode_scalars: number;
  complete: boolean;
}

interface UsageIoMeasurement {
  request_body_bytes?: number | null;
  response_body_bytes?: number | null;
  request_complete?: boolean | null;
  response_complete: boolean;
  exchange_count?: number | null;
  transport?: string | null;
  input_text?: TextMeasurement | null;
  output_text?: TextMeasurement | null;
}

export type UsageIoAudit = UsageIoMeasurement & (
  | { version: 1; boundary: "client_exchange" }
  | { version: 2; boundary: "upstream_attempt" }
);

/** 版本和边界必须成对匹配，避免把历史客户端观测重标为上游观测。 */
export function isSupportedUsageIoAudit(value: unknown): value is UsageIoAudit {
  if (value == null || typeof value !== "object") return false;
  const audit = value as Record<string, unknown>;
  return (audit.version === 1 && audit.boundary === "client_exchange")
    || (audit.version === 2 && audit.boundary === "upstream_attempt");
}

export interface IoTokenReference {
  method: "utf8_bytes_div4_v1";
  input_tokens?: number | null;
  output_tokens?: number | null;
}

/** 长度与计价用量分开投影，缺失不代表零。 */
export interface UsageAuditData {
  io_audit?: UsageIoAudit | null;
  io_token_reference?: IoTokenReference | null;
  usage?: Partial<Record<string, number>> | null;
  usage_input_tokens?: number | null;
  usage_estimated?: boolean | null;
}
