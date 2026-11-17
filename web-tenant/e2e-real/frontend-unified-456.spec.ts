import { expect, test, type Page } from "@playwright/test";
import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { startStack, STACK_START_TIMEOUT_MS, type Stack, type ApiClient } from "../e2e/stack";
import { seedBillingWorld, memberKey, chat, type BillingWorld, type Login } from "../e2e/world";
import { observe, health, login, go, shot, noOverflow, contained, get, metricValue, money, performance, performanceValues, noTextOverlap, allowFailure, output } from "./frontend-unified-456.helpers";

// 预期来自用户场景和可访问合同；不读取产品源码，不替换响应，不保存口令或密钥。
let stack: Stack;
let world: BillingWorld;
let ownerApi: ApiClient;
let pendingEmail: string;
const fixtureCalls = 5;
async function signed(actor: Login): Promise<ApiClient> { const api=stack.tenant();await api.post("/api/login",actor);return api; }
async function owner(page: Page): Promise<void> { await login(page,stack.previewUrl,world.owner,"/api"); }
async function admits(plaintext: string): Promise<number> {const r=await fetch(stack.proxyUrl+"/v1/models",{headers:{authorization:`Bearer ${plaintext}`}});await r.arrayBuffer();return r.status;}

test.beforeAll(async()=>{
  test.setTimeout(STACK_START_TIMEOUT_MS);
  stack=await startStack();
  world=await seedBillingWorld(stack,{unpricedModel:true,members:["member","member","billing"]});ownerApi=await signed(world.owner);
  const alice=await memberKey(stack,world,world.members[0]!);const bob=await memberKey(stack,world,world.members[1]!);
  await expect.poll(()=>admits(alice.plaintext),{timeout:30_000}).toBe(200);await expect.poll(()=>admits(bob.plaintext),{timeout:30_000}).toBe(200);
  expect(await chat(stack,world.key.plaintext,world.model.name)).toBe(200);
  for(let n=0;n<3;n++)expect(await chat(stack,alice.plaintext,world.model.name)).toBe(200);
  expect(await chat(stack,bob.plaintext,world.model.name)).toBe(200);
  expect(await chat(stack,world.key.plaintext,world.laterModel!.name)).toBe(200);
  await expect.poll(async()=>{const items=(await ownerApi.get("/api/usage/calls?limit=200")).items;const charged=items.filter((x:any)=>x.charge.status==="charged"),pending=items.filter((x:any)=>x.charge.status==="pending");return items.length===6&&charged.length===5&&pending.length===1&&charged.every((x:any)=>x.charge.amount_micro_cny===11000);},{timeout:30_000}).toBe(true);
  const stored=await ownerApi.request("GET",`/api/keys/${bob.id}`);await ownerApi.patch(`/api/keys/${bob.id}`,{disabled:true},stored.version!);
  pendingEmail=`pending-${world.tenantId.slice(0,8)}@unified456.test`;await stack.admin.post(`/admin/tenants/${world.tenantId}/invites`,{email:pendingEmail,role:"member"});
  writeFileSync(join(output!,"environment-public.json"),JSON.stringify({realBackend:true,authorizedUpstreamFixture:true,tenantUrl:stack.previewUrl,httpUrl:stack.httpUrl,tenantId:world.tenantId,expected:{members:4,pendingInvites:1,keys:3,availableKeys:2,chargedMicroCny:55000,totalCalls:6}}));
});
test.afterAll(async()=>{await stack?.stop();});
test.beforeEach(({page})=>observe(page));
test.afterEach(async({page},info)=>{await shot(page,"tenant-"+info.title.replace(/[^a-zA-Z0-9]+/g,"-"));await health(page);});

