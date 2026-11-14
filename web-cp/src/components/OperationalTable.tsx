import { Children, cloneElement, isValidElement, type ReactElement, type ReactNode } from "react";
import "./OperationalTable.css";

export { Loading } from "../ui/DataTable";

type CellProps = { children?: ReactNode; colSpan?: number; className?: string; scope?: string; role?: string; "data-label"?: string };

function textOf(node: ReactNode): string {
  return Children.toArray(node).map((child) => typeof child === "string" || typeof child === "number"
    ? String(child) : isValidElement<CellProps>(child) ? textOf(child.props.children) : "").join("");
}

function headingsOf(node: ReactNode, inHead = false): string[] {
  return Children.toArray(node).flatMap((child) => {
    if (!isValidElement<CellProps>(child)) return [];
    if (child.type === "tbody" || child.type === "tfoot") return [];
    if (child.type === "th" && inHead) return [textOf(child.props.children).trim() || "操作"];
    return headingsOf(child.props.children, inHead || child.type === "thead");
  });
}

function describeRows(node: ReactNode, headings: readonly string[], inHead = false): ReactNode {
  return Children.map(node, (child) => {
    if (!isValidElement<CellProps>(child)) return child;
    const head = inHead || child.type === "thead";
    if (child.type === "tr") {
      let column = 0;
      const cells = Children.map(child.props.children, (cell) => {
        if (!isValidElement<CellProps>(cell) || (cell.type !== "td" && cell.type !== "th")) return cell;
        const label = headings[column] || "详情";
        column += cell.props.colSpan ?? 1;
        return cloneElement(cell, {
          role: head ? "columnheader" : cell.type === "th" ? "rowheader" : "cell",
          ...(!head && (cell.props.colSpan ?? 1) === 1 ? { "data-label": label } : {}),
          className: [cell.props.className, !head && (cell.props.colSpan ?? 1) > 1 ? "ops-table-detail" : null].filter(Boolean).join(" "),
        });
      });
      return cloneElement(child, { role: "row" }, cells);
    }
    if (child.type === "thead" || child.type === "tbody" || child.type === "tfoot") {
      return cloneElement(child, { role: "rowgroup" }, describeRows(child.props.children, headings, head));
    }
    // 行片段保留稳定 key；不展开业务组件，也不改变单元格中的交互。
    if (typeof child.type === "symbol") return cloneElement(child as ReactElement<CellProps>, {}, describeRows(child.props.children, headings, head));
    return child;
  });
}

/** 宽屏按列比较，窄工作区按字段阅读；分页内容始终随页面自然展开。 */
export function OperationalTable({ label, compact = false, children }: { label?: string; compact?: boolean; scroll?: boolean; children: ReactNode }) {
  const headings = headingsOf(children);
  return <div className="dt-wrap ops-record-table" role="region" aria-label={label ? `${label}表格` : "记录表格"}>
    <table role="table" className={compact ? "dt dt-compact" : "dt"} aria-label={label}>
      {describeRows(children, headings)}
    </table>
  </div>;
}
