import { test, expect } from "@playwright/test";
import { readFileSync, mkdirSync } from 'node:fs';
import { join, resolve } from 'node:path';
const session = resolve(process.env.LAYOUT_QA_SESSION),
  output = resolve(process.env.LAYOUT_QA_OUTPUT);
const world = JSON.parse(readFileSync(join(session, 'public.json'), 'utf8')),
  auth = JSON.parse(readFileSync(join(session, 'local-auth.json'), 'utf8'));
if (!world.ready || world.mockUpstream !== false || !world.realBackend) throw Error('A real CP/Postgres session is required');
for (const k of ['adminUrl', 'tenantUrl', 'httpUrl']) {
  const u = new URL(world[k]);
  if (u.protocol !== 'http:' || u.hostname !== '127.0.0.1' || u.username || u.password) throw Error('Only isolated loopback origins are accepted');
}
const health = new WeakMap();
async function settle(page) {
  await expect(page.locator('main h1:visible')).toHaveCount(1);
  await page.waitForLoadState('networkidle');
  await page.waitForFunction(() => ![...document.querySelectorAll('main [aria-busy="true"],main [role=status]')].some(e => e.getAttribute('aria-busy') === 'true' || e.textContent?.includes('载入中')));
  await page.evaluate(async () => {
    await document.fonts.ready;
    await new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)));
  });
  await expect(page.locator('vite-error-overlay')).toHaveCount(0);
}
async function login(page, surface) {
  const base = surface === 'admin' ? world.adminUrl : world.tenantUrl,
    prefix = surface === 'admin' ? '/admin' : '/api',
    who = surface === 'admin' ? auth.operator : auth.metricsOwner;
  await page.goto(base);
  await page.getByLabel('邮箱', {
    exact: true
  }).fill(who.email);
  await page.getByLabel('口令', {
    exact: true
  }).fill(who.password);
  const answer = page.waitForResponse(r => new URL(r.url()).pathname === prefix + '/login' && r.request().method() === 'POST');
  await page.getByRole('button', {
    name: '登录',
    exact: true
  }).click();
  expect((await answer).status()).toBe(200);
  await settle(page);
  const ledger = {
    errors: [],
    writes: [],
    forbidden: [],
    responses: []
  };
  health.set(page, ledger);
  page.on('pageerror', e => ledger.errors.push(e.message));
  page.on('console', m => {
    if (m.type() === 'error') ledger.errors.push(m.text());
  });
  page.on('request', r => {
    const p = new URL(r.url()).pathname;
    if (!['GET', 'HEAD'].includes(r.method())) ledger.writes.push(r.method() + ' ' + p);
    if (/^\/(?:v1|v1beta|v2)\/|\/(?:probe|probes|model-tests|upstream-models)(?:\/|$)/.test(p)) ledger.forbidden.push(r.method() + ' ' + p);
  });
  page.on('response', r => {
    const p = new URL(r.url()).pathname;
    if (/^\/(?:api|admin)\//.test(p)) ledger.responses.push({
      status: r.status(),
      path: p
    });
  });
  return base;
}
async function navigate(page, base, route) {
  await page.goto(base.replace(/\/$/, '') + '/#' + route);
  await settle(page);
  expect(new URL(page.url()).hash).toBe('#' + route);
}
async function capture(page, name, fullPage = true) {
  mkdirSync(join(output, 'screenshots'), {
    recursive: true
  });
  await page.screenshot({
    path: join(output, 'screenshots', name + '.png'),
    fullPage,
    animations: 'disabled'
  });
}
async function noOverflow(page) {
  const x = await page.evaluate(() => ({
    viewport: document.documentElement.clientWidth,
    doc: document.documentElement.scrollWidth,
    body: document.body.scrollWidth
  }));
  expect(x.doc, 'document stays in viewport').toBeLessThanOrEqual(x.viewport + 1);
  expect(x.body, 'body stays in viewport').toBeLessThanOrEqual(x.viewport + 1);
}
async function get(page, path) {
  const r = await page.evaluate(async path => {
    const response = await fetch(path, {
      credentials: 'include'
    });
    return {
      status: response.status,
      body: await response.json()
    };
  }, path);
  expect(r.status, 'real independent API oracle').toBe(200);
  return r.body;
}
function scope(page, surface) {
  return surface === 'admin' ? page.locator('.charge-filter-controls') : page.locator('.usage-record-section').first();
}
async function filterGeometry(page, surface) {
  return scope(page, surface).evaluate(el => [...el.querySelectorAll('input:not([type=hidden]),button[role=combobox],button')].filter(e => {
    const r = e.getBoundingClientRect();
    if (r.width < 10 || r.height < 10 || !e.checkVisibility({
      checkVisibilityCSS: true
    }) || e.closest('[hidden],.record-table-toolbar,table,.dt-pagination,.dt-footer,details:not([open])')) return false;
    if (e.closest('.usage-record-section') && !e.closest('.usage-filters,.usage-record-filter-primary')) return false;
    return ![...(e.closest('.usage-record-section')?.querySelectorAll('.record-table-toolbar') ?? [])].some(t => t.contains(e));
  }).map(e => {
    const r = e.getBoundingClientRect();
    return {
      tag: e.tagName,
      text: e.getAttribute('aria-label') || e.getAttribute('placeholder') || e.textContent,
      top: r.top,
      bottom: r.bottom,
      left: r.left,
      right: r.right
    };
  }));
}
test.afterEach(async ({
  page
}) => {
  const h = health.get(page);
  if (!h) return;
  await test.info().attach('real-api-health', {
    body: JSON.stringify(h),
    contentType: 'application/json'
  });
  expect.soft(h.errors, 'console and runtime errors match only an explicitly induced API failure').toEqual(h.expectedErrors ?? []);
  expect.soft(h.writes, 'no business writes').toEqual([]);
  expect.soft(h.forbidden, 'no inference or model probing').toEqual([]);
  expect.soft(h.responses.filter(r => r.status >= 400), 'real API failures match only an explicitly induced invalid query').toEqual(h.expectedFailures ?? []);
});

