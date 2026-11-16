// 只避免本次分配重复；释放的 socket 仍可能被另一个进程抢先占用。
export function uniquePorts(requested, freePort) {
  const used = new Map();
  // 提前保留显式端口，自动选择不能撞上稍后才启动的 listener。
  for (const [name, value] of Object.entries(requested)) {
    if (value === undefined) continue;
    const previous = used.get(value);
    if (previous !== undefined) {
      throw new Error(`--${previous} and --${name} both use port ${value}`);
    }
    used.set(value, name);
  }
  return async (name) => {
    if (requested[name] !== undefined) return requested[name];
    let value;
    do {
      value = await freePort();
    } while (used.has(value));
    used.set(value, name);
    return value;
  };
}
