import { useEffect, useState } from "react";
import { Msg, type Note } from "../components/Msg";
import type { DocState } from "../lib/useDoc";

export function Raw({ doc }: { doc: DocState }) {
  const [text, setText] = useState(doc.raw);
  /**
   * 因版本冲突没保存上的那份内容。
   *
   * 冲突之后编辑框必须换成磁盘上的最新内容：留着旧文本的话，下一次保存带的是新
   * 版本号，别人刚做的改动就被它整份盖掉了。可手上的改动不能跟着消失，所以单独
   * 留一份只读的，让人照着合进去。
   */
  const [draft, setDraft] = useState<string | null>(null);
  const [note, setNote] = useState<Note | null>(null);
  const [busy, setBusy] = useState(false);

  // 磁盘上的内容变了（保存、或别处重载）就跟着更新编辑框。
  useEffect(() => {
    setText(doc.raw);
  }, [doc.raw]);

  async function save() {
    // 与表单页同一道守卫。原文页更需要它：这里提交的是整份文件，一次过期
    // 覆盖抹掉的是别人所有的改动，不是某一条。
    if (!doc.doc || !doc.version) {
      setNote({
        text: "配置未能读取，拒绝保存：编辑框里是上一次成功载入的内容，保存会把此后的改动全部覆盖掉。",
        kind: "crit",
      });
      return;
    }
    setBusy(true);
    setNote({ text: "校验中…", kind: "ok" });
    const { ok, message, stale, unapplied } = await doc.saveRawText(text);
    setNote({ text: message, kind: !ok ? "crit" : unapplied ? "warn" : "ok" });
    setDraft(stale ? text : null);
    setBusy(false);
  }

  return (
    <div className="panel">
      <h2>resources.yaml</h2>
      <p className="hint">
        网关当前加载的声明式配置，可直接编辑。保存时先用网关自带的校验器过一遍——
        <strong>校验不通过就不落盘</strong>，线上配置不受影响。
        这是没有专用界面的那几种资源的编辑入口。
      </p>
      <textarea
        spellCheck={false}
        value={text}
        onChange={(e) => setText(e.target.value)}
        style={{
          width: "100%",
          minHeight: 440,
          fontFamily: "var(--mono)",
          fontSize: 12,
          lineHeight: 1.5,
          padding: 12,
          border: "1px solid var(--rule-2)",
          background: "var(--paper-2)",
          color: "var(--ink)",
          resize: "vertical",
        }}
      />
      <div style={{ display: "flex", gap: 8, marginTop: 12, flexWrap: "wrap" }}>
        <button className="act" disabled={busy} onClick={() => void save()}>
          {busy ? "校验中…" : "校验并保存"}
        </button>
        <button className="ghost" disabled={busy} onClick={() => void doc.reload()}>
          放弃修改，重新载入
        </button>
      </div>
      <div className="note">
        控制台保存时整体重写这个文件，所以手写的注释会消失。
      </div>
      <Msg note={note} />
      {draft !== null && (
        <div className="note warn">
          <strong>刚才的内容没有保存，这里留了一份。</strong>
          编辑框已换成磁盘上的最新内容，请把要改的部分重新合进去。
          <textarea
            readOnly
            aria-label="没有保存上的内容"
            spellCheck={false}
            value={draft}
            style={{
              width: "100%",
              minHeight: 200,
              marginTop: 8,
              fontFamily: "var(--mono)",
              fontSize: 12,
              background: "var(--paper-2)",
              color: "var(--ink)",
            }}
          />
          <button className="ghost" onClick={() => setDraft(null)}>
            不要了
          </button>
        </div>
      )}
    </div>
  );
}