// 这些断言来自用户可见合同；旧版缺图标、筛选换行、时间混列或路由过高会失败。
test('tenant favicon and five named metric tabs carry icons with keyboard navigation', async ({
  page
}) => {
  const base = await login(page, 'tenant');
  await navigate(page, base, 'usage');
  const icon = page.locator('link[rel~="icon"]');
  await expect(icon).toHaveCount(1);
  const url = await icon.getAttribute('href');
  expect(url).toBeTruthy();
  const loaded = await page.evaluate(async url => {
    const r = await fetch(new URL(url, location.href));
    const body = await r.text();
    return {
      status: r.status,
      svg: body.includes('<svg')
    };
  }, url);
  expect(loaded).toEqual({
    status: 200,
    svg: true
  });
  const list = page.getByRole('tablist', {
    name: '指标视图',
    exact: true
  });
  for (const name of ['请求', '延迟', 'Token', '缓存', '费用']) {
    const tab = list.getByRole('tab', {
      name,
      exact: true
    });
    await expect(tab).toBeVisible();
    await expect(tab.locator('svg[aria-hidden="true"]')).toHaveCount(1);
  }
  const first = list.getByRole('tab', {
    name: '请求',
    exact: true
  });
  await first.focus();
  await page.keyboard.press('End');
  await expect(list.getByRole('tab', {
    name: '费用',
    exact: true
  })).toHaveAttribute('aria-selected', 'true');
  await page.keyboard.press('Home');
  await expect(first).toHaveAttribute('aria-selected', 'true');
  await page.keyboard.press('ArrowRight');
  await expect(list.getByRole('tab', {
    name: '延迟',
    exact: true
  })).toHaveAttribute('aria-selected', 'true');
  await page.setViewportSize({
    width: 390,
    height: 1000
  });
  const y = await list.getByRole('tab').evaluateAll(es => es.map(e => Math.round(e.getBoundingClientRect().y)));
  expect(new Set(y).size, 'all five tabs remain on one mobile row').toBe(1);
  await noOverflow(page);
  await capture(page, 'tenant-metric-tabs-icons-390');
});
for (const surface of ['admin', 'tenant']) {
  for (const width of [1440, 1280]) {
    test(`${surface} calls at ${width} have one desktop filter row and separate time / grouped performance columns`, async ({
      page
    }) => {
      await page.setViewportSize({
        width,
        height: 1000
      });
      const base = await login(page, surface);
      await navigate(page, base, 'calls');
      await capture(page, surface + '-calls-' + width);
      const geometry = await filterGeometry(page, surface);
      await test.info().attach('primary-filter-geometry', {
        body: JSON.stringify(geometry),
        contentType: 'application/json'
      });
      expect(geometry.length, 'the real filters are present').toBeGreaterThanOrEqual(5);
      const centers = geometry.filter(x => !x.text?.includes('更多')).map(x => (x.top + x.bottom) / 2);
      expect.soft(Math.max(...centers) - Math.min(...centers), 'primary filters fit one row at the selected desktop width').toBeLessThanOrEqual(18);
      const table = page.getByRole('table', {
        name: surface === 'admin' ? '计费明细' : '调用记录',
        exact: true
      });
      await expect(table.getByRole('columnheader', {
        name: '时间',
        exact: true
      })).toHaveCount(1);
      const timeIndex = await table.getByRole('columnheader', {
        name: '时间',
        exact: true
      }).evaluate(e => (e as HTMLTableCellElement).cellIndex);
      const perfIndex = await table.getByRole('columnheader', {
        name: /响应性能/
      }).evaluate(e => (e as HTMLTableCellElement).cellIndex);
      expect(timeIndex).not.toBe(perfIndex);
      const rows = table.locator('tbody tr');
      expect(await rows.count(), 'retained real calls exist').toBeGreaterThan(0);
      for (const row of await rows.all()) {
        const cell = row.locator('td').nth(timeIndex);
        const text = (await cell.innerText()).trim();
        expect(text, 'time cell shows a date and clock on separate lines').toMatch(/\d{4}-\d{2}-\d{2}\s*\n\s*\d{2}:\d{2}:\d{2}/);
        const p = row.locator('td').nth(perfIndex);
        await expect(p).toContainText('首字');
        await expect(p).toContainText('总用时');
        await expect(p).toContainText('输出 TPS');
      }
      await noOverflow(page);
    });
  }
}
test('catalog targets share a compact row with groups and keep management controls', async ({ page }) => {
  const base = await login(page, 'admin');
  for (const viewportWidth of [1440, 1280]) {
    await page.setViewportSize({ width: viewportWidth, height: 1000 });
    await navigate(page, base, 'models');
    const table = page.getByRole('table', { name: '全部模型', exact: true });
    const target = table.getByRole('columnheader', { name: '上游账号 / 路由目标', exact: true });
    const width = (await target.boundingBox()).width;
    expect.soft(width, 'targets receive at least 35% of usable table width').toBeGreaterThanOrEqual((await table.boundingBox()).width * .35);
    const rows = table.locator('tbody tr').filter({ hasText: '路由' });
    expect(await rows.count()).toBeGreaterThan(0);
    const sample = rows.filter({ has: page.locator('td:first-child').getByText(world.tokenUiFixture.publicModelName, { exact: true }) });
    const index = await target.evaluate(e => (e as HTMLTableCellElement).cellIndex);
    const cell = sample.locator('td').nth(index);
    const groups = cell.getByText('可用分组', { exact: true });
    const firstBadge = cell.getByText(world.tokenUiFixture.groupName, { exact: true }).first();
    await expect(groups).toBeVisible();
    await expect(firstBadge, 'the actual first group value is present').toBeVisible();
    expect((await cell.innerText()).length).toBeGreaterThan(20);
    expect.soft((await sample.boundingBox()).height, 'single target routing row stays compact').toBeLessThanOrEqual(90);
    const firstline = await cell.locator(':scope > *').first().boundingBox();
    const label = await groups.boundingBox(), badge = await firstBadge.boundingBox();
    expect.soft(Math.abs(label.y - firstline.y), 'group label aligns with the target first line').toBeLessThanOrEqual(12);
    expect.soft(Math.abs(badge.y - firstline.y), 'the actual first group badge aligns with the target first line').toBeLessThanOrEqual(12);
    await test.info().attach('catalog-first-group-geometry-' + viewportWidth, {
      body: JSON.stringify({ target: firstline, label, badge, group: world.tokenUiFixture.groupName }),
      contentType: 'application/json',
    });
    await expect(sample.getByRole('button').first()).toBeVisible();
    await capture(page, 'admin-models-compact-' + viewportWidth);
    await noOverflow(page);
  }
});