test("10-login-signup-password-manager-semantics-and-real-DOM-submit",async({page})=>{
  await page.goto(stack.previewUrl);
  const form=page.getByRole("form",{name:"登录",exact:true});await expect(form).toHaveAttribute("method","post");
  const email=form.getByLabel("邮箱",{exact:true}),password=form.getByLabel("口令",{exact:true});
  await expect(email).toHaveAttribute("type","email");await expect(email).toHaveAttribute("autocomplete","username");await expect(password).toHaveAttribute("autocomplete","current-password");
  for(const input of [email,password]){expect(await input.getAttribute("id")).toBeTruthy();expect(await input.getAttribute("name")).toBeTruthy();}
  // 直接更新 DOM 模拟密码管理器填值；不触发 React 的 input/change 事件。
  await email.evaluate((el,v)=>(el as HTMLInputElement).value=v,world.owner.email);
  await password.evaluate((el,v)=>(el as HTMLInputElement).value=v,world.owner.password);
  const answer=page.waitForResponse(r=>new URL(r.url()).pathname==="/api/login"&&r.request().method()==="POST");await form.getByRole("button",{name:"登录",exact:true}).click();expect((await answer).status()).toBe(200);await expect(form).toHaveCount(0);await expect(page.locator("main")).toBeVisible();
  const fresh=await page.context().browser()!.newContext();const signup=await fresh.newPage();try{await signup.goto(stack.previewUrl);await signup.getByRole("tab",{name:"注册组织",exact:true}).click();const register=signup.getByRole("form",{name:"注册组织",exact:true});await expect(register).toHaveAttribute("method","post");await expect(register.getByLabel("邮箱",{exact:true})).toHaveAttribute("autocomplete","username");await expect(register.getByLabel("口令",{exact:true})).toHaveAttribute("autocomplete","new-password");await expect(register.getByRole("button",{name:"注册组织",exact:true})).toHaveAttribute("type","submit");await shot(signup,"tenant-signup-standard-autocomplete");}finally{await fresh.close();}
});
test("11-portal-has-1440-content-cap-and-usable-mobile-shell",async({page})=>{
  await owner(page);
  for(const width of [1920,1440,390]){await page.setViewportSize({width,height:1000});for(const route of ["org","keys","prices","billing","members","usage","calls"]){await go(page,stack.previewUrl,route);const main=page.locator("#workspace-main"),toolbar=page.locator("header.shell-view-header");await expect(main).toBeVisible();const m=await main.boundingBox(),h=await toolbar.boundingBox();expect(m!.width).toBeLessThanOrEqual(1441);expect(Math.abs(m!.x-h!.x)).toBeLessThanOrEqual(2);expect(Math.abs(m!.width-h!.width)).toBeLessThanOrEqual(2);await noOverflow(page);}await shot(page,"tenant-shell-"+width);}
});
test("12-key-statistics-are-real-and-same-day-date-is-once",async({page})=>{
  await owner(page);await go(page,stack.previewUrl,"keys");const stats=page.getByRole("region",{name:"密钥统计",exact:true});
  await expect(metricValue(stats,"当前可见密钥")).toHaveText("3");await expect(metricValue(stats,"当前可用")).toHaveText("2");await expect(metricValue(stats,"当前不可用")).toHaveText("1");await expect(metricValue(stats,"本月已计价费用")).toHaveText("¥0.055");
  for(const dd of await stats.locator("dd").all()){const size=await dd.evaluate(el=>parseFloat(getComputedStyle(el).fontSize));expect(size,"compact key readouts").toBeLessThanOrEqual(24);}
  const from=new Date(Date.now()-24*3600_000).toISOString(),to=new Date().toISOString();await go(page,stack.previewUrl,`keys?key=${world.key.id}&from=${encodeURIComponent(from)}&to=${encodeURIComponent(to)}`);
  const table=page.getByRole("table",{name:"密钥计量明细",exact:true});await expect(table).toContainText(world.model.name);await expect(table).toContainText(world.laterModel!.name);
  const dates=table.locator('tbody th[scope="rowgroup"]');await expect(dates).toHaveCount(1);await expect(dates).toHaveAttribute("rowspan","2");const raw=(await dates.innerText()).trim();expect(raw).toMatch(/\d{4}-\d{2}-\d{2}/);expect((await table.innerText()).split(raw).length-1).toBe(1);
  await page.setViewportSize({width:390,height:1000});await noOverflow(page);await contained(table);await shot(page,"tenant-key-metrics-date-group-390");
});
test("13-price-summary-and-columns-stay-compact-and-exact",async({page})=>{
  await owner(page);await go(page,stack.previewUrl,"prices");const table=page.getByRole("table",{name:"模型单价",exact:true});await expect(table).toContainText(world.model.name);const model=table.locator("tbody[data-price-model]").filter({has:page.getByText(world.model.name,{exact:true})});await expect(model.locator('[data-price-group="base"] [data-label="输入"]')).toHaveText("$1,000.00");await expect(model.locator('[data-price-group="base"] [data-label="输出"]')).toHaveText("$2,000.00");
  const summaries=["当前组织优惠","可计价模型","报价时刻"].map(name=>page.getByLabel(name,{exact:true}));const boxes=await Promise.all(summaries.map(summary=>summary.boundingBox()));expect(new Set(boxes.map(box=>Math.round(box!.y))).size,"three desktop price summaries share one row").toBe(1);
  for(const [index,summary] of summaries.entries()){await expect(summary).toBeVisible();const numeric=summary.getByText(index===0?"0%":index===1?"1":/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}$/,{exact:true});await expect(numeric).toBeVisible();expect(await numeric.evaluate(el=>parseFloat(getComputedStyle(el).fontSize)),"compact summary value font").toBeLessThanOrEqual(24);const minHeight=await summary.evaluate(el=>getComputedStyle(el).minHeight);expect(["auto","0px"],"summary height follows content without a fixed empty floor").toContain(minHeight);const box=boxes[index]!;if(index>0)expect(box.x,"summary cards do not overlap").toBeGreaterThanOrEqual(boxes[index-1]!.x+boxes[index-1]!.width);const clipped=await summary.evaluate(el=>{const b=el.getBoundingClientRect(),walk=document.createTreeWalker(el,NodeFilter.SHOW_TEXT);let node:Node|null;while((node=walk.nextNode())){if(!node.textContent?.trim())continue;const range=document.createRange();range.selectNodeContents(node);if([...range.getClientRects()].some(r=>r.left<b.left-1||r.right>b.right+1||r.top<b.top-1||r.bottom>b.bottom+1))return true;}return false;});expect(clipped,"summary text stays within its own card").toBe(false);}
  for(const width of [1440,390]){await page.setViewportSize({width,height:1000});await noOverflow(page);await contained(table);await shot(page,"tenant-price-columns-"+width);}
});
test("14-balance-ledger-keeps-11000-charge-and-readable-layout",async({page})=>{
  const one=await seedBillingWorld(stack);expect(await chat(stack,one.key.plaintext,one.model.name)).toBe(200);await expect.poll(async()=>(await stack.admin.get(`/admin/tenants/${one.tenantId}/billing`)).balance_micro_cny,{timeout:30_000}).toBe(9_989_000);
  await login(page,stack.previewUrl,one.owner,"/api");await go(page,stack.previewUrl,"billing");const balance=page.getByRole("region",{name:"余额",exact:true}),calc=page.getByRole("region",{name:"余额计算",exact:true}),ledger=page.getByRole("table",{name:"余额流水",exact:true});await expect(balance).toContainText("¥9.989");await expect(ledger).toContainText("-¥0.011");await expect(ledger).toContainText("¥10.00");
  for(const width of [1440,390]){await page.setViewportSize({width,height:1000});await noOverflow(page);await contained(ledger);await noTextOverlap(calc);const b=await balance.boundingBox(),c=await page.getByRole("region",{name:"余额流水",exact:true}).boundingBox();expect(Math.min(b!.x+b!.width,c!.x+c!.width)-Math.max(b!.x,c!.x)<=1||Math.min(b!.y+b!.height,c!.y+c!.height)-Math.max(b!.y,c!.y)<=1,"balance regions do not overlap").toBe(true);await shot(page,"tenant-balance-ledger-"+width);}
});
test("15-members-and-invites-separate-with-real-statistics",async({page})=>{
  await owner(page);await go(page,stack.previewUrl,"members");const tabs=page.getByRole("tablist",{name:"成员与邀请",exact:true});await tabs.getByRole("tab",{name:/^组织成员/}).click();const table=page.getByRole("table",{name:"组织成员",exact:true});await expect(table.locator("tbody tr")).toHaveCount(4);await expect(table).not.toContainText(pendingEmail);
  for(const [label,expected] of [["成员数量","4"],["本月活跃成员","3"],["待接受邀请数量","1"],["成员本月费用","¥0.055"]])await expect(page.getByRole("region",{name:label,exact:true}).locator(".metric")).toHaveText(expected);
  await tabs.getByRole("tab",{name:/^待接受邀请/}).click();await expect(page.getByRole("table",{name:"待接受的邀请",exact:true})).toContainText(pendingEmail);await expect(table).toHaveCount(0);await noOverflow(page);
});
test("16-member-ranking-and-detail-use-nonzero-real-usage",async({page})=>{
  await owner(page);await go(page,stack.previewUrl,"members");const api=await get(page,"/api/members/usage");const alice=api.items.find((x:any)=>x.member_id===world.members[0]!.id);expect(alice.requests).toBe(3);expect(String(alice.charged_micro_cny)).toBe("33000");
  const fee=page.getByRole("region",{name:"成员费用排行榜",exact:true}),active=page.getByRole("region",{name:"成员活跃排行榜",exact:true});for(const ranking of [fee,active]){await expect(ranking).toContainText(world.members[0]!.email);await expect(ranking.locator("ol li,tbody tr").first()).toContainText(world.members[0]!.email);}
  await expect(fee).toContainText("¥0.033");await expect(active).toContainText("3");
  await page.getByRole("link",{name:`${world.members[0]!.email} 的成员详情`,exact:true}).click();expect(new URL(page.url()).hash).toBe(`#members?member=${world.members[0]!.id}`);await expect(page.getByRole("region",{name:"本月用量明细",exact:true})).toContainText("¥0.033");await page.reload();await expect(page.getByRole("region",{name:"成员信息",exact:true})).toContainText(world.members[0]!.email);
});
test("17-member-scope-remains-restricted-at-real-API-boundary",async({page})=>{
  await login(page,stack.previewUrl,world.members[0]!,"/api");for(const path of ["/api/members","/api/members/usage"]){allowFailure(page,403,path);const code=await page.evaluate(async path=>(await fetch(path,{credentials:"include"})).status,path);expect(code).toBe(403);}
  const own=(await get(page,"/api/usage/calls?limit=200")).items;expect(own).toHaveLength(3);expect(own.every((x:any)=>Object.hasOwn(x.charge,"amount_micro_cny")&&x.charge.amount_micro_cny===11000)).toBe(true);await go(page,stack.previewUrl,"keys");await expect(metricValue(page.getByRole("region",{name:"密钥统计",exact:true}),"当前可见密钥")).toHaveText("1");
  const billing=await signed(world.members[2]!);expect((await billing.request("GET","/api/members")).status).toBe(403);
});
test("18-usage-columns-and-calls-performance-retain-money-on-mobile",async({page})=>{
  await owner(page);await go(page,stack.previewUrl,"calls");const table=page.getByRole("table",{name:"调用记录",exact:true});await performance(table);const rows=(await get(page,"/api/usage/calls?limit=200")).items;expect(rows).toHaveLength(6);await performanceValues(table,rows);const billed=rows.filter((x:any)=>x.charge.status==="charged");expect(billed).toHaveLength(fixtureCalls);expect(billed.reduce((sum:number,x:any)=>sum+x.charge.amount_micro_cny,0)).toBe(55000);
  for(const width of [1440,390]){await page.setViewportSize({width,height:1000});await noOverflow(page);await contained(table);if(width===390){const open=table.getByRole("button",{name:"查看调用详情",exact:true}).first();await open.focus();await expect(open).toBeInViewport();await open.click();const drawer=page.getByRole("dialog",{name:"调用详情",exact:true});await expect(drawer).toBeVisible();await shot(page,"tenant-real-call-drawer-390");await drawer.getByRole("button",{name:"关闭调用详情",exact:true}).click();await table.getByLabel("响应性能",{exact:true}).first().scrollIntoViewIfNeeded();await expect(table.getByLabel("响应性能",{exact:true}).first()).toBeInViewport();}await shot(page,"tenant-calls-performance-"+width);await go(page,stack.previewUrl,"usage");for(const t of await page.getByRole("table").all())await contained(t);await noOverflow(page);await shot(page,"tenant-usage-columns-"+width);await go(page,stack.previewUrl,"calls");}
});
