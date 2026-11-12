# 共享设计系统合同

组织门户与运维台独立构建，各保存一份组件。`web-tenant/src/lib/tokenParity.test.ts` 固定以下文件逐字节一致：theme/tokens.css、theme/base.css、本合同，以及 Button/Field/Toast/TwoStep/Lamp/EmptyState/DataTable/Icon/CopyButton/KeySecret.tsx。修改其中任何文件须两端一起提交。ui.css 保留各 app 布局差异；page.tsx 的公开接口和视觉原语在两端同步维护。Playwright 中既有业务文字、role、label、权限与状态流始终有效。

## 视觉与主题

双端使用 Refined 暖石灰视觉：画布 #ECEAE4、暖白面板 #FBFAF7、嵌套区 #E1DED6、墨色 #121211、次级文字 #64635D。主按钮、链接和焦点使用墨色；#FF5A1F 仅用于当前导航小点、实时点与重点图形，不用于浅底小字。成功、警告、失败仍附状态文字与图形。颜色原值集中在 tokens.css，不在业务页面另建主题。

页面标题28px、手机26px；hero36px、display32px、figure/value28px，手机hero32px、其余28px，指标400字重。正文与导航14px，描述13px，meta/micro最少12px。CardHeader为16px/600，页标题500。中文普通标签使用sans，等宽用于机器标识与代码。面板padding20px、手机16px；内容gap16px、区域24px、面板圆角24px，按钮与输入框圆角8px。控件命中区保持44px。面板用细边线分层，不使用投影、装饰渐变或入场位移。

桌面侧栏224px，距离视口左、上、下24px，与主内容间距24px；圆角24px，暖白面板，菜单独立滚动，账号固定在底部。当前项采用淡墨底、墨字与橙点。页头融入画布；宽表只在自身容器滚动。窄屏使用保留焦点合同的抽屉，距离视口16px，内容按原断点重排。

主题默认浅色，不读取系统主题；现有手动开关、两端独立storage key、preload与同源字体加载保持。深色按画布/面板/嵌套区分层，保留同样的颜色语义和AA对比要求。历史 `--muted` / `--accent` 仍是中性填充别名；canonical text-muted是次级文字，accent映射品牌。on-inverse-muted在旧卡片回归普通面板后映射次级文字，避免浅底浅字。

货币继续使用lib/money.ts的BigInt与精确小数。Metric只呈现格式化结果，不解析、不缩写、不舍入；未知不是零，未设置与无限额不同。

## 控件行为

**Button**：kind 为 primary（墨色实底主操作）、secondary（默认轮廓）、danger（危险色破坏性第二步实底）或 quiet。两种 size（默认/sm）均至少44×44；sm 只减少横向 padding。block 填满容器；icon 装饰，无文字时须有 aria-label；type 默认为 button。busy 禁用重复提交、保持标签占位，读屏名称 busyLabel 默认“处理中…”。与 busy 共用锁的其它操作使用 disabled。disabledReason 用于屏幕没有解释的禁用原因：按钮保留焦点，以 aria-disabled 阻止点击，tooltip 同时支持 hover/键盘并接入 aria-describedby。焦点为2px+offset2，不用颜色或移动来制造按压效果。

**Field**：label、hint、error、wide、children API 不变。children 可为 control 或接收 aria 的函数；有 hint/refusal 必须用函数并 spread。label 包裹控件；hint/error 由 useId 连接 aria-describedby；拒绝控件 aria-invalid 与虚线边框，原因文字在下方，危险色不替代原因说明。field-problem 没有 alert role，随字段读出。提交拒绝时 useRefusalFocus 将焦点移到第一处被拒字段，fieldset 选第一个可聚焦控件；没有字段拒绝则保留焦点，由表单 note.crit role=alert 说明。checkbox/radio 可保持18px视觉，但可点击 label 至少44px。

**Toast**：ToastProvider/useToast API 不变，动词对应操作（保存→已保存），底部显示，无入场动作，停留3秒、离开200ms，最多3条。栈是 polite live region，无额外 role，避免与 status/alert 争读。announce 默认 true；只有页面自己的活区已读相同消息才 false。toast 不能是消息的唯一载体。

**TwoStep**：label/onConfirm/disabled/kind/size/ask/warning/busy/busyLabel API 不变。first click 切到精确的“确认<label>”和“取消”；ask=false 时直接执行。确认文字体现破坏性，不依靠颜色。键盘打开后焦点移到取消，第二次 Enter 仅收回；关闭归还原操作，除非页面已将焦点移到拒绝字段。Escape 只收回当前确认、不关闭外层面板。warning role=alert。确认后保持展开，busy 禁用确认和取消，Escape 不得打断；请求结束才收回。busy 名称保留原标签，或采用 busyLabel。切换不播放宽度或淡入动画；上述询问、确认与焦点状态机保持。

