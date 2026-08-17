import { createHash } from "node:crypto";
import { createServer, type Server } from "node:http";
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import {
  decodedTextFor,
  EtcdClient,
  pickFreePort,
  SeedClient,
  spawnApp,
  startMockSls,
  startOpenAiUpstream,
  waitConfigPropagation,
  waitForToken,
  type MockSls,
  type OpenAiUpstream,
  type SpawnedApp,
} from "../harness/index.js";

const CALLER = "sk-sls-fail-open-nontext-input";
const CALLER_HASH = createHash("sha256").update(CALLER).digest("hex");
const CREDENTIAL_REF = "nontextfailopen";
const LOGSTORE = "full-events-nontext-fail-open";
const MODELS = {
  embeddings: "fail-open-input-embeddings",
  rerank: "fail-open-input-rerank",
  images: "fail-open-input-images",
  speech: "fail-open-input-speech",
  transcription: "fail-open-input-transcription",
};
const INPUTS = {
  embeddings: "uninspected-embeddings-input-17ac",
  rerank: "uninspected-rerank-input-28bd",
  images: "uninspected-images-input-39ce",
  speech: "uninspected-speech-input-4adf",
  transcription: "uninspected-transcription-input-5be0",
};
const REQUEST_IDS = {
  embeddings: "fail-open-input-embeddings-request-a196",
  rerank: "fail-open-input-rerank-request-b2a7",
  images: "fail-open-input-images-request-c3b8",
  speech: "fail-open-input-speech-request-d4c9",
  transcription: "fail-open-input-transcription-request-e5da",
};

interface FailingBedrock {
  url: string;
  requestBodies: string[];
  close(): Promise<void>;
}

async function startFailingBedrock(): Promise<FailingBedrock> {
  const mock = { requestBodies: [] as string[] } as FailingBedrock;
  const server: Server = createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (chunk: Buffer) => chunks.push(chunk));
    req.on("end", () => {
      mock.requestBodies.push(Buffer.concat(chunks).toString("utf8"));
      res.statusCode = 500;
      res.end("mock Bedrock outage");
    });
  });
  const port = await pickFreePort();
  await new Promise<void>((resolve) => server.listen(port, "127.0.0.1", resolve));
  mock.url = `http://127.0.0.1:${port}`;
  mock.close = async () => {
    await new Promise<void>((resolve, reject) => {
      server.close((error) => (error ? reject(error) : resolve()));
    });
  };
  return mock;
}

