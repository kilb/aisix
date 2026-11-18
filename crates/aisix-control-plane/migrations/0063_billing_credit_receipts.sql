-- 保存手工入账的原始回执，租户或操作员更名后重送仍能返回同一结果。
CREATE TABLE billing_credit_receipts (
    tenant_id uuid NOT NULL REFERENCES billing_accounts(tenant_id),
    operation_id uuid NOT NULL,
    topup_id uuid NOT NULL UNIQUE REFERENCES billing_topups_cny(id),
    receipt jsonb NOT NULL,
    PRIMARY KEY (tenant_id, operation_id)
);
