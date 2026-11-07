import { useState, type ReactNode } from "react";
import { useTheme } from "./lib/useTheme";
import { Button, type ButtonKind } from "./ui/Button";
import { CopyButton } from "./ui/CopyButton";
import { DataTable, Loading } from "./ui/DataTable";
import { EmptyState } from "./ui/EmptyState";
import { Field } from "./ui/Field";
import { GroupList, GroupName } from "./ui/GroupName";
import { Icon, ICON_NAMES, Mark, MARK_NAMES } from "./ui/Icon";
import { Lamp } from "./ui/Lamp";
import { RouteNav } from "./ui/RouteNav";
import { SidePanel } from "./ui/SidePanel";
import { useToast } from "./ui/Toast";
import { TwoStep } from "./ui/TwoStep";
import "./Styleguide.css";

/**
 * Every shared component in every state, side by side, for review against
 * the design direction's polish standard. Served at `/__styleguide` by the
 * development server only; a production build drops it.
 */

const STATES = ["默认", "悬停", "按下", "键盘焦点", "不可用", "载入中"] as const;
const FORCED = ["", "is-hover", "is-active", "is-focus", "", ""] as const;

const BUTTONS: { kind: ButtonKind; label: string; name: string; icon?: "refresh" }[] = [
  { kind: "primary", label: "保存限额", name: "主要" },
  { kind: "secondary", label: "刷新", name: "次要", icon: "refresh" },
  { kind: "quiet", label: "修改口令", name: "轻按钮" },
  { kind: "danger", label: "确认停用", name: "危险确认" },
];

const SWATCHES = [
  ["--ink", "Ink", "文字、主线"],
  ["--concrete", "Concrete", "页面底色"],
  ["--paper", "Paper", "面板、表格、表单"],
  ["--sign", "Sign yellow", "主要操作、当前位置、选中"],
  ["--line", "Line blue", "链接、焦点、传播中"],
  ["--proceed", "Proceed green", "已应用、正常"],
  ["--stop", "Stop red", "被拒绝、危险确认"],
  ["--n1", "Neutral 1", "次要文字"],
  ["--n2", "Neutral 2", "说明、表头、输入框边"],
  ["--n3", "Neutral 3", "停用灯"],
  ["--hairline", "Hairline", "面板边"],
  ["--hairline-inner", "Hairline inner", "内部分隔"],
] as const;

const SCALE = [45, 36, 29, 23, 19, 15, 13, 12] as const;

function Row({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="sg-row" aria-label={title}>
      <h2>{title}</h2>
      <div className="sg-spec">{children}</div>
    </section>
  );
}

function Fig({ caption, children }: { caption: string; children: ReactNode }) {
  return (
    <figure className="sg-fig">
      {children}
      <figcaption>{caption}</figcaption>
    </figure>
  );
}

