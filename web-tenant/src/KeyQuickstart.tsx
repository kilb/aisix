import { useEffect, useState } from "react";
import { ApiError, call } from "./lib/api";
import { messageFor } from "./lib/errors";
import { PROTOCOL_LABELS, quickstartExample, type QuickstartProtocol } from "./lib/quickstart";
import { Button } from "./ui/Button";
import { Loading } from "./ui/DataTable";
import { Field } from "./ui/Field";
import { Select } from "./ui/Select";
import { useToast } from "./ui/Toast";
import "./KeyQuickstart.css";

export const QUICKSTART_TITLE = "首个 API 调用";

interface QuickstartModel {
  id: string;
  name: string;
  protocols: QuickstartProtocol[];
}

declare const __ROUTIO_GATEWAY_PUBLIC_URL__: string | null;

function CodeExample({ title, code, install }: { title: string; code: string; install?: string }) {
  const toast = useToast();
  const [copyError, setCopyError] = useState(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(code);
      setCopyError(false);
      toast("已复制示例");
    } catch {
      setCopyError(true);
    }
  }

  return <section className="key-quickstart-example" aria-label={title}>
    <div className="key-quickstart-example-header">
      <h3>{title}</h3>
      <Button size="sm" icon="copy" onClick={() => void copy()}>复制 {title}</Button>
    </div>
    {install && <p className="hint">安装 SDK：<code>{install}</code></p>}
    <pre tabIndex={0} aria-label={title}><code>{code}</code></pre>
    {copyError && <p className="field-problem" role="alert">无法写入剪贴板，请选中代码复制。</p>}
  </section>;
}

/** 示例只依赖当前权限和公开模型别名。 */
export function KeyQuickstart({ keyId }: { keyId: string }) {
  const base = __ROUTIO_GATEWAY_PUBLIC_URL__;
  const [result, setResult] = useState<{ items: QuickstartModel[] } | { error: string } | null>(null);
  const [attempt, retry] = useState(0);
  const [modelId, setModelId] = useState("");
  const [protocol, setProtocol] = useState<QuickstartProtocol>("chat_completions");

  useEffect(() => {
    if (!base) return;
    let live = true;
    void call<{ items: QuickstartModel[] }>("GET", `/keys/${encodeURIComponent(keyId)}/quickstart`).then(
      ({ data }) => {
        if (live) setResult({ items: data.items.filter(model => model.protocols.some(value => Object.hasOwn(PROTOCOL_LABELS, value))) });
      },
      (error: unknown) => {
        if (live) setResult({ error: error instanceof ApiError ? messageFor(error) : "模型读取失败，请重试。" });
      },
    );
    return () => { live = false; };
  }, [keyId, attempt, base]);

  if (!base) return <p role="alert" className="note crit">网关地址不可用，请联系平台运营。</p>;
  if (result === null) return <Loading />;
  if ("error" in result) return <div role="alert" className="note crit">
    {result.error}<Button onClick={() => { setResult(null); retry(value => value + 1); }}>重新读取模型</Button>
  </div>;
  const { items } = result;
  const model = items.find(item => item.id === modelId) ?? items[0];
  if (!model) return <p className="hint" role="status">当前密钥没有可用于文本示例的模型。请检查密钥的模型范围和分组。</p>;

  const protocols = model.protocols.filter(value => Object.hasOwn(PROTOCOL_LABELS, value));
  const selectedProtocol = protocols.includes(protocol) ? protocol : protocols[0];
  if (!selectedProtocol) return null;
  const example = quickstartExample(base, model.name, selectedProtocol);
  return <div className="key-quickstart">
    <p className="hint">从密钥列表复制密钥，在本机终端设为环境变量 <code>ROUTIO_API_KEY</code>，再运行下方示例。</p>
    <div className="key-quickstart-fields">
      <Field label="模型"><Select value={model.id} onValueChange={setModelId} options={items.map(item => ({ value: item.id, label: item.name }))} /></Field>
      <Field label="协议"><Select value={selectedProtocol} onValueChange={(value) => setProtocol(value as QuickstartProtocol)} options={protocols.map(value => ({ value, label: PROTOCOL_LABELS[value] }))} /></Field>
    </div>
    <CodeExample key={`${model.id}/${selectedProtocol}/curl`} title="cURL 示例" code={example.curl} />
    <CodeExample key={`${model.id}/${selectedProtocol}/python`} title="Python 示例" code={example.python} install={`pip install ${example.sdk}`} />
  </div>;
}
