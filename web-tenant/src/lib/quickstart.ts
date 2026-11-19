const DEFAULT_GATEWAY = "https://api.routio.ai";
const PROMPT = "Say hello in one sentence.";
const ANTHROPIC_VERSION = "2023-06-01";

export const PROTOCOL_LABELS = {
  chat_completions: "OpenAI Chat Completions",
  messages: "Anthropic Messages",
  responses: "OpenAI Responses",
};

export type QuickstartProtocol = keyof typeof PROTOCOL_LABELS;

export function gatewayBase(configured?: string): string | null {
  if (configured === undefined) return DEFAULT_GATEWAY;
  try {
    const url = new URL(configured);
    if (url.protocol !== "http:" && url.protocol !== "https:") return null;
    const path = url.pathname.replace(/\/+$/, "").replace(/\/v1$/, "");
    return `${url.origin}${path}`;
  } catch {
    return null;
  }
}

function shellString(value: string): string {
  return `'${value.replaceAll("'", `'"'"'`)}'`;
}

export function quickstartExample(base: string, model: string, protocol: QuickstartProtocol) {
  const anthropic = protocol === "messages";
  const responses = protocol === "responses";
  const endpoint = protocol === "chat_completions" ? "chat/completions" : protocol;
  const messages = [{ role: "user", content: PROMPT }];
  const body = responses ? { model, input: PROMPT } : anthropic ? { model, max_tokens: 128, messages } : { model, messages };
  const auth = anthropic ? "x-api-key: $ROUTIO_API_KEY" : "Authorization: Bearer $ROUTIO_API_KEY";
  const curl = [
    `curl --globoff ${shellString(`${base}/v1/${endpoint}`)}`,
    `  -H "${auth}"`,
    ...(anthropic ? [`  -H "anthropic-version: ${ANTHROPIC_VERSION}"`] : []),
    '  -H "Content-Type: application/json"',
    `  -d ${shellString(JSON.stringify(body))}`,
  ].join(" \\\n");

  const sdk = anthropic ? "anthropic" : "openai";
  const client = anthropic ? "Anthropic" : "OpenAI";
  const sdkBase = anthropic ? base : `${base}/v1`;
  const method = responses ? "responses" : anthropic ? "messages" : "chat.completions";
  const input = responses ? `input=${JSON.stringify(PROMPT)}` : `messages=${JSON.stringify(messages)}`;
  const output = responses ? "output_text" : anthropic ? "content" : "choices[0].message.content";
  const python = [
    "import os",
    `from ${sdk} import ${client}`,
    "",
    `client = ${client}(`,
    `    base_url=${JSON.stringify(sdkBase)},`,
    '    api_key=os.environ["ROUTIO_API_KEY"],',
    ...(anthropic ? [`    default_headers={"anthropic-version": "${ANTHROPIC_VERSION}"},`] : []),
    ")",
    `result = client.${method}.create(`,
    `    model=${JSON.stringify(model)},`,
    ...(anthropic ? ["    max_tokens=128,"] : []),
    `    ${input},`,
    ")",
    `print(result.${output})`,
  ].join("\n");

  return { curl, python, sdk };
}