**CopyButton**：text 为字符串或按需请求，label 默认“复制”；size/kind/disabled/disabledReason 与 Button 一致；成功“已复制”1.5秒并 toast；请求中禁止重复。失败通过 onFailed 显示原因，保留手工选择复制路径。

**KeySecret**：prefix/reveal/unavailable/onError API 不变，明文仅组件内存，刷新丢失，禁止存储/日志/额外缓存。复制按需 reveal 后一次复制；显示/隐藏只交换前缀和明文。没有明文时给禁用原因；clipboard 失败在按钮下显示。

**Lamp**：state applied/propagating/pending/rejected/disabled 不变；图形为成功实点/警告空心/危险方点，状态文字始终在场，无脉冲入场。children 是真实状态；title 可选。属性保持 data-lamp，禁止改为 data-state（后者是 rollout 合同）。

**DataTable/Loading**：label 保留可访问表名，compact/scroll/children 不变；header44，普通行至少56，compact至少48，使行内44按钮不被固定小高度裁切。num/right/tabular 仍控制对齐。长表横向只在 dt-wrap 内滚，scroll 同时给纵向区域和 sticky header。Loading 三骨架行和 DOM 中“载入中…”不变，骨架 aria-hidden；reduced-motion 下关闭动画。

**EmptyState**：text/mark/action 不变；页面已有同名操作时不重复添加 action，保持 strict locator 唯一性。

**Icon/Mark**：装饰 SVG currentColor、aria-hidden，控件承担可访问名称；Icon16px/1.5stroke、Mark32px；侧栏导航仍以可读文字命名，装饰图标不进入可访问名称。

## 页面原语

PageHeader 保留title/description/actions/className/eyebrow/dataLead接口；唯一h1，dataLead为主摘要，description完整显示。eyebrow仅兼容旧调用而不再生成装饰标注。Card为section，保留原生props和tone=surface|inverse接口；旧inverse调用呈现普通面板，不重新定义局部颜色。CardHeader保留title/description/actions/as/className/number接口，默认h2，number不再呈现非顺序编号。CardFooter仍承载图例与说明。

BentoGrid为12列、间距16px；span.d3/d4/d5/d6在tablet为6，其余为12；desktop>=1100采用指定列数，mobile<720全12。Split保留asideWidth兼容参数，桌面7+5，其余纵向。Grid/StatGrid/Stat/Toolbar/DescList接口保留。骨架不生成业务数据，CardMessage重试使用真实回调。

主导航桌面>=1100采用224px侧栏，其余为抽屉。入口按运行/接入/组织账务/平台管理分组；门户按工作空间/使用账务/组织管理分组。权限过滤后空组隐藏。侧栏使用真实href链接与aria-current=page，保留label、data-tab和attention文字；Tab依次访问链接，方向键/Home/End移动焦点，Enter原生激活。页内tabs仍保留tab id、role、aria-controls与方向键/Home/End/Enter/Space的手动激活合同。个人账号页打开时，业务导航仍可通过Tab访问。抽屉支持Escape、焦点归还，账号入口同样关闭抽屉。

双端个人账号设置使用#account，侧栏身份和顶栏账号按钮均可达。个人登录资料只读，修改口令仅在账号页；登出归入会话区。所有已登录角色都可改自己的口令，门户pending/suspended同样可达。运维账号管理保留admin权限，组织账务保留原权限，billing id不变且显示名为账务账户。原#prices?group深链、邀请优先于已登录页、登录/会话失效/API版本保护保持。

RecordTable为记录集合的筛选分页基础设施，DataTable旧API保持。RecordTableToolbar使用search={value,onChange,label,placeholder}与children；RecordTablePager使用1-based page、pageSize、onPageChange、onPageSizeChange及total?/loadedCount?/hasNext?/busy?/label?。默认25条，可选10/25/50；未知总数只显示已加载量，不虚构总页数，不跳到尚未获得cursor的页。useRecordTable只搜索调用方允许的字段，筛选变化回首页，删除后夹取页码；父行与展开详情保持同页。统计汇总使用全量数据，分页仅作用于记录。

## 状态和样式钩子

note/ok/warn/crit 以相应语义边框、浅色底与文字呈现；失败 role=alert，成功 role=status 不变。tag、hint、field-problem、cell-note、trunc（title含完整文本）、vh、cur/unit、money/num/tabular 均保留。两步确认、秘密显示、对话框焦点、field refusal、权限和数据请求属于业务合同，换皮不得放宽。
