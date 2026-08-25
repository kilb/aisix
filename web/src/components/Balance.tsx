import { fmtUsd } from "../lib/fmt";

/**
 * 结账块。
 *
 * 这一页的签名，也是台账自己的装置：上限记一笔、已花记一笔，双线收口，
 * 余额用红墨。运维真正在问的不是「花了多少」，而是**离上限还有多远**——
 * 那个上限是配置里真实存在的数（`RateLimitPolicy.max_spend_micro_usd`），
 * 不是编出来的刻度。用记账的说法写出来，这句话就是一行余额。
 *
 * 没配上限就不结这笔账：写明未设上限，不去编一个余额。给一个不存在的
 * 上限算差额，比不算更糟。
 */

/** 窗口的中文说法。界面通篇中文，账页上夹一个 `DAY` 是把配置字段端到人眼前。 */
const WIN: Record<string, { per: string; span: string }> = {
  day: { per: "本日", span: "近 24 小时" },
  hour: { per: "本时", span: "近 1 小时" },
  minute: { per: "本分钟", span: "近 1 分钟" },
};

/** 把金额拆成「前导零」和「有效位」—— 见 Entry 里的说明。 */
function splitLead(text: string): [string, string] {
  const m = /^([^1-9]*)(.*)$/.exec(text);
  return m ? [m[1] ?? "", m[2] ?? ""] : ["", text];
}

function Figure({ text, className }: { text: string; className?: string }) {
  const [lead, sig] = splitLead(text);
  return (
    <span className={`val${className ? ` ${className}` : ""}`}>
      {lead && <span className="lead">{lead}</span>}
      {sig}
    </span>
  );
}

export function Balance({
  spendMicro,
  ceilingMicro,
  window: win,
}: {
  /** 窗口内已花。null = 还没读到。 */
  spendMicro: number | null;
  /** 配置里的上限总额。null = 没有配 —— 此时不结账。 */
  ceilingMicro: number | null;
  /** 读数覆盖的窗口。没配上限时也要说清 —— 不带窗口的花费数字读不出意思。 */
  window: string | null;
}) {
  const w = WIN[win ?? "day"] ?? WIN.day!;
  const pending = spendMicro === null;
  const remaining =
    ceilingMicro !== null && spendMicro !== null ? ceilingMicro - spendMicro : null;
  const over = remaining !== null && remaining < 0;

  return (
    <div className="balance">
      <div className="entries">
        {ceilingMicro !== null && (
          <div className="entry">
            <span className="lab">{w.per}上限</span>
            <Figure text={fmtUsd(ceilingMicro)} />
          </div>
        )}
        <div className="entry">
          <span className="lab">已花费</span>
          <Figure text={pending ? "—" : fmtUsd(spendMicro)} className={pending ? "absent" : ""} />
          <span className="foot">{w.span}，按模型定价折算</span>
        </div>
      </div>

      {remaining === null ? (
        <p className="balance-note">
          {ceilingMicro === null
            ? "未设花费上限，这笔账不结。要看余额，先在「限流与预算」里给密钥配一条上限。"
            : "读不到窗口内花费，余额无法结出。"}
        </p>
      ) : (
        <>
          <div className="balance-rule" />
          {/* 超支不额外加负号：标签已经写着「超支」，数字又是红墨 ——
              再来一个 −$1.48 会读成双重否定。会计记负数靠红墨或括号，
              这里两样都有了。 */}
          <div className="balance-sum">
            <span className="lab">{over ? "超支" : "余额"}</span>
            <Figure text={fmtUsd(Math.abs(remaining))} />
            <span className="foot">
              {over
                ? "已经越过上限。网关按已记录的花费收口，仍在途的请求可能再溢出一点。"
                : `距${w.per}上限还剩这些。`}
            </span>
          </div>
        </>
      )}
    </div>
  );
}
