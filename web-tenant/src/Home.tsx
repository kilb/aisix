import { useEffect, useId, useState } from "react";
import { ArrowDown, ArrowRight, ChevronDown, Search } from "lucide-react";
import routingArtwork from "./assets/routio-optical-fibers.webp";
import { PriceDetails } from "./PriceDetails";
import { modelNameMatcher } from "./lib/modelNameMatcher";
import { fmtCnyPerMillion } from "./lib/money";
import { loadPublicPrices, type PublicPricedModel, type PublicPriceList } from "./lib/publicPrices";
import { fmtBeijing } from "./lib/time";
import "./theme/home-tokens.css";
import "./Prices.css";
import "./Home.css";

type PriceResult =
  | { status: "loading" | "error" }
  | { status: "ready"; prices: PublicPriceList };

interface HomeProps {
  homeHref: string;
  consoleHref: string;
  loginHref: string;
  signupHref: string;
  signedIn?: boolean;
}

const RATE_COLUMNS = [
  { key: "input_per_million", label: "输入" },
  { key: "output_per_million", label: "输出" },
  { key: "cache_read_per_million", label: "缓存读取" },
] as const;

function PriceRow({ model }: { model: PublicPricedModel }) {
  const [expanded, setExpanded] = useState(false);
  const detailId = useId();
  const tiers = model.prices_by_tier;
  const showDetails = tiers !== null && Object.keys(tiers).length > 0;
  return <tbody className="public-model-group">
    <tr className="public-model-row">
      <th scope="row">
        {showDetails
          ? <button
              type="button"
              className="public-model-toggle"
              aria-expanded={expanded}
              aria-controls={expanded ? detailId : undefined}
              onClick={() => setExpanded(!expanded)}
            >
              <ChevronDown size={15} aria-hidden="true" /><span>{model.name}</span>
            </button>
          : <span className="public-model-name">{model.name}</span>}
      </th>
      {RATE_COLUMNS.map(({ key }) => <td key={key}>
        {model[key] === null
          ? <><span className="public-rate-missing" aria-hidden="true">—</span><span className="vh">未提供价格</span></>
          : fmtCnyPerMillion(model[key])}
      </td>)}
    </tr>
    {expanded && tiers !== null && <tr className="public-model-details" id={detailId}>
      <td colSpan={RATE_COLUMNS.length + 1}>
        <PriceDetails tiers={tiers} rateDescription="公开标准单价" />
      </td>
    </tr>}
  </tbody>;
}

function PublicPricing() {
  const [search, setSearch] = useState("");
  const [attempt, setAttempt] = useState(0);
  const [result, setResult] = useState<PriceResult>({ status: "loading" });

  useEffect(() => {
    const request = new AbortController();
    loadPublicPrices(request.signal).then(
      (prices) => {
        if (!request.signal.aborted) setResult({ status: "ready", prices });
      },
      () => {
        if (!request.signal.aborted) setResult({ status: "error" });
      },
    );
    return () => request.abort();
  }, [attempt]);

  const quote = result.status === "ready" ? result.prices : null;
  const matches = modelNameMatcher(search);
  const models = quote?.models.filter((model) => matches(model.name)) ?? [];
  const searching = search.trim() !== "";

  function retry() {
    setResult({ status: "loading" });
    setAttempt((value) => value + 1);
  }

  return <section id="pricing" className="public-pricing" aria-labelledby="public-pricing-title">
    <div className="public-section-header">
      <div>
        <h2 id="public-pricing-title">模型价格</h2>
        <p>人民币元 / 百万 Token · 新组织标准价</p>
      </div>
      <label className="public-model-search">
        <Search size={17} aria-hidden="true" />
        <span className="vh">搜索模型</span>
        <input type="search" placeholder="搜索模型" value={search} onChange={(event) => setSearch(event.target.value)} />
      </label>
    </div>
    <div className="public-price-surface" aria-busy={result.status === "loading"}>
      {result.status === "loading" && <p className="public-price-message" role="status">正在加载模型价格…</p>}
      {result.status === "error" && <div className="public-price-message" role="alert">
        <p>模型价格暂时无法加载。</p>
        <button className="public-action public-action-subtle" type="button" onClick={retry}>重新加载</button>
      </div>}
      {quote !== null && !quote.available && <p className="public-price-notice" role="status">平台暂未发布新组织标准价。</p>}
      {quote !== null && (models.length === 0
        ? <div className="public-price-message" role="status">
            <p>{searching ? "没有找到匹配的模型。" : "暂无公开模型。"}</p>
            {searching && <button className="public-action public-action-subtle" type="button" onClick={() => setSearch("")}>清除搜索</button>}
          </div>
        : <table className="public-price-table" aria-label="公开模型标准价">
            <thead><tr>
              <th scope="col">模型</th>
              {RATE_COLUMNS.map(({ key, label }) => <th key={key} scope="col">{label}</th>)}
            </tr></thead>
            {models.map((model) => <PriceRow key={model.id} model={model} />)}
          </table>)}
    </div>
    {quote !== null && quote.available && quote.models.length > 0 && <div className="public-price-footer">
      <span>组织实际价格请登录后查看。未公布的单价以「—」表示。</span>
      <span>报价时刻 <time dateTime={quote.as_of}>{fmtBeijing(quote.as_of)}</time>（北京时间）</span>
    </div>}
  </section>;
}

export function Home({ homeHref, consoleHref, loginHref, signupHref, signedIn = false }: HomeProps) {
  return <div className="routio-public">
    <header className="public-topbar">
      <div className="public-container public-nav-layout">
        <a className="public-brand" href={homeHref}>
          <span className="brand-mark" aria-hidden="true" />
          <span>Routio</span>
        </a>
        <nav className="public-nav" aria-label="网站导航">
          <a href="#pricing">模型价格</a>
        </nav>
        <div className="public-nav-actions">
          {signedIn ? <a className="public-action public-action-primary" href={consoleHref}>进入控制台<ArrowRight size={16} aria-hidden="true" /></a> : <>
            <a className="public-action public-action-subtle" href={loginHref}>登录</a>
            <a className="public-action public-action-primary" href={signupHref}>注册<ArrowRight size={16} aria-hidden="true" /></a>
          </>}
        </div>
      </div>
    </header>
    <main className="public-container public-main">
      <section className="public-hero" aria-labelledby="public-home-title">
        <div className="public-hero-copy">
          <h1 id="public-home-title">统一接入 AI 模型</h1>
          <p>在这里比较公开模型价格。登录后创建 API 密钥，查看用量。</p>
          <div className="public-hero-actions">
            <a className="public-action public-action-primary" href="#pricing">查看模型价格<ArrowDown size={18} aria-hidden="true" /></a>
            {!signedIn && <a className="public-action public-action-secondary" href={signupHref}>
              注册组织<ArrowRight size={16} aria-hidden="true" />
            </a>}
          </div>
        </div>
        <img className="public-hero-art" src={routingArtwork} width={1200} height={900} alt="" decoding="async" />
      </section>
      <PublicPricing />
    </main>
  </div>;
}
