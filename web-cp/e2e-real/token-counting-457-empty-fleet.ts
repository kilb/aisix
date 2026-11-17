import { spawn, type ChildProcess } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { ApiClient } from "../e2e/stack";

interface Ready { ready: true; httpUrl: string; cpDatabase: string; operator: { email: string; password: string } }
interface EmptyFleet { admin: ApiClient; cpDatabase: string; bootArchivedGateway(binary: string): Promise<void>; stop(): Promise<void> }

async function freePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (!address || typeof address === "string") return reject(new Error("No isolated listener port"));
      server.close(() => resolvePort(address.port));
    });
  });
}
function exited(child: ChildProcess, timeout: number): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) return Promise.resolve();
  return new Promise((resolveExit, reject) => {
    const timer = setTimeout(() => reject(new Error("An owned isolated test process did not exit")), timeout);
    child.once("exit", () => { clearTimeout(timer); resolveExit(); });
  });
}

// 复用正式真实栈启动脚本但不传 --gateway：先验证空舰队保存，再启动封存版本。
export async function startEmptyFleet(): Promise<EmptyFleet> {
  const processStack = spawn(process.execPath, ["../scripts/cp-dev-stack.mjs"], { stdio: ["pipe", "pipe", "pipe"] });
  let startup = "";
  processStack.stderr!.setEncoding("utf8");
  processStack.stderr!.on("data", (chunk: string) => {
    startup += chunk.split("\n").filter(line => line.startsWith("[cp-dev-stack]")).join("\n");
  });
  let gateway: ChildProcess | undefined;
  let runtimeDirectory: string | undefined;
  const stop = async (): Promise<void> => {
    if (gateway && gateway.exitCode === null && gateway.signalCode === null) {
      gateway.kill("SIGTERM");
      await exited(gateway, 20_000);
    }
    processStack.stdin!.end();
    await exited(processStack, 30_000);
    // 仅清理由本 helper 创建的临时运行目录，绝不触及封存二进制或用户目录。
    if (runtimeDirectory) rmSync(runtimeDirectory, { recursive: true, force: true });
  };
  try {
    const ready = await new Promise<Ready>((resolveReady, reject) => {
      const timer = setTimeout(() => reject(new Error("The real empty-fleet CP stack did not become ready")), 90_000);
      processStack.once("error", error => { clearTimeout(timer); reject(error); });
      processStack.once("exit", code => { clearTimeout(timer); reject(new Error(`The empty-fleet stack exited before readiness: ${code}\n${startup}`)); });
      let buffer = "";
      processStack.stdout!.setEncoding("utf8");
      processStack.stdout!.on("data", (chunk: string) => {
        buffer += chunk;
        const lines = buffer.split("\n");
        buffer = lines.pop() ?? "";
        for (const line of lines) {
          let parsed: unknown;
          try { parsed = JSON.parse(line); } catch (error) { if (error instanceof SyntaxError) continue; throw error; }
          if (parsed && typeof parsed === "object" && (parsed as { ready?: boolean }).ready === true) {
            clearTimeout(timer);
            resolveReady(parsed as Ready);
          }
        }
      });
    });
    const admin = new ApiClient(ready.httpUrl);
    await admin.post("/admin/login", ready.operator);
    return {
      admin,
      cpDatabase: ready.cpDatabase,
      async bootArchivedGateway(binary: string): Promise<void> {
        if (gateway) throw new Error("The archived gateway was already started");
        const issued = await admin.post("/admin/gateways/credentials", { name: "457-actual-old641" });
        runtimeDirectory = mkdtempSync(join(tmpdir(), "aisix-457-old641-"));
        const [proxy, metrics] = await Promise.all([freePort(), freePort()]);
        const env = Object.fromEntries(Object.entries(process.env).filter(([name, value]) => value !== undefined && !name.startsWith("AISIX_") && !/^(https?|all)_proxy$/i.test(name))) as Record<string, string>;
        for (const [name, value] of Object.entries(issued.env)) if (name !== "AISIX_CONFIG_PATH") env[name] = String(value);
        Object.assign(env, {
          AISIX_MANAGED__MTLS_DIR: join(runtimeDirectory, "mtls"),
          AISIX_MANAGED__DP_ID_FILE: join(runtimeDirectory, "dp-id"),
          AISIX_MANAGED__SNAPSHOT_CACHE_PATH: join(runtimeDirectory, "snapshot.json"),
          AISIX_MANAGED__HEARTBEAT_INTERVAL_SECS: "1",
          AISIX_PROXY__ADDR: `127.0.0.1:${proxy}`,
          AISIX_OBSERVABILITY__METRICS__PROMETHEUS__ADDR: `127.0.0.1:${metrics}`,
          AISIX_OBSERVABILITY__DEBUG__ENABLED: "false",
          AISIX_SHUTDOWN__MIN_DRAIN_SECS: "0",
          NO_PROXY: "127.0.0.1,localhost", no_proxy: "127.0.0.1,localhost",
        });
        gateway = spawn(binary, ["--config", resolve("../config.managed.yaml")], { env, stdio: ["ignore", "pipe", "pipe"] });
        gateway.stdout!.on("data", () => {});
        gateway.stderr!.on("data", () => {});
        await new Promise<void>((resolveSpawn, reject) => { gateway!.once("spawn", resolveSpawn); gateway!.once("error", reject); });
      },
      stop,
    };
  } catch (error) {
    await stop();
    throw error;
  }
}
