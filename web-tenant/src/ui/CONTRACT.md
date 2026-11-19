# 共享设计系统合同

组织门户与运维台独立构建，各保存一份组件。`web-tenant/src/lib/tokenParity.test.ts` 固定以下文件逐字节一致：theme/tokens.css、theme/base.css、本合同，以及 Button/Field/Toast/TwoStep/Lamp/EmptyState/DataTable/Icon/CopyButton/KeySecret.tsx。修改其中任何文件须两端一起提交。ui.css 保留各 app 布局差异；page.tsx 的公开接口和视觉原语在两端同步维护。Playwright 中既有业务文字、role、label、权限与状态流始终有效。

## 视觉与主题

双端采用用户指定的本地控制台模板：暖白画布 #F3F2EE、白色面板 #FFFFFF、墨色正文 #111110、lilac #E3D0FF 与 lime #D5F95F。主操作为墨色胶囊，装饰图标使用 lime 圆形底；次级按钮为白色胶囊与轻投影。成功、警告、危险同时附文字与不同形状，不能只依赖颜色。所有颜色原值集中在 theme/tokens.css；业务页面只使用语义令牌。

本地 Manrope variable 承担拉丁正文和数字，中文沿用本地 Noto Sans SC 400，以 font-synthesis:weight 支持标题粗体；不加载重复的整套中文字重。页面标题34px/800、手机30px；小标题20px/800，卡标题15px/700，正文14px，small13px、caption12px、micro11px。指标30px/800，大指标44px；精确金额不缩写。卡片圆角28px，内部分组14px，控件为胶囊；卡片默认padding20px、sm16px、table与none0，区域gap16px。为保留用户已指定的紧凑操作密度，常规控件40px、compact32px，窄屏或粗指针命中区44px。卡片轻投影、边界柔和；不堆叠黑色线框与装饰容器。每页最多一张lilac、一张ink与一张lime卡片，其余白色。

桌面侧栏内容236px、收起68px，距离视口16px、与主内容gap16px，圆角28px；当前项墨色填充、lime图标，折叠状态保留可访问名称。桌面切换点1024px；主内容12列网格在1280px展开。表格和编辑表单始终占满一行，详情纵向排列，模型与路由编辑不嵌套卡片。窄屏使用保留焦点合同的Radix抽屉，距离视口16px。列表保留筛选、列信息与分页，避免图标外再套卡片。

主题默认浅色，不读取系统主题；现有手动开关、两端独立storage key、preload与同源字体加载保持。深色按画布/面板/嵌套区分层，保留同样的颜色语义和AA对比要求。历史 `--muted` / `--accent` 仍是中性填充别名；canonical text-muted是次级文字，accent映射品牌。ink/inverse卡片在局部重设前景色，lilac/lime卡片使用墨色文字；普通面板保持中性前景。

货币继续使用lib/money.ts的BigInt与精确小数。Metric只呈现格式化结果，不解析、不缩写、不舍入；未知不是零，未设置与无限额不同。

## 控件行为

**Button**：kind 为 primary（墨色实底主操作）、secondary（默认白色轻投影）、danger（危险色破坏性第二步实底）或 quiet（无常驻边框或底色的轻操作）。默认40px，sm为32px，手机或粗指针均44px；没有显式kind的纯图标操作默认quiet。block 填满容器；icon 装饰，无文字时须有 aria-label；type 默认为 button。busy 禁用重复提交、保持标签占位，读屏名称 busyLabel 默认“处理中…”。与 busy 共用锁的其它操作使用 disabled。disabledReason 用于屏幕没有解释的禁用原因：按钮保留焦点，以 aria-disabled 阻止点击，tooltip 同时支持 hover/键盘并接入 aria-describedby。焦点为主题高对比2px+offset2，输入与菜单附柔和光晕，不用移动来制造按压效果。

**Field**：label、hint、error、wide、children API 不变。children 可为 control 或接收 aria 的函数；有 hint/refusal 必须用函数并 spread。label 包裹控件；hint/error 由 useId 连接 aria-describedby；拒绝控件 aria-invalid 与虚线边框，原因文字在下方，危险色不替代原因说明。field-problem 没有 alert role，随字段读出。提交拒绝时 useRefusalFocus 将焦点移到第一处被拒字段，fieldset 选第一个可聚焦控件；没有字段拒绝则保留焦点，由表单 note.crit role=alert 说明。checkbox/radio 可保持18px视觉，但可点击 label 至少44px。

