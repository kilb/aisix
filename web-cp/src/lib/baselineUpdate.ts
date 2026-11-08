import type { CatalogueUpdate } from "./api.ts";

/**
 * The words 基准价 uses for a model whose baseline in force the catalogue has
 * moved past: the short form on its row, and the sentence in its panel, which
 * asks for the save that clears it.
 */
export function catalogueUpdateWords(update: CatalogueUpdate): { row: string; panel: string } {
  return {
    row: `官方目录已更新（目录 ${update.catalogue_version}）`,
    panel:
      `官方目录已更新（${update.catalogue_version}，条目 ${update.entry_key}）：` +
      "保存一份新的官方目录基准价即可确认。",
  };
}
