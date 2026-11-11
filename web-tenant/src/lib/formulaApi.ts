import { call, type Balance, type Call as BaseCall, type CallsQuery, type LedgerItem, type LedgerKind, type Page } from "./api";

export interface ChargeCalculation {
  original_micro_cny: string | null;
  adjusted_micro_cny: string;
  final_micro_cny: string | null;
  rounding: "ceil_micro_cny";
}

export interface FormulaCall extends BaseCall {
  latency_ms: number | null;
  first_token_latency_ms: number | null;
  attempt_duration_ms: number | null;
  charge: BaseCall["charge"] & { calculation: ChargeCalculation };
}

export interface FormulaBalance extends Balance {
  calculation: {
    topups_micro_cny: string;
    usage_micro_cny: string;
    adjustments_micro_cny: string;
    balance_micro_cny: string;
    credit_limit_micro_cny: string;
    available_micro_cny: string;
    available_overflow: boolean;
    last_ledger_seq: string | null;
    as_of: string;
  };
}

export interface FormulaLedgerItem extends LedgerItem {
  calculation: { before_micro_cny: string; amount_micro_cny: string; after_micro_cny: string };
}

function query(q: Record<string, string | number | undefined>): string {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(q)) if (value !== undefined && value !== "") params.set(key, String(value));
  return params.size ? `?${params}` : "";
}

export async function formulaCalls(q: CallsQuery): Promise<Page<FormulaCall>> {
  return (await call<Page<FormulaCall>>("GET", `/usage/calls${query({ ...q })}`)).data;
}

export async function formulaBalance(): Promise<FormulaBalance> {
  return (await call<FormulaBalance>("GET", "/billing/balance")).data;
}

export async function formulaLedger(q: { from?: string; to?: string; kind?: LedgerKind; limit?: number; before?: number }): Promise<Page<FormulaLedgerItem>> {
  return (await call<Page<FormulaLedgerItem>>("GET", `/billing/ledger${query(q)}`)).data;
}