**Select / MultiSelect / Menu**：双端共享同一Radix原语与dropdown.css。单选使用value/onValueChange/options，兼容option/optgroup/Fragment描述符；空字符串保留原值，name/required/form保持原生表单合同。默认表单40px，size=compact桌面32px、手机或粗指针均44px；紧凑触发器与表格行对齐，长文字在触发器省略、弹层折行。弹层通过Portal脱离表格overflow，与触发器对齐并在视口边缘翻转/约束；不扩大表格单元格。键盘方向键/Home/End/类型检索与Enter选取由Radix管理；Escape和外点收起并保留焦点合同。MultiSelect保留数组原值，勾选后保持展开。Menu使用label/items，disabled项不能选择，danger项始终附具体操作文字。拒绝态保持aria-invalid/aria-describedby和虚线边框。

**Toast**：ToastProvider/useToast API 不变，动词对应操作（保存→已保存），底部显示，无入场动作，停留3秒、离开200ms，最多3条。栈是 polite live region，无额外 role，避免与 status/alert 争读。announce 默认 true；只有页面自己的活区已读相同消息才 false。toast 不能是消息的唯一载体。

**TwoStep**：label/onConfirm/disabled/kind/size/ask/warning/busy/busyLabel API 不变。first click 切到精确的“确认<label>”和“取消”；ask=false 时直接执行。确认文字体现破坏性，不依靠颜色。键盘打开后焦点移到取消，第二次 Enter 仅收回；关闭归还原操作，除非页面已将焦点移到拒绝字段。Escape 只收回当前确认、不关闭外层面板。warning role=alert。确认后保持展开，busy 禁用确认和取消，Escape 不得打断；请求结束才收回。busy 名称保留原标签，或采用 busyLabel。切换不播放宽度或淡入动画；上述询问、确认与焦点状态机保持。

**CopyButton**：text 为字符串或按需请求，label 默认“复制”；size/kind/disabled/disabledReason 与 Button 一致，kind默认quiet；始终只显示 Copy/Check 图标，aria-label/title 保留操作与成功名称；成功 Check 持续1.5秒并 toast；请求中禁止重复。失败通过 onFailed 显示重试方式；密钥明文不能因复制失败而进入 DOM、属性或输入值。

**KeySecret**：prefix/reveal/unavailable/onError API 不变，明文仅组件内存，刷新丢失，禁止存储/日志/额外缓存。正常只显示前缀遮罩与复制图标，复制按需 reveal 后一次复制；无显示/隐藏控件。没有明文时给禁用原因；clipboard 失败时只显示权限提示并允许重试，不生成明文文本、属性或输入值。新建结果同样只显示前缀遮罩与复制图标，明文仅保留在 React 内存与复制闭包，刷新或离页清除。

**Lamp**：state applied/propagating/pending/rejected/disabled 不变；图形为成功勾选圆/警告三角/危险叉号圆，处理中时钟/停用横杠圆，状态文字始终在场，无脉冲入场。children 是真实状态；title 可选。属性保持 data-lamp，禁止改为 data-state（后者是 rollout 合同）。

**DataTable/Loading**：label 同时保留aria-label与隐藏caption可访问表名，compact/scroll/children 不变；header40，普通行基准40，含操作的行由44px命中区自然撑开，使按钮不被固定小高度裁切。num/right/tabular 仍控制对齐。记录分页自然撑开页面，不在表格外框限制高度；长列仅在 dt-wrap 内按需横向滚动，scroll 继续作为兼容钩子而不强制纵向滚动。Loading 三骨架行和 DOM 中“载入中…”不变，骨架 aria-hidden；reduced-motion 下关闭动画。

**EmptyState**：text/mark/action 不变；页面已有同名操作时不重复添加 action，保持 strict locator 唯一性。

**Icon/Mark**：装饰 SVG currentColor、aria-hidden，控件承担可访问名称；Icon16px/1.75stroke、Mark32px；侧栏导航仍以可读文字命名，装饰图标不进入可访问名称。

## 页面原语

PageHeader 保留title/description/actions/className/eyebrow/dataLead接口；唯一h1，dataLead为主摘要，description完整显示。eyebrow仅兼容旧调用而不再生成装饰标注。Card为section，保留原生props和tone=surface|default|inverse|ink|lilac|lime接口，inverse兼容ink。padding=md|sm|table|none明示间距，业务字段不增加卡片层级。CardHeader保留title/description/actions/as/className/id/number接口，默认h2，number不再呈现非顺序编号。CardFooter仍承载图例与说明。