describe("SLS full-content capture after non-text input guardrail fail-open", () => {
  let etcdReachable = false;
  let app: SpawnedApp | undefined;
  let sls: MockSls | undefined;
  let bedrock: FailingBedrock | undefined;
  const upstreams: OpenAiUpstream[] = [];

  beforeAll(async () => {
    const etcd = new EtcdClient();
    etcdReachable = await etcd.ping();
    if (!etcdReachable) return;

    sls = await startMockSls();
    bedrock = await startFailingBedrock();
    upstreams.push(
      await startOpenAiUpstream({
        nonStreamBody: {
          object: "list",
          data: [{ object: "embedding", index: 0, embedding: [0.1, 0.2] }],
          model: "text-embedding-3-small",
          usage: { prompt_tokens: 2, total_tokens: 2 },
        },
      }),
      await startOpenAiUpstream({
        nonStreamBody: {
          id: "rerank-fail-open",
          results: [{ index: 0, relevance_score: 0.9 }],
          usage: { total_tokens: 2 },
        },
      }),
      await startOpenAiUpstream({
        nonStreamBody: { created: 1_700_000_000, data: [{ url: "https://img.example/ok.png" }] },
      }),
      await startOpenAiUpstream({ nonStreamBody: { audio: "placeholder" } }),
      await startOpenAiUpstream({ nonStreamBody: { text: "transcription succeeded" } }),
    );
    app = await spawnApp({
      extra: { bedrock_endpoint_url: bedrock.url },
      extraEnv: {
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_ID`]: "mock-akid",
        [`SLS_CRED_${CREDENTIAL_REF.toUpperCase()}_AK_SECRET`]: "mock-secret",
      },
    });
    const seed = new SeedClient(etcd, app.etcdPrefix);
    await seed.createObservabilityExporter({
      name: "sls-nontext-input-fail-open-full",
      enabled: true,
      kind: "aliyun_sls",
      endpoint: sls.url,
      project: "aisix-e2e-obs",
      logstore: LOGSTORE,
      credential_ref: CREDENTIAL_REF,
      content_mode: "full",
    });
    const modelNames = ["text-embedding-3-small", "rerank-english-v3", "dall-e-3", "tts-1", "whisper-1"];
    for (const [index, displayName] of Object.values(MODELS).entries()) {
      const pk = await seed.createProviderKey({
        display_name: `${displayName}-pk`,
        secret: "sk-mock",
        api_base: `${upstreams[index].baseUrl}/v1`,
      });
      await seed.createModel({
        display_name: displayName,
        provider: "openai",
        model_name: modelNames[index],
        provider_key_id: pk.id,
      });
    }
    await seed.createGuardrail({
      name: "bedrock-nontext-input-fail-open",
      enabled: true,
      hook_point: "input",
      fail_open: true,
      kind: "bedrock",
      guardrail_id: "failopengr0003",
      guardrail_version: "DRAFT",
      region: "us-east-1",
      aws_credentials: {
        kind: "static",
        access_key_id: "AKIDFAILOPEN0003",
        secret_access_key: "secret-input-fail-open",
      },
      latency_mode: { kind: "serial" },
      enforcement_mode: "enforce",
    });
    await seed.createApiKey({
      key_hash: CALLER_HASH,
      allowed_models: Object.values(MODELS),
    });
    await waitConfigPropagation(async () => {
      const response = await fetch(`${app!.proxyUrl}/v1/models`, {
        headers: { authorization: `Bearer ${CALLER}` },
      });
      if (response.status === 401) return false;
      if (response.status !== 200) {
        throw new Error(`model propagation probe returned ${response.status}`);
      }
      const body = (await response.json()) as { data?: Array<{ id?: string }> };
      const ids = new Set(body.data?.map((model) => model.id));
      return Object.values(MODELS).every((model) => ids.has(model));
    });
  });

  afterAll(async () => {
    await app?.exit();
    await Promise.all(upstreams.map((upstream) => upstream.close()));
    await bedrock?.close();
    await sls?.close();
  });

  test(
    "releases each request but omits every uninspected input from full-content logs",
    async (ctx) => {
      if (!etcdReachable || !app || !sls || !bedrock) {
        ctx.skip();
        return;
      }
      const beforeLogs = sls.requests.length;
      const postJson = (path: string, requestId: string, body: unknown) =>
        fetch(`${app!.proxyUrl}${path}`, {
          method: "POST",
          headers: {
            authorization: `Bearer ${CALLER}`,
            "content-type": "application/json",
            "x-aisix-request-id": requestId,
          },
          body: JSON.stringify(body),
        });

      const embeddings = await postJson("/v1/embeddings", REQUEST_IDS.embeddings, {
        model: MODELS.embeddings,
        input: INPUTS.embeddings,
      });
      expect(embeddings.status).toBe(200);
      await embeddings.text();

      const rerank = await postJson("/v1/rerank", REQUEST_IDS.rerank, {
        model: MODELS.rerank,
        query: INPUTS.rerank,
        documents: ["ordinary document"],
      });
      expect(rerank.status).toBe(200);
      await rerank.text();

      const images = await postJson("/v1/images/generations", REQUEST_IDS.images, {
        model: MODELS.images,
        prompt: INPUTS.images,
      });
      expect(images.status).toBe(200);
      await images.text();

      const speech = await postJson("/v1/audio/speech", REQUEST_IDS.speech, {
        model: MODELS.speech,
        input: INPUTS.speech,
        voice: "alloy",
      });
      expect(speech.status).toBe(200);
      await speech.arrayBuffer();

      const form = new FormData();
      form.set("model", MODELS.transcription);
      form.set("prompt", INPUTS.transcription);
      form.set("file", new Blob(["ID3fail-open-audio"], { type: "audio/mpeg" }), "a.mp3");
      const transcription = await fetch(`${app.proxyUrl}/v1/audio/transcriptions`, {
        method: "POST",
        headers: {
          authorization: `Bearer ${CALLER}`,
          "x-aisix-request-id": REQUEST_IDS.transcription,
        },
        body: form,
      });
      expect(transcription.status).toBe(200);
      await transcription.text();

      expect(upstreams.map((upstream) => upstream.receivedRequests.length)).toEqual([1, 1, 1, 1, 1]);
      const guardrailRequests = bedrock.requestBodies.join("\n");
      for (const input of Object.values(INPUTS)) {
        expect(guardrailRequests).toContain(input);
      }
      for (const requestId of Object.values(REQUEST_IDS)) {
        await waitForToken(sls, LOGSTORE, requestId, 15_000, beforeLogs);
      }
      const exported = decodedTextFor(sls, LOGSTORE, beforeLogs);
      for (const requestId of Object.values(REQUEST_IDS)) {
        expect(exported).toContain(requestId);
      }
      for (const input of Object.values(INPUTS)) {
        expect(exported).not.toContain(input);
      }
    },
    90_000,
  );
});