export default function Styleguide() {
  const [theme, toggleTheme] = useTheme();
  const [tab, setTab] = useState<"a" | "b" | "c" | "d">("b");
  const [panel, setPanel] = useState(true);
  const toast = useToast();

  return (
    <div className="sg">
      <header className="sg-head">
        <div>
          <h1>组件</h1>
          <p className="hint">
            每个共享组件的全部状态，静态并排，便于对照设计方向的精致度标准检查。只在开发构建中提供。
          </p>
        </div>
        <Button kind="secondary" onClick={toggleTheme} aria-label={theme === "dark" ? "切换到浅色" : "切换到深色"}>
          {theme === "dark" ? "浅色" : "深色"}
        </Button>
      </header>

      <Row title="按钮的状态">
        <table className="sg-matrix">
          <thead>
            <tr>
              <th />
              {STATES.map((s) => (
                <th key={s}>{s}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {BUTTONS.map((b) => (
              <tr key={b.kind}>
                <th>{b.name}</th>
                {STATES.map((s, i) => (
                  <td key={s}>
                    <Button
                      kind={b.kind}
                      size="sm"
                      icon={b.icon}
                      className={FORCED[i] || undefined}
                      disabled={s === "不可用"}
                      busy={s === "载入中"}
                      tabIndex={-1}
                    >
                      {b.label}
                    </Button>
                  </td>
                ))}
              </tr>
            ))}
            <tr>
              <th>图标按钮</th>
              {STATES.map((s, i) => (
                <td key={s}>
                  {s === "载入中" ? (
                    <span className="sg-na">—</span>
                  ) : (
                    <Button
                      kind="quiet"
                      size="sm"
                      icon="close"
                      aria-label="关闭"
                      className={FORCED[i] || undefined}
                      disabled={s === "不可用"}
                      tabIndex={-1}
                    />
                  )}
                </td>
              ))}
            </tr>
            <tr>
              <th>名称链接</th>
              {STATES.map((s, i) => (
                <td key={s}>
                  {i < 4 ? (
                    <button type="button" className={`link-button ${FORCED[i]}`} tabIndex={-1}>
                      北辰数据
                    </button>
                  ) : (
                    <span className="sg-na">—</span>
                  )}
                </td>
              ))}
            </tr>
            <tr>
              <th>输入框</th>
              {STATES.map((s, i) => (
                <td key={s}>
                  {s === "按下" || s === "载入中" ? (
                    <span className="sg-na">—</span>
                  ) : (
                    <input
                      className={FORCED[i] || undefined}
                      placeholder="不限"
                      defaultValue={s === "键盘焦点" || s === "不可用" ? "600" : undefined}
                      disabled={s === "不可用"}
                      aria-label="示例输入"
                      tabIndex={-1}
                    />
                  )}
                </td>
              ))}
            </tr>
            <tr>
              <th>下拉</th>
              {STATES.map((s, i) => (
                <td key={s}>
                  {s === "载入中" ? (
                    <span className="sg-na">—</span>
                  ) : (
                    <select className={FORCED[i] || undefined} disabled={s === "不可用"} aria-label="示例下拉" tabIndex={-1}>
                      <option>全部</option>
                    </select>
                  )}
                </td>
              ))}
            </tr>
            {(["checkbox", "radio"] as const).map((type) => (
              <tr key={type}>
                <th>{type === "checkbox" ? "复选框" : "单选"}</th>
                {STATES.map((s, i) => (
                  <td key={s}>
                    {s === "载入中" ? (
                      <span className="sg-na">—</span>
                    ) : (
                      <span className="sg-duo">
                        {[false, true].map((on) => (
                          <input
                            key={String(on)}
                            type={type}
                            name={`${type}-${s}`}
                            className={FORCED[i] || undefined}
                            defaultChecked={on}
                            disabled={s === "不可用"}
                            aria-label={on ? "已选" : "未选"}
                            tabIndex={-1}
                          />
                        ))}
                      </span>
                    )}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
        <p className="sg-note">
          悬停是 120ms 的颜色过渡；按下再深一级并内陷 1px；键盘焦点是 2px 线路蓝描边，外扩 2px；不可用是 45% 透明；载入中由转圈替换文字，宽度不变。
        </p>
      </Row>

      <Row title="字段">
        <div className="sg-fields">
          <Field label="租户并发">
            <input type="number" placeholder="不限" />
          </Field>
          <Field label="并发" error="请填整数；留空表示不单独限制">
            {(aria) => <input defaultValue="2.5" {...aria} />}
          </Field>
          <Field label="按状态筛选">
            <select>
              <option>全部</option>
              <option>待审核</option>
            </select>
          </Field>
        </div>
        <div className="sg-inline">
          <label className="sg-opt">
            <input type="checkbox" defaultChecked />
            chat-large
          </label>
          <label className="sg-opt">
            <input type="checkbox" />
            vision-2
          </label>
          <label className="sg-opt">
            <input type="radio" name="sg-bill" defaultChecked />
            预付费
          </label>
          <label className="sg-opt">
            <input type="radio" name="sg-bill" />
            后付费
          </label>
        </div>
      </Row>

      <Row title="信号灯、标签与线路色">
        <div className="sg-inline">
          <Lamp state="applied">已生效</Lamp>
          <Lamp state="propagating">下发中 9/14</Lamp>
          <Lamp state="pending">待下发</Lamp>
          <Lamp state="rejected">被拒绝</Lamp>
          <Lamp state="disabled">已停用</Lamp>
          <span className="tag">平台内部</span>
          <GroupName name="shared" />
          <GroupName name="eu-pool" />
          <GroupList groups={["shared", "apac-low", "batch"]} />
        </div>
        <p className="sg-note">信号灯、标签和线路色条共用一条中线；颜色从不单独表达状态，旁边总有文字。</p>
      </Row>

      <Row title="提示框">
        <div className="sg-stack">
          <div className="note">平台内部租户的密钥是运维自用的。</div>
          <div className="note ok">已保存授权。</div>
          <div className="note warn">
            <strong>邀请 token 只显示这一次。</strong>请现在复制并交给受邀人。
          </div>
          <div className="note crit" role="presentation">
            <strong>配置下发已停止。</strong>
            <p>kine 不可达：connection refused</p>
          </div>
        </div>
      </Row>

      <Row title="两步确认">
        <Fig caption="第一步，点击后原地切换">
          <TwoStep label="停用" size="sm" onConfirm={() => toast("已停用")} />
        </Fig>
        <Fig caption="第二步：确认才是红色">
          <span className="two-step-confirm">
            <Button kind="danger" size="sm" tabIndex={-1}>
              确认停用
            </Button>
            <Button size="sm" tabIndex={-1}>
              取消
            </Button>
          </span>
        </Fig>
        <Fig caption="带说明的两步确认">
          <TwoStep label="吊销" size="sm" warning="吊销后该密钥永久失效，不能恢复。" onConfirm={() => toast("已吊销")} />
        </Fig>
      </Row>

      <Row title="复制与提示">
        <Fig caption="复制后按钮显示已复制 1.5 秒，并弹出提示">
          <CopyButton text="sk-aisix-example" onFailed={() => toast("复制没有成功")} />
        </Fig>
        <Fig caption="提示在左下角，用动作本身的动词">
          <Button size="sm" onClick={() => toast("已保存")}>
            保存
          </Button>
        </Fig>
        <Fig caption="静态示例">
          <div className="toast">已保存限额</div>
        </Fig>
        <Fig caption="不可用但看不出原因时，悬停给出原因">
          <Button size="sm" disabled disabledReason="这个密钥没有分组覆盖">
            清除覆盖
          </Button>
        </Fig>
      </Row>

      <Row title="表格、载入与空状态">
        <div className="sg-wide">
          <div className="panel">
            <h2>租户</h2>
            <DataTable label="示例表格">
              <thead>
                <tr>
                  <th>名称</th>
                  <th>状态</th>
                  <th>分组</th>
                  <th className="num">成员</th>
                  <th className="num">密钥</th>
                  <th>下发</th>
                </tr>
              </thead>
              <tbody>
                <tr>
                  <td>
                    <button type="button" className="link-button">
                      北辰数据
                    </button>
                  </td>
                  <td>
                    <Lamp state="applied">正常</Lamp>
                  </td>
                  <td>
                    <GroupList groups={["shared", "apac-low"]} />
                  </td>
                  <td className="num">8</td>
                  <td className="num">12</td>
                  <td>
                    <Lamp state="applied">已生效</Lamp>
                  </td>
                </tr>
                <tr>
                  <td>
                    <button type="button" className="link-button">
                      Kestrel Labs
                    </button>
                    <span className="tag tn-mark">平台内部</span>
                  </td>
                  <td>
                    <Lamp state="pending">待审核</Lamp>
                  </td>
                  <td>
                    <GroupName name="shared" />
                  </td>
                  <td className="num">1</td>
                  <td className="num">1,024</td>
                  <td>
                    <Lamp state="rejected">被拒绝</Lamp>
                    <span className="cell-note">
                      <code>gw-east-07</code>：分组 batch 不存在
                    </span>
                  </td>
                </tr>
              </tbody>
            </DataTable>
          </div>
          <div className="panel">
            <h2>载入中</h2>
            <Loading />
          </div>
          <div className="panel">
            <h2>密钥</h2>
            <EmptyState
              mark="key"
              text="还没有密钥。新建一个，就能用它调用组织获授权的模型。"
              action={<Button kind="primary">新建密钥</Button>}
            />
          </div>
        </div>
      </Row>

      <Row title="详情面板与路线导航">
        <div className="sg-nav">
          <RouteNav
            items={[
              { id: "a", label: "概览" },
              { id: "b", label: "租户" },
              { id: "c", label: "密钥" },
              { id: "d", label: "审计日志" },
            ]}
            current={tab}
            attention={{ a: "1 个资源被网关拒绝", b: "2 个租户待审核" }}
            label="示例分区"
            onSelect={setTab}
          />
        </div>
        <div className="sg-panel">
          {panel ? (
            <SidePanel title="北辰数据" label="租户详情：北辰数据" extra={<Lamp state="applied">已生效</Lamp>} onClose={() => setPanel(false)}>
              <p className="hint tn-facts">正常 · 预付费 · 成员 8 · 密钥 12</p>
              <p className="hint">从右侧滑入 160ms，列表保持可见；Esc 或关闭按钮收起。</p>
            </SidePanel>
          ) : (
            <Button size="sm" onClick={() => setPanel(true)}>
              打开详情
            </Button>
          )}
        </div>
      </Row>

      <Row title="图标">
        <ul className="sg-icons">
          {ICON_NAMES.map((n) => (
            <li key={n}>
              <Icon name={n} />
              <span>{n}</span>
            </li>
          ))}
          {MARK_NAMES.map((n) => (
            <li key={n} className="sg-mark">
              <Mark name={n} />
              <span>{n} 32px</span>
            </li>
          ))}
        </ul>
      </Row>

      <Row title="字号">
        <ul className="sg-scale">
          {SCALE.map((px) => (
            <li key={px}>
              <span className="sg-px">{px}</span>
              <span style={{ fontSize: px, fontWeight: px >= 36 ? 800 : px >= 19 ? 700 : 400, lineHeight: px >= 19 ? 1.2 : 1.6 }}>
                {px >= 36 ? (
                  <span className="money">
                    <span className="cur" style={{ fontSize: px >= 45 ? 29 : 23, fontWeight: 600, marginRight: 4 }}>
                      $
                    </span>
                    2,418.36
                  </span>
                ) : px >= 19 ? (
                  "授权模型 Routing 12"
                ) : (
                  "停用租户会让它的全部密钥立即失效 Overpass 0123456789"
                )}
              </span>
            </li>
          ))}
          <li>
            <span className="sg-px">13</span>
            <code>rt_live_8f2c… req_01JD7Q4M8X</code>
          </li>
        </ul>
      </Row>

      <Row title="颜色">
        <ul className="sg-swatches">
          {SWATCHES.map(([token, name, role]) => (
            <li key={token}>
              <span className="sg-chip" style={{ background: `var(${token})` }} />
              <b>{name}</b>
              <code>{token}</code>
              <span>{role}</span>
            </li>
          ))}
        </ul>
        <div className="sg-inline">
          {[1, 2, 3, 4, 5, 6].map((n) => (
            <span key={n} className="grp" style={{ "--c": `var(--route-${n})` } as React.CSSProperties}>
              --route-{n}
            </span>
          ))}
        </div>
      </Row>
    </div>
  );
}
