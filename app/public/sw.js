// Chat 最小 app-shell Service Worker（T1）：
// network-first；安装时预缓存外壳（index + /assets/*），运行中同源 GET 继续补缓存。
// 服务端停止/不可达时回缓存打开离线界面。/v1 API 一律不缓存（数据由 localStorage 快照兜底）。
const CACHE = "chat-shell-v1";

// 安装时抓一次 index.html，解析出构建产物地址一并缓存；
// 否则首访的资源都早于 SW 接管，缓存是空的，离线打开会失败
self.addEventListener("install", (e) => {
  e.waitUntil(
    (async () => {
      const cache = await caches.open(CACHE);
      try {
        const res = await fetch("/index.html", { cache: "no-store" });
        if (res.ok) {
          await Promise.all([cache.put("/index.html", res.clone()), cache.put("/", res.clone())]);
          const html = await res.text();
          const assets = [...html.matchAll(/(?:src|href)="(\/assets\/[^"]+)"/g)].map((m) => m[1]);
          await Promise.allSettled(assets.map((u) => cache.add(u)));
          // 资源名带内容哈希：上版缓存的 /assets/* 已经过期，清掉避免无限堆积
          const keep = new Set(["/", "/index.html", ...assets]);
          const reqs = await cache.keys();
          await Promise.allSettled(
            reqs.map((r) => (keep.has(new URL(r.url).pathname) ? null : cache.delete(r))),
          );
        }
      } catch {
        // 装的时候就断网：不阻塞安装，运行时缓存继续补
      }
      await self.skipWaiting();
    })(),
  );
});

self.addEventListener("activate", (e) => {
  e.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

async function cachedShell(request) {
  return (
    (await caches.match(request)) ??
    (await caches.match("/index.html")) ??
    (await caches.match("/")) ??
    null
  );
}

self.addEventListener("fetch", (e) => {
  const url = new URL(e.request.url);
  if (e.request.method !== "GET" || url.origin !== location.origin) return;
  if (url.pathname.startsWith("/v1/")) return;
  e.respondWith(
    fetch(e.request)
      .then(async (res) => {
        if (res.ok) {
          const copy = res.clone();
          caches
            .open(CACHE)
            .then((c) => c.put(e.request, copy))
            .catch(() => {});
          return res;
        }
        // 服务端 5xx 时导航请求也回退到缓存外壳，仍能进入离线界面
        if (e.request.mode === "navigate") return (await cachedShell(e.request)) ?? res;
        return res;
      })
      .catch(async () => {
        const hit = await caches.match(e.request);
        if (hit) return hit;
        if (e.request.mode === "navigate") return (await cachedShell(e.request)) ?? Response.error();
        return Response.error();
      }),
  );
});
