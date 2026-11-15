import { expect, type Locator } from "@playwright/test";

type SelectChoice = string | { label?: string; value?: string; index?: number };

/** 原生控件保留原接口，自定义控件的选项在页面级 Portal 中查找。 */
export async function selectOptions(control: Locator): Promise<Locator> {
  if (await control.evaluate(element => element.tagName === "SELECT")) return control.locator("option");
  await control.click();
  await expect(control.page().getByRole("listbox")).toBeVisible();
  return control.page().getByRole("option");
}

/** 按原有标签、实际值或顺序选择，不改业务值或禁用状态。 */
export async function chooseSelect(control: Locator, choice: SelectChoice): Promise<void> {
  if (await control.evaluate(element => element.tagName === "SELECT")) {
    await control.selectOption(choice);
    return;
  }
  const page = control.page();
  let option = await selectOptions(control);
  const { label, value, index } = typeof choice === "string" ? { value: choice } : choice;
  if (index !== undefined) option = option.nth(index);
  if (label !== undefined) option = option.and(page.getByRole("option", { name: label, exact: true }));
  if (value !== undefined) {
    const escaped = await page.evaluate(selected => CSS.escape(selected), value);
    option = option.and(page.locator(`[data-value="${escaped}"]`));
  }
  if (label === undefined && value === undefined && index === undefined) throw new Error("请选择一个选项");
  await option.click();
}
