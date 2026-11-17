import { expect, test, type Page } from "@playwright/test";
import { spawn, type ChildProcess } from "node:child_process";
import { createServer } from "node:net";
import { mkdirSync, openSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { startStack, STACK_START_TIMEOUT_MS, type Stack } from "../e2e/stack";
import { seedTenant, chat, tenantClient, type BillingWorld } from "../e2e/world";
import { chooseSelect } from "../e2e/select";
import { observe, health, login, go, shot, noOverflow, contained, get, value, money, performance, performanceValues, metricViews, allowWrite, output } from "./frontend-unified-456.helpers";

// 用户要求及 owner 的可访问合同先于实现；金额由授权上游 5×1000+3×2000 独立计算。
let stack: Stack;
let world: BillingWorld;
let extraGateway: ChildProcess | undefined;
const views = [ ["basic","基本与限流"],["access","模型与分组"],["members","成员"],["keys","密钥"],["billing","余额与计费"],["quotas","额度"],["discounts","折扣记录"],["activity","账务记录"] ];
const detail = (view: string, from = "tenants") => `tenants?tenant=${world.tenantId}&view=${view}&from=${from}`;
async function port(): Promise<number> { return new Promise((resolvePort,reject)=>{const server=createServer();server.once("error",reject);server.listen(0,"127.0.0.1",()=>{const a=server.address();if(!a||typeof a==="string")return reject(new Error("No test port"));const n=a.port;server.close(()=>resolvePort(n));});}); }
async function admin(page: Page): Promise<void> { await login(page,stack.previewUrl,stack.operator,"/admin"); }
async function keyStatus(plaintext: string): Promise<number> { const r=await fetch(stack.proxyUrl+"/v1/models",{headers:{authorization:`Bearer ${plaintext}`}});await r.arrayBuffer();return r.status; }

test.beforeAll(async()=>{
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack=await startStack();
  world=await seedTenant(stack,{unpricedModel:true,members:["member"]});
  expect(await chat(stack,world.key.plaintext,world.model.name)).toBe(200);
  expect(await chat(stack,world.key.plaintext,world.laterModel!.name)).toBe(200);
  await expect.poll(async()=>{const items=(await stack.admin.get(`/admin/tenants/${world.tenantId}/billing/ledger`)).items;return items.length >= 2;},{timeout:30_000}).toBe(true);
  // 第二个网关使用正式签发的部署环境启动，不伪造注册或心跳。
  const issued=await stack.admin.post("/admin/gateways/credentials",{name:"aaa-newer-456"});
  const dir=join(output!,"second-gateway");mkdirSync(dir,{recursive:true});
  const env=Object.fromEntries(Object.entries(process.env).filter(([k,v])=>v!==undefined&&!k.startsWith("AISIX_")&&!k.startsWith("PLATFORM_"))) as Record<string,string>;
  for(const [k,v] of Object.entries(issued.env))if(k!=="AISIX_CONFIG_PATH")env[k]=String(v);
  Object.assign(env,{AISIX_MANAGED__MTLS_DIR:join(dir,"mtls"),AISIX_MANAGED__DP_ID_FILE:join(dir,"dp-id"),AISIX_MANAGED__SNAPSHOT_CACHE_PATH:join(dir,"snapshot.json"),AISIX_MANAGED__HEARTBEAT_INTERVAL_SECS:"1",AISIX_PROXY__ADDR:`127.0.0.1:${await port()}`,AISIX_OBSERVABILITY__METRICS__PROMETHEUS__ADDR:`127.0.0.1:${await port()}`,AISIX_OBSERVABILITY__DEBUG__ENABLED:"false",AISIX_SHUTDOWN__MIN_DRAIN_SECS:"0"});
  const fd=openSync(join(dir,"gateway.log"),"a",0o600);
  extraGateway=spawn(resolve(process.env.CP_E2E_GATEWAY_BIN!),["--config",resolve("../config.managed.yaml")],{env,stdio:["ignore",fd,fd]});
  await expect.poll(async()=>(await stack.admin.get("/admin/gateways/instances")).items.length,{timeout:45_000}).toBe(2);
  writeFileSync(join(output!,"environment-public.json"),JSON.stringify({realBackend:true,authorizedUpstreamFixture:true,adminUrl:stack.previewUrl,httpUrl:stack.httpUrl,cpDatabase:stack.cpDatabase,realGatewayInstances:2,tenantId:world.tenantId}));
});
test.afterAll(async()=>{if(extraGateway&&extraGateway.exitCode===null){extraGateway.kill("SIGTERM");await Promise.race([new Promise<void>(r=>extraGateway!.once("exit",()=>r())),new Promise<void>(r=>setTimeout(r,10_000))]);if(extraGateway.exitCode===null&&extraGateway.signalCode===null)extraGateway.kill("SIGKILL");}await stack?.stop();});
test.beforeEach(({page})=>observe(page));
test.afterEach(async({page},info)=>{await shot(page,"cp-"+info.title.replace(/[^a-zA-Z0-9]+/g,"-"));await health(page);});

test("01-billing-opens-independent-tenant-detail",async({page})=>{
  await admin(page);await go(page,stack.previewUrl,"billing");
  await page.getByRole("table",{name:"账户列表",exact:true}).getByRole("link",{name:world.tenantName,exact:true}).click();await settleDetail(page,"billing","billing");
  await expect(page.getByRole("dialog")).toHaveCount(0);
  await expect(page.getByRole("table",{name:"账户列表",exact:true})).toHaveCount(0);
  await expect(page.getByRole("region",{name:`租户详情：${world.tenantName}`,exact:true})).toBeVisible();
  await page.getByRole("button",{name:"返回账户列表",exact:true}).click();
  await expect(page.getByRole("table",{name:"账户列表",exact:true})).toBeVisible();
});
async function settleDetail(page: Page, view: string, from: string): Promise<void> {
  await expect(page.locator("main h1")).toHaveText(world.tenantName);
  const params=new URLSearchParams(new URL(page.url()).hash.split("?")[1]);expect(params.get("tenant")).toBe(world.tenantId);expect(params.get("view")).toBe(view);expect(params.get("from")).toBe(from);
}
test("02-eight-tenant-views-survive-refresh-and-back",async({page})=>{
  await admin(page);await go(page,stack.previewUrl,detail("basic"));
  const nav=page.getByRole("navigation",{name:"租户设置",exact:true});await expect(nav.getByRole("link")).toHaveCount(8);
  for(const [view,label] of views){const link=nav.getByRole("link",{name:label,exact:true});await link.click();await settleDetail(page,view,"tenants");await expect(link).toHaveAttribute("aria-current","page");await page.reload();await settleDetail(page,view,"tenants");}
  await page.goBack();await expect(page.locator("main h1")).toHaveText(world.tenantName);await noOverflow(page);
  await go(page,stack.previewUrl,"tenants");const entry=page.getByRole("table",{name:"租户列表",exact:true}).getByRole("link",{name:world.tenantName,exact:true});await entry.click();await settleDetail(page,"basic","tenants");await page.getByRole("button",{name:"返回租户列表",exact:true}).click();await expect(entry).toBeFocused();await entry.click();await page.goBack();await expect(entry).toBeFocused();
});
test("03-pricing-tenant-entry-converges-on-discounts",async({page})=>{
  await stack.admin.post("/admin/pricing/discounts",{tenant_id:world.tenantId,discount_bps:1000,effective_from:new Date(Date.now()+3_600_000).toISOString()});
  await admin(page);await go(page,stack.previewUrl,"pricing?section=plans");
  const table=page.getByRole("table",{name:"租户折扣",exact:true});await table.getByRole("link",{name:world.tenantName,exact:true}).click();
  await settleDetail(page,"discounts","pricing");await expect(page.getByRole("navigation",{name:"租户设置"}).getByRole("link",{name:"折扣记录",exact:true})).toHaveAttribute("aria-current","page");
  await page.getByRole("button",{name:"返回租户方案",exact:true}).click();await expect(table).toBeVisible();
  await page.getByRole("button",{name:"新租户折扣",exact:true}).click();const picker=page.getByRole("dialog",{name:"新租户折扣",exact:true});await expect(picker.getByRole("button",{name:"前往设置折扣",exact:true})).toBeDisabled();await picker.getByLabel("搜索租户",{exact:true}).fill(world.tenantName);await chooseSelect(picker.getByRole("combobox",{name:"租户",exact:true}),world.tenantId);await picker.getByRole("button",{name:"前往设置折扣",exact:true}).click();await settleDetail(page,"discounts","pricing");expect(new URLSearchParams(new URL(page.url()).hash.split("?")[1]).get("action")).toBeNull();const form=page.getByRole("dialog",{name:"新折扣",exact:true});await expect(form).toBeVisible();await expect(form).toContainText(world.tenantName);await expect(form.getByRole("combobox",{name:"租户",exact:true})).toHaveCount(0);await shot(page,"cp-discount-picker-arrives-at-hub");await page.keyboard.press("Escape");await expect(form).toHaveCount(0);await page.reload();await settleDetail(page,"discounts","pricing");await expect(page.getByRole("dialog")).toHaveCount(0);
});
test("04-key-icons-confirm-and-persist-real-state",async({page})=>{
  const isolated=await seedTenant(stack);const api=await tenantClient(stack,isolated);const minted=await api.post("/api/keys",{label:"state-proof-456 · 研发团队生产与灰度服务共用网关凭据长名称可读性检查"});
  await expect.poll(()=>keyStatus(minted.plaintext),{timeout:30_000}).toBe(200);
  await admin(page);await go(page,stack.previewUrl,"keys");await chooseSelect(page.getByRole("combobox",{name:"租户",exact:true}),{label:isolated.tenantName});
  const row=page.getByRole("table",{name:"密钥列表",exact:true}).getByRole("row").filter({hasText:"state-proof-456"});
  for(const name of ["停用","吊销"]){const action=row.getByRole("button",{name,exact:true});await expect(action).toHaveText("");await expect(action.locator("svg")).toHaveCount(1);await expect(action).toHaveAttribute("title",new RegExp(name));await action.hover();}
  await row.getByRole("button",{name:"停用",exact:true}).click();expect((await stack.admin.get(`/admin/keys/${minted.key.id}`)).disabled).toBe(false);
  await row.getByRole("button",{name:"取消",exact:true}).click();expect((await stack.admin.get(`/admin/keys/${minted.key.id}`)).disabled).toBe(false);
  await row.getByRole("button",{name:"停用",exact:true}).click();allowWrite(page,"PATCH",`/admin/keys/${minted.key.id}`);await row.getByRole("button",{name:"确认停用",exact:true}).click();
  await expect.poll(()=>keyStatus(minted.plaintext),{timeout:15_000}).toBe(401);expect((await api.get(`/api/keys/${minted.key.id}`)).disabled_by_platform).toBe(true);
  await row.getByRole("button",{name:"启用",exact:true}).click();await expect.poll(()=>keyStatus(minted.plaintext),{timeout:15_000}).toBe(200);
  await row.getByRole("button",{name:"吊销",exact:true}).click();allowWrite(page,"DELETE",`/admin/keys/${minted.key.id}`);await row.getByRole("button",{name:"确认吊销",exact:true}).click();await expect(row).toHaveCount(0);await expect.poll(()=>keyStatus(minted.plaintext),{timeout:15_000}).toBe(401);await noOverflow(page);
});
test("05-credentials-precede-two-real-instances-newest-first",async({page})=>{
  await admin(page);await go(page,stack.previewUrl,"gateways");
  const credentials=page.getByRole("region",{name:"网关凭据",exact:true}).getByRole("heading",{name:"网关凭据 2",exact:true});const instances=page.getByRole("heading",{name:"网关实例",exact:true});
  await expect(credentials).toBeVisible();await expect(instances).toBeVisible();expect((await credentials.boundingBox())!.y).toBeLessThan((await instances.boundingBox())!.y);
  const table=page.getByRole("table",{name:"网关实例",exact:true});await expect(table.locator("tbody tr")).toHaveCount(2);
  await expect(table.locator("tbody tr").first()).toContainText("aaa-newer-456");await expect(table.locator("tbody tr").last()).toContainText("stack-gw");
  await page.setViewportSize({width:390,height:1000});await noOverflow(page);await contained(table);await shot(page,"cp-gateways-390");
});
test("06-totals-width-figures-and-unknown-money",async({page})=>{
  await admin(page);await go(page,stack.previewUrl,"totals");
  const data=await get(page,"/admin/billing/profit-summary");expect(data.totals.charged_micro_cny).toBe("11000");expect(BigInt(data.totals.rows_without_cost)).toBeGreaterThan(0n);
  const revenue=page.getByRole("region",{name:"总收入统计",exact:true}),cost=page.getByRole("region",{name:"总成本统计",exact:true}),profit=page.getByRole("region",{name:"利润率统计",exact:true});
  await expect(revenue).toContainText("¥0.011");await expect(profit.locator(".metric")).toHaveText("—");
  for(const width of [1920,1440,390]){await page.setViewportSize({width,height:1000});const boxes=await Promise.all([revenue,cost,profit].map(x=>x.boundingBox()));expect(Math.max(...boxes.map(b=>b!.width))-Math.min(...boxes.map(b=>b!.width))).toBeLessThanOrEqual(2);await expect(page.getByRole("figure",{name:"收入、成本与完整利润金额对照",exact:true})).toBeVisible();await expect(page.getByRole("figure",{name:"已计费记录的成本覆盖",exact:true})).toBeVisible();await noOverflow(page);await shot(page,"cp-totals-"+width);}
});
test("07-overview-light-price-action-and-global-card-sweep",async({page})=>{
  await admin(page);await go(page,stack.previewUrl,"overview");const action=page.getByRole("link",{name:"计价状态",exact:true});await expect(action).toHaveAttribute("href","#pricing?section=status");expect((await action.boundingBox())!.height).toBeLessThanOrEqual(44);await action.click();await expect(page.locator("main h1")).toBeVisible();
  for(const route of ["overview","analytics","totals","billing","pricing","keys","gateways",detail("billing"),detail("access"),detail("activity")]){await go(page,stack.previewUrl,route);await expect(page.locator("main h1")).toHaveCount(1);await expect(page.locator("main .card .card,main [data-slot=card] [data-slot=card]")).toHaveCount(0);await noOverflow(page);}
});
test("08-admin-summary-and-table-columns-fit-desktop-mobile",async({page})=>{
  await admin(page);
  for(const width of [1440,390]){await page.setViewportSize({width,height:1000});for(const route of ["analytics","keys","billing",detail("activity")]){await go(page,stack.previewUrl,route);if(route==="analytics")await metricViews(page,"cp-analytics-"+width);for(const table of await page.getByRole("table").all())await contained(table);await noOverflow(page);await shot(page,`cp-${route.split("?")[0]}-columns-${width}`);}}
});
test("09-calls-keep-labelled-performance-and-exact-charge",async({page})=>{
  await admin(page);await go(page,stack.previewUrl,`calls?tenant_id=${world.tenantId}`);const table=page.getByRole("table",{name:"计费明细",exact:true});await performance(table);
  const rows=await get(page,`/admin/pricing/charges?tenant_id=${world.tenantId}&limit=200`);const calls=[];for(const charge of rows.items){const stored=await get(page,`/admin/usage/events/${charge.event_id}`);expect(stored.event_id).toBe(charge.event_id);expect(Object.hasOwn(stored,"request_id")).toBe(true);expect(typeof stored.request_id).toBe("string");calls.push({...charge,request_id:stored.request_id});}await test.info().attach("real-charge-and-performance-oracle",{body:JSON.stringify(calls.map(x=>({event_id:x.event_id,request_id:x.request_id,charge_micro_cny:x.charge_micro_cny,first_token_latency_ms:x.first_token_latency_ms,attempt_duration_ms:x.attempt_duration_ms,throughput_tps:x.throughput_tps,usage:x.usage}))),contentType:"application/json"});await performanceValues(table,calls,true);expect(rows.items.filter((x:any)=>x.status==="charged").map((x:any)=>x.charge_micro_cny)).toEqual([11000]);await expect(table).toContainText(money(11000));await noOverflow(page);
});