for (const surface of ['admin', 'tenant']) {
  for (const width of [1440, 1280, 390]) {
    test(`${surface} affected and neighbouring data pages render without overflow at ${width}`, async ({
      page
    }) => {
      await page.setViewportSize({
        width,
        height: 1000
      });
      const base = await login(page, surface);
      const routes = surface === 'admin' ? ['analytics', 'calls', 'providers', 'models', 'public-routes', 'tenants', 'keys', 'operators', 'resources', 'totals'] : ['usage', 'calls', 'members', 'keys', 'prices', 'org', 'billing', 'budgets', 'account'];
      for (const route of routes) {
        await navigate(page, base, route);
        const names = surface === 'admin' ? {
          analytics: '用量分析',
          calls: '调用记录',
          providers: '上游账号',
          models: '模型目录',
          'public-routes': '公共模型与路由',
          tenants: '租户',
          keys: '密钥',
          operators: '运维账号',
          resources: '全部资源',
          totals: '总收入与总成本'
        } : {
          usage: '用量分析',
          calls: '调用记录',
          members: '成员',
          keys: 'API 密钥',
          prices: '模型与价格',
          org: '概览',
          billing: /^账务(?:账户)?$/,
          budgets: /^预算(?:与额度)?$/,
          account: '账号设置'
        };
        await expect(page.locator('main h1:visible')).toHaveText(names[route]);
        await noOverflow(page);
        await capture(page, `${surface}-${route}-${width}`);
      }
    });
  }
}
test('tenant usage filter fills the content row and data pages stay left aligned at 1920', async ({
  page
}) => {
  const base = await login(page, 'tenant');
  await navigate(page, base, 'usage');
  const main = page.locator('main'),
    panel = page.getByRole('region', {
      name: '用量分析筛选',
      exact: true
    });
  const narrow = await main.boundingBox();
  const p = await panel.boundingBox();
  expect(Math.abs(p.x - narrow.x), 'filter begins at the main content left edge').toBeLessThanOrEqual(1);
  expect(Math.abs(p.width - narrow.width), 'filter spans the main content width').toBeLessThanOrEqual(1);
  const fields = await panel.locator('input,[role=combobox]').evaluateAll(es => es.filter(e => e.checkVisibility()).map(e => e.getBoundingClientRect().y));
  expect(fields).toHaveLength(4);
  expect(Math.max(...fields) - Math.min(...fields), 'four desktop scope controls share one row').toBeLessThanOrEqual(2);
  await page.setViewportSize({
    width: 1920,
    height: 1000
  });
  for (const route of ['usage', 'calls', 'members', 'keys', 'prices', 'org', 'billing', 'budgets']) {
    await navigate(page, base, route);
    const wide = await main.boundingBox();
    expect(Math.abs(wide.x - narrow.x), `${route} retains the left content edge`).toBeLessThanOrEqual(1);
    expect(Math.abs(wide.width - narrow.width - 480), `${route} uses all newly available desktop width`).toBeLessThanOrEqual(1);
    await noOverflow(page);
  }
  await capture(page, 'tenant-data-full-width-1920');
});
for (const surface of ['admin', 'tenant']) {
  test(`${surface} call filters remain visible during real scroll and the result dropdown reaches real API`, async ({
    page
  }) => {
    const base = await login(page, surface);
    await page.setViewportSize({
      width: 1440,
      height: 400
    });
    await navigate(page, base, 'calls');
    const outcome = page.getByRole('combobox', {
      name: '调用结果',
      exact: true
    });
    await expect(outcome).toBeVisible();
    const scroll = async extra => outcome.evaluate((element, extra) => {
      let root: Element | null = element.parentElement;
      while (root && !(root.scrollHeight > root.clientHeight + 5 && ['auto', 'scroll'].includes(getComputedStyle(root).overflowY))) root = root.parentElement;
      root ??= document.scrollingElement;
      const before = root.scrollTop,
        max = root.scrollHeight - root.clientHeight;
      root.scrollTop = Math.min(max, root.scrollTop + extra);
      return {
        before,
        after: root.scrollTop,
        max
      };
    }, extra);
    const original = (await outcome.boundingBox()).y;
    const first = await scroll(original + 80);
    expect(first.after, 'actual scroll position increases').toBeGreaterThan(first.before + 40);
    await page.evaluate(() => new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r))));
    const sticky = await outcome.boundingBox();
    expect(sticky.y, 'sticky control stays below top navigation').toBeGreaterThanOrEqual(60);
    expect(sticky.y + sticky.height, 'sticky filter is still visible in viewport').toBeLessThanOrEqual(400);
    const second = await scroll(60);
    if (second.after > first.after + 5) {
      await page.evaluate(() => new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r))));
      expect(Math.abs((await outcome.boundingBox()).y - sticky.y), 'sticky position survives further real scroll').toBeLessThanOrEqual(2);
    }
    await outcome.click();
    const list = page.getByRole('listbox');
    await expect(list).toBeVisible();
    const box = await list.boundingBox();
    expect(box.x).toBeGreaterThanOrEqual(0);
    expect(box.y).toBeGreaterThanOrEqual(0);
    expect(box.x + box.width).toBeLessThanOrEqual(1440);
    expect(box.y + box.height).toBeLessThanOrEqual(400);
    await capture(page, surface + '-calls-sticky-dropdown-open', false);
    const response = page.waitForResponse(r => {
      const u = new URL(r.url());
      return r.request().method() === 'GET' && u.pathname === (surface === 'admin' ? '/admin/pricing/charges' : '/api/usage/calls') && u.searchParams.get('outcome') === 'failed';
    });
    await list.getByRole('option', {
      name: '失败',
      exact: true
    }).click();
    const actual = await response;
    expect(actual.status()).toBe(200);
    const body = await actual.json();
    expect(body.items.length, 'retained real failure exists').toBeGreaterThan(0);
    await settle(page);
    const table = page.getByRole('table', {
      name: surface === 'admin' ? '计费明细' : '调用记录',
      exact: true
    });
    await expect(table.locator('tbody tr')).toHaveCount(body.items.length);
    await test.info().attach('real-scroll-and-outcome', {
      body: JSON.stringify({
        first,
        second,
        original,
        sticky,
        query: new URL(actual.url()).search,
        rows: body.items.length
      }),
      contentType: 'application/json'
    });
    await capture(page, surface + '-calls-sticky-real-failure', false);
    await noOverflow(page);
  });
}
test('tenant unknown request filter has a real empty state and clearing restores real rows', async ({
  page
}) => {
  const base = await login(page, 'tenant');
  await navigate(page, base, 'calls');
  const table = page.getByRole('table', {
    name: '调用记录',
    exact: true
  });
  const original = await table.locator('tbody tr').count();
  expect(original).toBeGreaterThan(0);
  const input = page.getByLabel('请求 ID', {
    exact: true
  });
  const requestId = '452-no-such-request-908e40f3-72f6-49df-a496-69fb9acbb702';
  const result = page.waitForResponse(r => new URL(r.url()).pathname === '/api/usage/calls' && new URL(r.url()).searchParams.get('request_id') === requestId);
  await input.fill(requestId);
  await input.press('Enter');
  const response = await result;
  expect(response.status()).toBe(200);
  expect((await response.json()).items).toEqual([]);
  await settle(page);
  await expect(page.getByText('所选条件下没有调用记录', {
    exact: true
  })).toBeVisible();
  await expect(input).toBeVisible();
  await capture(page, 'tenant-calls-real-empty-state');
  const restored = page.waitForResponse(r => new URL(r.url()).pathname === '/api/usage/calls' && !new URL(r.url()).searchParams.has('request_id'));
  await input.fill('');
  await input.press('Enter');
  expect((await restored).status()).toBe(200);
  await settle(page);
  await expect(table.locator('tbody tr')).toHaveCount(original);
});
test('admin analytics scope follows the heading and spans the full left-aligned row', async ({
  page
}) => {
  const base = await login(page, 'admin');
  await navigate(page, base, 'analytics');
  const main = await page.locator('main').boundingBox();
  const scope = page.locator('main .metric-scope');
  await expect(scope).toHaveCount(1);
  const box = await scope.boundingBox(),
    heading = await page.locator('main h1').boundingBox();
  await test.info().attach('admin-analytics-scope-geometry', {
    body: JSON.stringify({
      main,
      scope: box,
      heading
    }),
    contentType: 'application/json'
  });
  expect.soft(Math.abs(box.x - main.x), 'scope begins at the same left edge as the page heading').toBeLessThanOrEqual(1);
  expect.soft(Math.abs(box.width - main.width), 'scope fills its own full content row').toBeLessThanOrEqual(1);
  expect(box.y, 'scope appears below the page heading').toBeGreaterThan(heading.y + heading.height);
  await noOverflow(page);
  await capture(page, 'admin-analytics-full-width-1440');
});