BentoGrid为12列、间距16px；span.d3/d4/d5/d6在tablet为6，其余为12；desktop>=1280采用指定列数，mobile<768全12。Split保留asideWidth兼容参数，列表与详情均纵向全宽。Grid/StatGrid/Stat/Toolbar/DescList接口保留。骨架不生成业务数据，CardMessage重试使用真实回调。

主导航桌面>=1024采用可折叠236px侧栏，其余为抽屉。入口按运行/接入/组织账务/平台管理分组；门户按工作空间/使用账务/组织管理分组。权限过滤后空组隐藏。侧栏使用真实href链接与aria-current=page，保留label、data-tab和attention文字；Tab依次访问链接，方向键/Home/End移动焦点，Enter原生激活。页内tabs仍保留tab id、role、aria-controls与方向键/Home/End/Enter/Space的手动激活合同。个人账号页打开时，业务导航仍可通过Tab访问。抽屉支持Escape、焦点归还，账号入口同样关闭抽屉。

双端个人账号设置使用#account，侧栏身份和顶栏账号按钮均可达。个人登录资料只读，修改口令仅在账号页；登出归入会话区。所有已登录角色都可改自己的口令，门户pending/suspended同样可达。运维账号管理保留admin权限，组织账务保留原权限，billing id不变且显示名为账务账户。原#prices?group深链、邀请优先于已登录页、登录/会话失效/API版本保护保持。

RecordTable为记录集合的筛选分页基础设施，DataTable旧API保持。RecordTableToolbar使用search={value,onChange,label,placeholder}与children；RecordTablePager使用1-based page、pageSize、onPageChange、onPageSizeChange及total?/loadedCount?/hasNext?/busy?/label?。默认25条，可选10/25/50；未知总数只显示已加载量，不虚构总页数，不跳到尚未获得cursor的页。useRecordTable只搜索调用方允许的字段，筛选变化回首页，删除后夹取页码；父行与展开详情保持同页。统计汇总使用全量数据，分页仅作用于记录。

## 状态和样式钩子

note/ok/warn/crit 以相应语义边框、浅色底与文字呈现；失败 role=alert，成功 role=status 不变。tag、hint、field-problem、cell-note、trunc（title含完整文本）、vh、cur/unit、money/num/tabular 均保留。两步确认、秘密显示、对话框焦点、field refusal、权限和数据请求属于业务合同，换皮不得放宽。

## 输出 TPS

调用记录与详情统一标为“输出 TPS”：输出 Token（含推理，不含输入）÷ 完整上游耗时（秒）。输出以服务端规范化口径为准，已包含的推理 Token 不重复相加，输入与缓存 Token 不进入分子。前端只格式化 API 的 `throughput_tps`，不从 Token、首字、上游首帧、TPOT 或其它耗时重算，不补缺失值、不限幅。历史记录缺少有效输出或完整上游耗时时显示 —；非流式记录不因缺少首帧而被排除。

`usage_estimated === true` 时调用记录保留 ≈ 参考速度；缺少来源标记不推断估算发生在哪一方向。模型监测排除含估算用量的记录，平均值为有效输出 Token 合计 ÷ 有效完整上游耗时合计（秒），P50/P95 采用同口径的单次输出 TPS；无有效采样显示 —。底层 TPOT 指标和模型主动探测的端到端速率保持独立。

## 上游 HTTP 阶段

调用记录中的“请求首内容”从网关收到请求计至首次交付生成内容，包含请求体读取和先前尝试。“本次上游耗时”从该次尝试开始计时。调用方断开且只观察到发起上游 HTTP 时，标签为“尝试开始至断开”。旧尝试耗时的零哨兵显示为 —，请求首内容的实际零值显示为 0 ms。

调用详情的“上游 HTTP 阶段”只展示 `upstream_timing` 的七个原始观测偏移，单位为毫秒、起点为同一次上游尝试；不计算相邻差值，不与入口阶段或客户端首 Token 时钟相减。HTTP 客户端开始发送记录交给 HTTP 客户端的时点，不表示操作系统实际写包；首次内容交付记录网关交给 HTTP server 的时点，不表示客户端收到或确认。

七字段均可选：`http_send_started_ms`、`response_headers_ms`、`first_response_byte_ms`、`first_protocol_frame_ms`、`first_canonical_chunk_ms`、`upstream_first_content_ms`、`first_content_ms`。只有整数且位于 u32 范围的值显示为原始毫秒；零保留，缺失、null 或非法值显示 —，不从 TTFT、完整耗时或其它阶段补算。协议帧与规范化片段可以未采集，非流式记录不拿完整响应补填。历史事件不补写，也不引入逐帧 JSON。
