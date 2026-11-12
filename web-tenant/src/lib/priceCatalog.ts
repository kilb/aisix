import type { QuotedGroup, QuotedModel } from "./priceQuote";

export interface ModelGroupQuote {
  id: string;
  display_name: string;
  description: string;
  quote: QuotedModel;
}

export interface CatalogModel {
  model_id: string;
  name: string;
  groups: ModelGroupQuote[];
}

// 仅重排同一份组织报价；价格、倍率、档位及上下文段保留服务端精确值。
export function modelPriceCatalog(groups: readonly QuotedGroup[], groupId = ""): CatalogModel[] {
  const models = new Map<string, CatalogModel>();
  for (const group of groups) {
    if (groupId !== "" && group.id !== groupId) continue;
    for (const quote of group.models) {
      let model = models.get(quote.model_id);
      if (model === undefined) {
        model = { model_id: quote.model_id, name: quote.name, groups: [] };
        models.set(quote.model_id, model);
      }
      model.groups.push({ id: group.id, display_name: group.display_name, description: group.description, quote });
    }
  }
  return [...models.values()].sort((a, b) => a.name.localeCompare(b.name) || a.model_id.localeCompare(b.model_id));
}