// oracle 是真实保留事件的只读原值快照；没有导入产品 formatter，也没有摄入计数。
const totalsOracle = () => JSON.parse(readFileSync(join(session, 'layout-oracle.json'), 'utf8'));
function detailValue(region, label) {
  return region.locator('dt').and(region.getByText(label, {
    exact: true
  })).locator('xpath=following-sibling::dd[1]');
}
for (const width of [1440, 1280, 390, 1920]) {
  test(`admin total revenue and unknown cost match retained real charges and both APIs at ${width}`, async ({ page }) => {
    await page.setViewportSize({ width, height: 1000 });
    const base = await login(page, 'admin'), oracle = totalsOracle();
    expect(oracle.revenueMicroCny, 'read-only original money snapshot is positive').toBe('79');
    expect(oracle.missingCostRows).toBe(2); expect(oracle.knownCostRows).toBe(0);
    const query = new URLSearchParams(oracle.window);
    await navigate(page, base, 'totals?' + query);
    await expect(page.locator('main h1')).toHaveText('总收入与总成本');
    const [metrics, margin, charges] = await Promise.all([
      get(page, '/admin/usage/metrics?' + query),
      get(page, '/admin/pricing/margin?' + query),
      get(page, '/admin/pricing/charges?' + query + '&limit=50'),
    ]);
    for (const key of ['charged_micro_cny', 'adjusted_micro_cny', 'billing_attempts', 'unpriced_attempts']) {
      expect(Object.hasOwn(metrics.totals, key), key + ' exists on wire').toBe(true);
    }
    expect(typeof metrics.totals.charged_micro_cny).toBe('string');
    expect(typeof metrics.totals.adjusted_micro_cny).toBe('string');
    expect(BigInt(metrics.totals.charged_micro_cny)).toBe(79n);
    expect(BigInt(metrics.totals.adjusted_micro_cny)).toBe(BigInt(oracle.netAdjustmentsMicroCny));
    expect(metrics.totals.billing_attempts).toBe(5);
    expect(metrics.totals.unpriced_attempts).toBe(2);
    expect(charges.items.map(row => row.event_id).sort()).toEqual(oracle.originalCharges.map(row => row.event_id).sort());
    const originalBase = oracle.originalCharges.reduce((sum, row) => sum + BigInt(row.charge_micro_cny ?? 0), 0n);
    const originalAdjustments = oracle.originalAdjustments.reduce((sum, row) => sum + BigInt(row.delta_micro_cny), 0n);
    const apiBase = charges.items.reduce((sum, row) => sum + BigInt(row.charge_micro_cny ?? 0), 0n);
    expect(originalBase).toBe(79n); expect(originalAdjustments).toBe(0n); expect(apiBase).toBe(originalBase);
    expect(originalBase + originalAdjustments, 'original charge snapshots plus net deltas independently reproduce income').toBe(BigInt(metrics.totals.charged_micro_cny));
    expect(margin.items.length).toBeGreaterThan(0);
    for (const item of margin.items) for (const key of ['cost_micro_cny', 'rows', 'rows_without_cost']) {
      expect(Object.hasOwn(item, key), key + ' exists on wire').toBe(true);
    }
    const known = margin.items.reduce((sum, item) => sum + BigInt(item.rows), 0n);
    const missing = margin.items.reduce((sum, item) => sum + BigInt(item.rows_without_cost), 0n);
    const cost = margin.items.reduce((sum, item) => sum + BigInt(item.cost_micro_cny), 0n);
    expect(known).toBe(0n); expect(missing).toBe(2n); expect(cost).toBe(0n);
    const revenue = page.getByRole('region', { name: '总收入统计', exact: true });
    const costRegion = page.getByRole('region', { name: '总成本统计', exact: true });
    await expect(revenue.getByRole('heading', { name: '调用收入', exact: true })).toBeVisible();
    await expect(revenue.locator('.metric')).toHaveText('¥0.000079');
    await expect(detailValue(revenue, '计费尝试')).toHaveText('5');
    await expect(detailValue(revenue, '待计价尝试')).toHaveText('2');
    await expect(detailValue(revenue, '已含费用调整')).toHaveText('¥0.00');
    await expect(costRegion.locator('.metric')).toHaveText('—');
    await expect(costRegion.getByText('¥0.00', { exact: true })).toHaveCount(0);
    await expect(detailValue(costRegion, '成本缺失的记录')).toHaveText('2');
    await expect(detailValue(costRegion, '成本已知的记录')).toHaveText('0');
    await expect(costRegion.getByText('2 笔记录缺失成本，当前仅为已知成本小计，不代表完整总成本。', { exact: true })).toBeVisible();
    await noOverflow(page);
    if (width === 1920) {
      const main = await page.locator('main').boundingBox(); expect(main.x).toBe(268); expect(main.x + main.width).toBe(1904);
    }
    await test.info().attach('real-revenue-and-cost-oracles', {
      body: JSON.stringify({ window: oracle.window, chargedMicroCny: metrics.totals.charged_micro_cny, originalBase: originalBase.toString(), originalAdjustments: originalAdjustments.toString(), knownCostRows: known.toString(), missingCostRows: missing.toString() }),
      contentType: 'application/json',
    });
    await capture(page, 'admin-totals-real-income-and-unknown-cost-' + width);
  });
}
test('admin total empty window is zero and changing the preset keeps both aggregate windows identical', async ({
  page
}) => {
  const base = await login(page, 'admin');
  const query = new URLSearchParams({
    from: '2026-10-01T00:00:00Z',
    to: '2026-10-02T00:00:00Z'
  });
  await navigate(page, base, 'totals?' + query);
  const [metrics, margin] = await Promise.all([get(page, '/admin/usage/metrics?' + query), get(page, '/admin/pricing/margin?' + query)]);
  expect(Object.hasOwn(metrics.totals, 'charged_micro_cny')).toBe(true);
  expect(typeof metrics.totals.charged_micro_cny).toBe('string');
  expect(BigInt(metrics.totals.charged_micro_cny)).toBe(0n);
  expect(metrics.totals.billing_attempts).toBe(0);
  expect(margin.items).toEqual([]);
  await expect(page.getByRole('region', {
    name: '总收入统计',
    exact: true
  }).locator('.metric')).toHaveText('¥0.00');
  await expect(page.getByRole('region', {
    name: '总成本统计',
    exact: true
  }).locator('.metric')).toHaveText('¥0.00');
  await capture(page, 'admin-totals-real-empty-window');
  const responses = Promise.all(['/admin/usage/metrics', '/admin/pricing/margin'].map(path => page.waitForResponse(r => new URL(r.url()).pathname === path && r.request().method() === 'GET')));
  await page.getByRole('button', {
    name: '最近 7 天',
    exact: true
  }).click();
  const answers = await responses;
  for (const answer of answers) expect(answer.status()).toBe(200);
  const scopes = answers.map(r => {
    const q = new URL(r.url()).searchParams;
    return {
      from: q.get('from'),
      to: q.get('to')
    };
  });
  expect(scopes[0]).toEqual(scopes[1]);
  expect(new Date(scopes[0].to).getTime() - new Date(scopes[0].from).getTime()).toBe(7 * 24 * 60 * 60 * 1000);
  await settle(page);
  await noOverflow(page);
  expect(health.get(page).responses.filter(x => x.path === '/admin/pricing/charges'), 'total statistics does not page charge rows').toEqual([]);
});
test('tenant navigation and cookie cannot reach operator cost statistics', async ({ page }) => {
  const base = await login(page, 'tenant');
  await navigate(page, base, 'usage');
  await expect(page.getByRole('link', { name: '总量统计', exact: true })).toHaveCount(0);
  const session = await get(page, '/api/session');
  expect(session.authed, 'positive control is a live browser tenant session').toBe(true);
  expect(session.tenant.id).toBe(auth.metricsTenantId);
  const path = '/admin/pricing/margin?from=2026-11-15T00:00:00Z&to=2026-11-18T00:00:00Z';
  const ledger = health.get(page);
  ledger.expectedErrors = ['Failed to load resource: the server responded with a status of 401 (Unauthorized)'];
  ledger.expectedFailures = [{ status: 401, path: '/admin/pricing/margin' }];
  const tenantStatus = await page.evaluate(async path => (await fetch(path, { credentials: 'same-origin' })).status, path);
  expect(tenantStatus, 'the actual browser tenant cookie is not an operator session').toBe(401);

  // 同一浏览器的真实管理员对照会被上面的401断言拒绝，防止无cookie请求造成假绿。
  const admin = await page.context().newPage();
  try {
    await login(admin, 'admin');
    const adminStatus = await admin.evaluate(async path => (await fetch(path, { credentials: 'same-origin' })).status, path);
    expect(adminStatus, 'a real operator browser session reaches the same cost API').toBe(200);
    expect(adminStatus).not.toBe(tenantStatus);
    expect(health.get(admin).errors).toEqual([]);
    expect(health.get(admin).writes).toEqual([]);
    expect(health.get(admin).forbidden).toEqual([]);
    expect(health.get(admin).responses.filter(response => response.status >= 400)).toEqual([]);
    await test.info().attach('actual-browser-cookie-controls', {
      body: JSON.stringify({ tenantId: session.tenant.id, tenantSessionAuthed: session.authed, tenantStatus, adminStatus }),
      contentType: 'application/json',
    });
  } finally {
    await admin.close();
  }
});

