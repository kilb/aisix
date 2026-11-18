-- 管理目录沿既有顺序走索引，避免每一页重新排序整张表。
CREATE INDEX tenants_admin_page ON tenants (created_at, id);
CREATE INDEX tenants_admin_status_page ON tenants (status, created_at, id);
CREATE INDEX gw_resources_admin_page ON gw_resources (kind, identity, id) WHERE NOT deleted;
