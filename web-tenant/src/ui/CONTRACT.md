# 共享设计系统合同

组织门户与运维台独立构建，各保存一份组件。`web-tenant/src/lib/tokenParity.test.ts` 固定以下文件逐字节一致：theme/tokens.css、theme/base.css、本合同，以及 Button/Field/Toast/TwoStep/Lamp/EmptyState/DataTable/Icon/CopyButton/KeySecret.tsx。修改其中任何文件须两端一起提交。ui.css 保留各 app 布局差异；page.tsx 的公开接口和视觉原语在两端同步维护。Playwright 中既有业务文字、role、label、权限与状态流始终有效。

## 视觉与主题

用户最新明确要求“前端页面改成侧边菜单栏比较好”“大字部分太大”，并强调“组织页面与管理后台都要改”。此指示覆盖旧套件的顶部导航、禁止侧栏及超大字级规则，两端统一采用以下紧凑合同；权限、精确金额、数据口径和可访问性合同保持。

页面标题28px、手机24px；Metric 的 hero/display/figure/value 分别为36/32/28/24px，均400字重，不再生成56–104px展示数字。lead/正文14px，辅助说明13px，meta12px、micro11px不缩小。CardHeader标题14px/500字重，编号11px mono；卡片padding20px、内容gap16px、圆角18px。字级由tokens定义，Tailwind映射引用相同变量，避免旧utility覆盖。

暖石 canvas、暖白 surface、ink；唯一橙 accent 表示当前/重点，禁止用作成功或失败颜色。没有投影、装饰渐变、入场位移。颜色原值集中在 tokens.css，字号使用 hero/display/figure/value/title/lead/stat/brand/body/caption/meta/micro，半径使用 card/cell/chip/bar/full。主题默认浅色，即使系统要求深色；现有手动开关、两端各自 storage key、刷新前 preload 行为保留。AA文本/控件边缘对比、44px控件命中区与焦点合同不变。所有字体保持同源独立文件，不内联为data字体URL。深色以同组暖中性色映射语义角色，inverse 卡固定深 ink，确保橙文字只落在足够对比的深底。

注意：原始 CSS `--muted`、`--accent` 是历史填充别名；canonical Tailwind text-muted 是灰文本、accent 是橙。旧 bg-muted/bg-accent 调用应迁至 well 或低透明 foreground；well 上的小字使用 ink-soft，不能用对比不足的 muted。货币继续通过 lib/money.ts 的 BigInt/精确小数工具，Metric 不解析、不缩写、不舍入数据。未知不是零，未设置与无限额不等价。

## 控件行为

**Button**：kind 为 primary（中性实底主操作）、secondary（默认轮廓）、danger（破坏性第二步实底）或 quiet。两种 size（默认/sm）均至少44×44；sm 只减少横向 padding。block 填满容器；icon 装饰，无文字时须有 aria-label；type 默认为 button。busy 禁用重复提交、保持标签占位，读屏名称 busyLabel 默认“处理中…”。与 busy 共用锁的其它操作使用 disabled。disabledReason 用于屏幕没有解释的禁用原因：按钮保留焦点，以 aria-disabled 阻止点击，tooltip 同时支持 hover/键盘并接入 aria-describedby。焦点为2px+offset2，不用颜色或移动来制造按压效果。

**Field**：label、hint、error、wide、children API 不变。children 可为 control 或接收 aria 的函数；有 hint/refusal 必须用函数并 spread。label 包裹控件；hint/error 由 useId 连接 aria-describedby；拒绝控件 aria-invalid 与虚线边框，原因文字在下方，不以红/橙代替说明。field-problem 没有 alert role，随字段读出。提交拒绝时 useRefusalFocus 将焦点移到第一处被拒字段，fieldset 选第一个可聚焦控件；没有字段拒绝则保留焦点，由表单 note.crit role=alert 说明。checkbox/radio 可保持18px视觉，但可点击 label 至少44px。

**Toast**：ToastProvider/useToast API 不变，动词对应操作（保存→已保存），底部显示，无入场动作，停留3秒、离开200ms，最多3条。栈是 polite live region，无额外 role，避免与 status/alert 争读。announce 默认 true；只有页面自己的活区已读相同消息才 false。toast 不能是消息的唯一载体。