test('tenant reversed date range displays the real validation error and valid dates recover the distribution', async ({
  page
}) => {
  await page.setViewportSize({
    width: 390,
    height: 1000
  });
  const base = await login(page, 'tenant');
  await navigate(page, base, 'usage');
  const ledger = health.get(page);
  ledger.expectedErrors = ['Failed to load resource: the server responded with a status of 422 (Unprocessable Entity)'];
  ledger.expectedFailures = [{
    status: 422,
    path: '/api/usage/summary'
  }];
  const invalid = page.waitForResponse(r => new URL(r.url()).pathname === '/api/usage/summary' && r.status() === 422);
  await page.getByLabel('开始日期', {
    exact: true
  }).fill('2026-11-18');
  await page.getByLabel('结束日期', {
    exact: true
  }).fill('2026-11-16');
  await page.getByRole('button', {
    name: '刷新分析',
    exact: true
  }).click();
  expect((await invalid).status()).toBe(422);
  await expect(page.getByRole('alert')).toHaveText('结束日期不能早于开始日期，请调整汇总筛选。');
  await noOverflow(page);
  await capture(page, 'tenant-usage-real-validation-error-390');
  const recovered = Promise.all(['/api/usage/summary', '/api/usage/metrics'].map(path => page.waitForResponse(r => new URL(r.url()).pathname === path && r.status() === 200)));
  await page.getByLabel('开始日期', {
    exact: true
  }).fill('2026-11-15');
  await page.getByLabel('结束日期', {
    exact: true
  }).fill('2026-11-17');
  await page.getByRole('button', {
    name: '刷新分析',
    exact: true
  }).click();
  for (const response of await recovered) expect(response.status()).toBe(200);
  await settle(page);
  await expect(page.getByRole('alert')).toHaveCount(0);
  const distribution = page.locator('section').filter({
    has: page.getByRole('heading', {
      name: '模型请求分布',
      exact: true
    })
  }).last();
  await expect(distribution).toContainText('qa435-468d9708-model');
  await expect(page.getByRole('region', {
    name: '完整 API 指标',
    exact: true
  })).toHaveAttribute('aria-busy', 'false');
  await noOverflow(page);
  await capture(page, 'tenant-usage-recovered-distribution-390');
});

