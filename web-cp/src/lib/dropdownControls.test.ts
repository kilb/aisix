import assert from "node:assert/strict";
import { mkdir, readFile } from "node:fs/promises";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";
import { build } from "esbuild";
import { chromium, expect } from "@playwright/test";

// 组件测试挂载真实 ReactDOM，不启动服务或替换网络；后端权限由真实 E2E 验证。
const compiled = await build({
  stdin: { loader: "tsx", resolveDir: fileURLToPath(new URL("../", import.meta.url)), sourcefile: "dropdown-journey.tsx", contents: `
    import { useState } from "react";
    import { createRoot } from "react-dom/client";
    import { Select, MultiSelect } from "./ui/Select";
    import { Menu } from "./ui/Menu";
    function Journey() {
      const [value, setValue] = useState("");
      const [multi, setMulti] = useState([]);
      const [action, setAction] = useState("无操作");
      const options = [{value:"",label:"全部角色"},{value:"admin",label:"管理员"},{value:"disabled",label:"不可用角色",disabled:true},{value:"member",label:"这是一个很长的成员角色名称用于验证窄屏弹层不会越过视口边缘"}];
      return <><form onSubmit={event=>event.preventDefault()}>
        <label>角色<Select name="role" value={value} onValueChange={setValue} options={options} size="compact" /></label>
        <label>必填角色<Select value={value} onValueChange={setValue} options={options} required /></label>
        <label>第二必填角色<Select value={value} onValueChange={setValue} options={options} required /></label>
        <Select aria-label="禁用选择" value="admin" onValueChange={setValue} options={options} disabled />
        <Select aria-label="拒绝选择" aria-invalid="true" aria-describedby="refusal" value={value} onValueChange={setValue} options={options} /><p id="refusal">角色已被拒绝</p>
        <label>可用模型<MultiSelect value={multi} onValueChange={setMulti} options={options.slice(1)} placeholder="选择模型" /></label>
        <button type="submit">保存</button><output aria-label="已选值">{value}</output><output aria-label="多选值">{multi.join(",")}</output>
      </form><div className="table-overflow" style={{position:"fixed",bottom:0,left:0,overflow:"auto",width:"100%",height:60}}><div style={{width:800,display:"flex",justifyContent:"flex-end"}}><Select aria-label="表格角色" size="compact" value={value} onValueChange={setValue} options={options} /><Menu label="更多操作" items={[{label:"不可用操作",disabled:true,onSelect:()=>setAction("错误")},{label:"修改成员",onSelect:()=>setAction("修改")},{label:"停用成员",danger:true,onSelect:()=>setAction("停用")}]} /></div></div><output aria-label="操作结果">{action}</output><button>外部按钮</button><fieldset disabled><Select aria-label="字段组单选" value={value} onValueChange={setValue} options={options}/><MultiSelect aria-label="字段组多选" value={multi} onValueChange={setMulti} options={options}/><Menu label="字段组操作" items={[{label:"不应执行",onSelect:()=>setAction("错误")}]} /></fieldset><div style={{height:400}}/></>;
    }
    createRoot(document.getElementById("root")).render(<Journey />);` },
  bundle: true, platform: "browser", format: "iife", jsx: "automatic", write: false,
  loader: { ".css": "empty" }, define: { "process.env.NODE_ENV": '"test"' },
});
const [tokens, base, dropdown] = await Promise.all(["theme/tokens.css", "theme/base.css", "ui/dropdown.css"].map(path => readFile(new URL(`../${path}`, import.meta.url), "utf8")));
const styles = `@layer theme, base, legacy, components, utilities; ${tokens} @layer base { ${base} } ${dropdown}`;