**TwoStep**：label/onConfirm/disabled/kind/size/ask/warning/busy/busyLabel API 不变。first click 切到精确的“确认<label>”和“取消”；ask=false 时直接执行。确认文字体现破坏性，不依靠颜色。键盘打开后焦点移到取消，第二次 Enter 仅收回；关闭归还原操作，除非页面已将焦点移到拒绝字段。Escape 只收回当前确认、不关闭外层面板。warning role=alert。确认后保持展开，busy 禁用确认和取消，Escape 不得打断；请求结束才收回。busy 名称保留原标签，或采用 busyLabel。切换不播放宽度或淡入动画；上述询问、确认与焦点状态机保持。

**CopyButton**：text 为字符串或按需请求，label 默认“复制”；size/kind/disabled/disabledReason 与 Button 一致；成功“已复制”1.5秒并 toast；请求中禁止重复。失败通过 onFailed 显示原因，保留手工选择复制路径。

**KeySecret**：prefix/reveal/unavailable/onError API 不变，明文仅组件内存，刷新丢失，禁止存储/日志/额外缓存。复制按需 reveal 后一次复制；显示/隐藏只交换前缀和明文。没有明文时给禁用原因；clipboard 失败在按钮下显示。

**Lamp**：state applied/propagating/pending/rejected/disabled 不变；图形为中性实点/空心/方点，状态文字始终在场，无脉冲入场。children 是真实状态；title 可选。属性保持 data-lamp，禁止改为 data-state（后者是 rollout 合同）。

**DataTable/Loading**：label 保留可访问表名，compact/scroll/children 不变；header44，普通行至少56，compact至少48，使行内44按钮不被固定小高度裁切。num/right/tabular 仍控制对齐。长表横向只在 dt-wrap 内滚，scroll 同时给纵向区域和 sticky header。Loading 三骨架行和 DOM 中“载入中…”不变，骨架 aria-hidden；reduced-motion 下关闭动画。

**EmptyState**：text/mark/action 不变；页面已有同名操作时不重复添加 action，保持 strict locator 唯一性。

**Icon/Mark**：装饰 SVG currentColor、aria-hidden，控件承担可访问名称；Icon16px/1.5stroke、Mark32px；侧栏导航仍以可读文字命名，装饰图标不进入可访问名称。

## 页面原语

PageHeader 保留 title/description/actions/className，新增 eyebrow/dataLead；唯一 h1，dataLead为主摘要；同时提供description时另行完整显示说明。Card 为 section，保持传入 aria 和其它原生 props，增加 tone=surface|inverse；padding20/radius18/gap16。CardHeader 保留 title/description/actions/as/className，默认 h2，number 可显式传入，否则读取页面卡片计数；编号在 aria-hidden span，不能改变标题可访问名称。CardFooter 将图表图例放在卡底。BentoGrid 12列、主业务卡间距16px，与Grid/StatGrid/Split及卡片堆叠容器统一读取共享card-gap令牌；span.d3/d4/d5/d6 在tablet为6，其余 d5Wide/d7/d8/d12 为12；desktop>=1100 为指定列数，mobile<720 全12。各行列数由页面 owner 合满12。

Metric 仅展示调用方已格式化 children，size hero/display/figure/value对应36/32/28/24px，weight400，hero每页最多一次，inverse每页最多一张；完整金额不截断、不重新计算，必要时换行。MetricSkeleton/ChartSkeleton 不生成业务值，CardMessage 可带真正重试回调。Split 接受旧 asideWidth 参数以兼容调用，新布局统一1100起7+5，tablet/mobile两块full；Grid/StatGrid/Stat/Toolbar/DescList 兼容保留。主导航在桌面>=1100px采用224px侧栏，窄屏采用可打开/关闭的抽屉，不再沿用顶部横滚主导航。保留现有role、可访问名称、roving tabindex/方向键/Home/End/Enter/Space、data-tab、aria-controls、hash深链、角色可见性与会话行为；抽屉保持焦点管理、Escape关闭和关闭后焦点归还。

## 状态和样式钩子

note/ok/warn/crit 以中性边框和文字呈现；失败 role=alert，成功 role=status 不变。tag、hint、field-problem、cell-note、trunc（title含完整文本）、vh、cur/unit、money/num/tabular 均保留。两步确认、秘密显示、对话框焦点、field refusal、权限和数据请求属于业务合同，换皮不得放宽。