for (const surface of ["admin", "tenant"]) {
  test(`${surface} mobile result dropdown stays in the viewport and reaches the real failure`, async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 1000 });
    const base = await login(page, surface);
    await navigate(page, base, "calls");
    const outcome = page.getByRole("combobox", { name: "调用结果", exact: true });
    await outcome.click();
    const list = page.getByRole("listbox");
    await expect(list).toBeVisible();
    const box = await list.boundingBox();
    expect(box.x).toBeGreaterThanOrEqual(0);
    expect(box.x + box.width).toBeLessThanOrEqual(390);
    expect(box.y).toBeGreaterThanOrEqual(0);
    expect(box.y + box.height).toBeLessThanOrEqual(1000);
    await capture(page, `${surface}-calls-mobile-dropdown-open`, false);
    const selected = page.waitForResponse(response => {
      const url = new URL(response.url());
      return url.pathname === (surface === "admin" ? "/admin/pricing/charges" : "/api/usage/calls") && url.searchParams.get("outcome") === "failed";
    });
    await list.getByRole("option", { name: "失败", exact: true }).click();
    const response = await selected;
    expect(response.status()).toBe(200);
    const rows = (await response.json()).items;
    expect(rows.length).toBeGreaterThan(0);
    await settle(page);
    const table = page.getByRole("table", { name: surface === "admin" ? "计费明细" : "调用记录", exact: true });
    await expect(table.locator("tbody tr")).toHaveCount(rows.length);
    if (surface === "tenant") {
      const details = table.getByRole("button", { name: "查看调用详情", exact: true }).first();
      await details.click();
      const drawer = page.getByRole("dialog", { name: "调用详情", exact: true });
      await expect(drawer).toBeVisible();
      await expect.poll(async () => {
        const rect = await drawer.boundingBox();
        return rect.x >= 0 && rect.x + rect.width <= 390;
      }).toBe(true);
      await capture(page, "tenant-call-real-failure-drawer-390", false);
      await page.keyboard.press("Escape");
      await expect(drawer).toHaveCount(0);
      await expect(details).toBeFocused();
    }
    await noOverflow(page);
  });
}