test("选择与操作菜单保留键盘、空值、多选、表单、拒绝状态和窄屏表格溢出合同", { timeout: 30_000 }, async () => {
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
    const errors: string[] = [];
    page.on("pageerror", error => errors.push(error.message));
    await page.setContent(`<style>${styles}</style><div id="root"></div>`);
    await page.addScriptTag({ content: compiled.outputFiles[0]!.text });
    const role = page.getByRole("combobox", { name: "角色", exact: true });
    await expect(role).toHaveText("全部角色");
    assert.equal((await role.boundingBox())?.height, 36);
    assert.equal((await page.getByRole("combobox", { name: "必填角色", exact: true }).boundingBox())?.height, 44);
    if (process.env.DROPDOWN_QA_OUTPUT) { await mkdir(process.env.DROPDOWN_QA_OUTPUT, { recursive: true }); await page.screenshot({ path: join(process.env.DROPDOWN_QA_OUTPUT, "dropdown-component-1280.png") }); }
    await page.setViewportSize({ width: 375, height: 667 });
    await page.evaluate(() => new Promise<void>(resolve => requestAnimationFrame(() => resolve())));
    assert.equal((await role.boundingBox())?.height, 44);
    await expect(page.getByRole("combobox", { name: "禁用选择", exact: true })).toBeDisabled();
    await expect(page.getByRole("combobox", { name: "拒绝选择", exact: true })).toHaveAttribute("aria-invalid", "true");
    await expect(page.getByRole("combobox", { name: "拒绝选择", exact: true })).toHaveAccessibleDescription("角色已被拒绝");
    await role.focus(); await expect(role).toBeFocused(); await page.keyboard.press("ArrowDown");
    await expect(page.getByRole("listbox")).toBeVisible();
    await expect(page.getByRole("option", { name: "不可用角色", exact: true })).toHaveAttribute("aria-disabled", "true");
    await page.keyboard.press("End"); await expect(page.getByRole("option", { name: "这是一个很长的成员角色名称用于验证窄屏弹层不会越过视口边缘", exact: true })).toBeFocused(); await page.keyboard.press("Enter");
    await expect(page.getByRole("status", { name: "已选值", exact: true })).toHaveText("member");
    await expect(role).toBeFocused();
    assert.ok((await role.boundingBox())!.width <= 240);
    assert.equal(await role.locator(".ui-select-value").evaluate(element => getComputedStyle(element).textOverflow), "ellipsis");
    assert.deepEqual(await page.locator("form").evaluate(form => [...new FormData(form as HTMLFormElement).entries()]), [["role", "member"]]);
    await role.click(); await expect(page.getByRole("option", { name: "这是一个很长的成员角色名称用于验证窄屏弹层不会越过视口边缘", exact: true })).toBeFocused(); await page.keyboard.press("Home"); await expect(page.getByRole("option", { name: "全部角色", exact: true })).toBeFocused(); await page.keyboard.press("Enter");
    await expect(role).toHaveText("全部角色");
    assert.equal(await page.locator("form").evaluate(form => (form as HTMLFormElement).checkValidity()), false);
    await page.getByRole("button", { name: "保存", exact: true }).click();
    await expect(page.getByRole("combobox", { name: "必填角色", exact: true })).toBeFocused();
    await role.click(); await page.keyboard.press("Escape"); await expect(role).toBeFocused();
    await expect(page.getByRole("listbox")).toHaveCount(0);
    const multi = page.getByRole("button", { name: "可用模型", exact: true });
    await multi.focus(); await page.keyboard.press("ArrowDown");
    await expect(page.getByRole("menuitemcheckbox", { name: "管理员", exact: true })).toBeFocused();
    await page.keyboard.press("Space");
    await expect(page.getByRole("menuitemcheckbox", { name: "管理员", exact: true })).toHaveAttribute("aria-checked", "true");
    await expect(page.getByRole("status", { name: "多选值", exact: true })).toHaveText("admin");
    await page.keyboard.press("Escape"); await expect(multi).toBeFocused();
    const scroll = page.locator(".table-overflow");
    await scroll.evaluate(element => { element.scrollLeft = element.scrollWidth; });
    const tableRole = page.getByRole("combobox", { name: "表格角色", exact: true });
    await tableRole.click();
    const list = page.getByRole("listbox");
    await expect(list).toBeVisible();
    await expect(list).toHaveAttribute("data-side", "top");
    assert.equal(await page.getByRole("option", { name: "这是一个很长的成员角色名称用于验证窄屏弹层不会越过视口边缘", exact: true }).evaluate(element => element.scrollWidth <= element.clientWidth), true);
    assert.equal(await list.evaluate(element => element.closest(".table-overflow") === null), true);
    if (process.env.DROPDOWN_QA_OUTPUT) await page.screenshot({ path: join(process.env.DROPDOWN_QA_OUTPUT, "dropdown-component-375.png") });
    const bounds = await list.boundingBox();
    assert.ok(bounds && bounds.x >= 0 && bounds.x + bounds.width <= 375 && bounds.y + bounds.height <= 667);
    await page.keyboard.press("Escape");
    const menu = page.getByRole("button", { name: "更多操作", exact: true });
    await menu.focus(); await page.keyboard.press("ArrowDown");
    await expect(page.getByRole("menuitem", { name: "修改成员", exact: true })).toBeFocused();
    await page.keyboard.press("Enter");
    await expect(page.getByRole("status", { name: "操作结果", exact: true })).toHaveText("修改");
    await expect(menu).toBeFocused();
    await menu.click(); await page.getByRole("button", { name: "外部按钮", exact: true }).click();
    await expect(page.getByRole("menu")).toHaveCount(0);
    for (const disabledControl of [page.getByRole("combobox", { name: "字段组单选", exact: true }), page.getByRole("button", { name: "字段组多选", exact: true }), page.getByRole("button", { name: "字段组操作", exact: true })]) {
      await expect(disabledControl).toBeDisabled();
      await disabledControl.evaluate(element => element.scrollIntoView({ block: "center" }));
      const controlBounds = await disabledControl.boundingBox();
      assert.ok(controlBounds);
      await page.mouse.click(controlBounds.x + controlBounds.width / 2, controlBounds.y + controlBounds.height / 2);
      await expect(page.getByRole("listbox")).toHaveCount(0);
      await expect(page.getByRole("menu")).toHaveCount(0);
    }
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});
