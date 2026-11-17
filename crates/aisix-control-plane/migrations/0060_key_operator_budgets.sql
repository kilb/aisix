-- 平台可为单个密钥设置真实的平台额度；组织与成员层级保持原有设置方限制。
ALTER TABLE billing_budgets_cny DROP CONSTRAINT billing_budgets_cny_check2;
ALTER TABLE billing_budgets_cny ADD CONSTRAINT billing_budgets_cny_check2
  CHECK ((scope = 'team') = (set_by = 'operator')
    OR (scope = 'api_key' AND set_by = 'operator'));
