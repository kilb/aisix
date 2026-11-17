import { expect, test, type Page, type Locator } from "@playwright/test";
import { mkdirSync } from "node:fs";
import { join } from "node:path";

// 只观察真实页面和同源 API；没有拦截、产品格式化器或替代后端。
export const output = process.env.UNIFIED_QA_OUTPUT;
if (!output) throw new Error("UNIFIED_QA_OUTPUT must point outside the repository");
interface Health { authenticated: boolean; errors: string[]; writes: string[]; allowedWrites: Set<string>; allowedFailures: Set<string> }
const ledger = new WeakMap<Page, Health>();
export function observe(page: Page): void {
  const h: Health = { authenticated: false, errors: [], writes: [], allowedWrites: new Set(), allowedFailures: new Set() };
  ledger.set(page, h);
  page.on("pageerror", error => h.errors.push(error.message));
  page.on("console", message => {
    if (message.type() !== "error") return;
    const source = message.location().url;
    const path = /^https?:/.test(source) ? new URL(source).pathname : "";
    if (!h.authenticated && /\/(?:api|admin)\/(?:me|session)$/.test(path) && /401/.test(message.text())) return;
    if ([...h.allowedFailures].some(entry => entry.endsWith(" " + path)) && /403/.test(message.text())) return;
    h.errors.push(message.text());
  });
  page.on("response", response => {
    const path = new URL(response.url()).pathname;
    if (response.status() < 400 || !/^\/(?:api|admin)\//.test(path)) return;
    if (!h.authenticated && response.status() === 401 && /\/(?:me|session)$/.test(path)) return;
    if (h.allowedFailures.has(`${response.status()} ${path}`)) return;
    h.errors.push(`${response.status()} ${path}`);
  });
  page.on("request", request => {
    const path = new URL(request.url()).pathname;
    if (["GET", "HEAD"].includes(request.method())) return;
    if (/^\/(?:api|admin)\/(?:login|logout)$/.test(path)) return;
    const event = `${request.method()} ${path}`;
    if (!h.allowedWrites.has(event)) h.writes.push(event);
  });
}
export function allowWrite(page: Page, method: string, path: string): void { ledger.get(page)!.allowedWrites.add(`${method} ${path}`); }
export function allowFailure(page: Page, status: number, path: string): void { ledger.get(page)!.allowedFailures.add(`${status} ${path}`); }
export async function health(page: Page): Promise<void> {
  const h = ledger.get(page);
  if (!h) return;
  await test.info().attach("real-browser-health", { body: JSON.stringify({ errors: h.errors, unexpectedWrites: h.writes }), contentType: "application/json" });
  expect.soft(h.errors, "runtime, console and real API health").toEqual([]);
  expect.soft(h.writes, "no automatic business writes, probes or inference").toEqual([]);
}
export async function settle(page: Page): Promise<void> {
  await page.waitForLoadState("networkidle");
  await page.evaluate(() => document.fonts.ready);
  await expect(page.getByText("载入中…", { exact: true })).toHaveCount(0);
  await expect(page.locator("vite-error-overlay")).toHaveCount(0);
}
export async function login(page: Page, base: string, actor: {email: string; password: string}, prefix: string): Promise<void> {
  await page.goto(base);
  await page.getByLabel("邮箱", { exact: true }).fill(actor.email);
  try { await page.getByLabel("口令", { exact: true }).fill(actor.password); }
  catch { throw new Error("Could not fill the local QA password"); }
  const result = page.waitForResponse(r => new URL(r.url()).pathname === prefix + "/login" && r.request().method() === "POST");
  await page.getByRole("button", { name: "登录", exact: true }).click();
  expect((await result).status()).toBe(200);
  ledger.get(page)!.authenticated = true;
  await settle(page);
  await expect(page.locator("main")).toBeVisible();
}
export async function go(page: Page, base: string, hash: string): Promise<void> { await page.goto(base + "/#" + hash); await settle(page); }
export async function shot(page: Page, name: string): Promise<void> {
  mkdirSync(join(output!, "screenshots"), {recursive:true});
  await page.screenshot({path:join(output!, "screenshots", name + ".png"), fullPage:true, animations:"disabled", mask:[page.locator('input[type="password"]')]});
}
export async function noOverflow(page: Page): Promise<void> {
  const size = await page.evaluate(() => ({w:document.documentElement.clientWidth,doc:document.documentElement.scrollWidth,body:document.body.scrollWidth}));
  expect(size.doc, "document fits the viewport").toBeLessThanOrEqual(size.w+1);
  expect(size.body, "body fits the viewport").toBeLessThanOrEqual(size.w+1);
}
export async function get(page: Page, path: string): Promise<any> {
  const answer = await page.evaluate(async path => { const r=await fetch(path,{credentials:"include"});return {status:r.status,body:await r.json()}; },path);
  expect(answer.status, path).toBe(200); return answer.body;
}
export function value(region: Locator, name: string): Locator { return region.locator("dt").and(region.getByText(name,{exact:true})).locator("xpath=following-sibling::dd[1]"); }
export async function contained(table: Locator): Promise<void> {
  const escaped=await table.locator("tbody td,tbody th").evaluateAll(cells => cells.flatMap(cell=>{
    const b=cell.getBoundingClientRect();if(!b.width||!b.height)return[];
    return [...cell.querySelectorAll("button,input,code,[role=combobox]")].flatMap(el=>{const r=el.getBoundingClientRect();return r.width&&r.height&&(r.left<b.left-2||r.right>b.right+2)?[{text:el.getAttribute("aria-label"),width:r.width,cellWidth:b.width}]:[];});
  }));
  expect(escaped,"controls stay inside their cells").toEqual([]);
}
export function money(n: number | string | bigint): string {
  const amount=BigInt(n),v=amount<0n?-amount:amount;let f=(v%1000000n).toString().padStart(6,"0");while(f.length>2&&f.endsWith("0"))f=f.slice(0,-1);
  return `${amount<0n?"-":""}¥${v/1000000n}.${f}`;
}
export async function performance(table: Locator): Promise<void> {
  const idx=await table.getByRole("columnheader",{name:/响应性能/}).evaluate(el=>(el as HTMLTableCellElement).cellIndex);
  const rows=table.locator("tbody tr").filter({has:table.page().locator("td")});expect(await rows.count()).toBeGreaterThan(0);
  const cell=rows.first().locator("td").nth(idx);
  for(const label of ["首字","总用时","输出 TPS"]){
    const title=cell.getByText(label,{exact:true});await expect(title).toBeVisible();
    const pair=title.locator("..");await expect(pair).toContainText(/(?:—|\d)/);
    const box=await pair.boundingBox();expect(box!.height,"one labelled value stays a compact unit").toBeLessThanOrEqual(34);
  }
}

// 原始 API 数值与用户可见单位独立核对，不能把所有值改成横线后仍然绿灯。
export async function performanceValues(table: Locator, calls: any[]): Promise<void> {
  expect(calls.length, "real performance rows exist").toBeGreaterThan(0);
  for (const call of calls) {
    for (const key of ["request_id", "first_token_latency_ms", "attempt_duration_ms", "throughput_tps"]) expect(Object.hasOwn(call,key), `explicit ${key}`).toBe(true);
    const row=table.getByRole("row").filter({hasText:call.request_id});await expect(row).toHaveCount(1);
    const area=row.getByLabel("响应性能",{exact:true});
    for(const [label,key] of [["首字","first_token_latency_ms"],["总用时","attempt_duration_ms"]]){
      const dd=area.getByText(label,{exact:true}).locator("..").locator("dd");
      if(call[key]===null){await expect(dd).toHaveText("—");continue;}
      const shown=(await dd.innerText()).match(/^([\d,.]+) (ms|s)$/);expect(shown,"duration retains an explicit unit").not.toBeNull();const scale=shown![2]==="s"?1000:1;expect(Math.abs(Number(shown![1].replaceAll(",",""))*scale-call[key])).toBeLessThanOrEqual(scale/200+0.0001);
    }
    const tps=area.getByText("输出 TPS",{exact:true}).locator("..").locator("dd");
    if(call.throughput_tps===null)await expect(tps).toHaveText("—");else expect(Number((await tps.innerText()).replaceAll(",",""))).toBeCloseTo(call.throughput_tps,2);
  }
}

export async function noTextOverlap(area: Locator): Promise<void> {
  const overlaps=await area.evaluate(root=>{
    const walker=document.createTreeWalker(root,NodeFilter.SHOW_TEXT);const lines:{text:string;x:number;y:number;r:number;b:number}[]=[];let node:Node|null;
    while((node=walker.nextNode())){if(!node.textContent?.trim())continue;const range=document.createRange();range.selectNodeContents(node);for(const r of range.getClientRects())if(r.width>1&&r.height>1)lines.push({text:node.textContent.trim(),x:r.left,y:r.top,r:r.right,b:r.bottom});}
    return lines.flatMap((a,i)=>lines.slice(i+1).flatMap(b=>Math.min(a.r,b.r)-Math.max(a.x,b.x)>2&&Math.min(a.b,b.b)-Math.max(a.y,b.y)>2?[{a:a.text,b:b.text}]:[]));
  });
  expect(overlaps,"distinct balance formula text must not overlap").toEqual([]);
}

export function metricValue(region: Locator, name: string): Locator { return value(region,name).locator(":scope > .metric"); }
