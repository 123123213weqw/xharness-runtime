#!/usr/bin/env python3
"""白名单出口代理。

沙箱进程被 iptables 限制成只能连本端口；这里再按域名做二次过滤，
放行的才转发给 mihomo(127.0.0.1:7897) 出海。

为什么要两层：
- 内核层(owner --uid-owner)挡住 127.0.0.1:7897 / :9090 / :3082 / Tailscale / LAN，
  所以沙箱拿不到"全互联网 + 内网"的裸出口。
- 应用层(本文件)只放行代码托管与包管理器，避免被当成任意数据外泄通道。

残余风险（必须承认）：白名单内的域名仍可承载任意数据。若将来给沙箱配了
任何 GitHub/PyPI 的写凭据，这条通道就能外传文件。
"""

import asyncio
import os
import sys

LISTEN = ("127.0.0.1", 7898)
UPSTREAM = ("127.0.0.1", 7897)

# 后缀匹配：host == s 或 host 以 ".s" 结尾。
ALLOW = [s.strip().lower() for s in os.environ.get("WHITELIST", "").split(",") if s.strip()]

# 这些域名**直连**，不经 mihomo。
# 原因：实测 pypi.org 在所有节点上都不可达（000），而国内镜像是 200；
# 反过来 github 必须走节点。所以按域名分流，而不是一刀切。
DIRECT = [s.strip().lower() for s in os.environ.get("DIRECT", "").split(",") if s.strip()]

MAX_HEADER = 32 * 1024


def is_direct(host: str) -> bool:
    host = host.lower().rstrip(".")
    return any(host == s or host.endswith("." + s) for s in DIRECT)


def allowed(host: str) -> bool:
    host = host.lower().rstrip(".")
    return any(host == s or host.endswith("." + s) for s in ALLOW)


async def read_header(reader) -> bytes:
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = await reader.read(4096)
        if not chunk:
            return buf
        buf += chunk
        if len(buf) > MAX_HEADER:
            return b""
    return buf


def parse_target(head: bytes):
    """返回 (method, host, port) 或 None。"""
    try:
        line = head.split(b"\r\n", 1)[0].decode("latin-1")
    except Exception:
        return None
    parts = line.split()
    if len(parts) < 2:
        return None
    method, target = parts[0], parts[1]
    if method.upper() == "CONNECT":
        host, _, port = target.rpartition(":")
        if not host:
            host, port = target, "443"
        try:
            return method, host.strip("[]"), int(port)
        except ValueError:
            return None
    # 普通 HTTP：期望绝对 URI
    if "://" not in target:
        return None
    rest = target.split("://", 1)[1]
    hostport = rest.split("/", 1)[0]
    host, _, port = hostport.rpartition(":")
    if not host:
        host, port = hostport, "80"
    try:
        return method, host.strip("[]"), int(port)
    except ValueError:
        return None


async def pipe(reader, writer):
    try:
        while True:
            data = await reader.read(65536)
            if not data:
                break
            writer.write(data)
            await writer.drain()
    except Exception:
        pass
    finally:
        try:
            writer.close()
        except Exception:
            pass


async def handle(creader, cwriter):
    try:
        head = await read_header(creader)
        if not head:
            cwriter.close()
            return
        parsed = parse_target(head)
        if not parsed:
            cwriter.write(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
            await cwriter.drain()
            cwriter.close()
            return
        method, host, port = parsed
        if not allowed(host):
            print(f"deny {method} {host}:{port}", file=sys.stderr, flush=True)
            body = f"whitelist denied: {host}\n".encode()
            cwriter.write(b"HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: "
                          + str(len(body)).encode() + b"\r\nConnection: close\r\n\r\n" + body)
            await cwriter.drain()
            cwriter.close()
            return
        print(f"allow {method} {host}:{port}", file=sys.stderr, flush=True)

        if is_direct(host):
            ureader, uwriter = await asyncio.open_connection(host, port)
            if method.upper() == "CONNECT":
                cwriter.write(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                await cwriter.drain()
            else:
                uwriter.write(head)
                await uwriter.drain()
            await asyncio.gather(pipe(creader, uwriter), pipe(ureader, cwriter))
            return

        ureader, uwriter = await asyncio.open_connection(*UPSTREAM)
        if method.upper() == "CONNECT":
            uwriter.write(f"CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n".encode())
            await uwriter.drain()
            resp = await read_header(ureader)
            if not resp.startswith(b"HTTP/1.1 200") and not resp.startswith(b"HTTP/1.0 200"):
                cwriter.write(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                await cwriter.drain()
                cwriter.close()
                uwriter.close()
                return
            cwriter.write(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            await cwriter.drain()
        else:
            # 普通 HTTP 交给 mihomo 处理（它接受绝对 URI）
            uwriter.write(head)
            await uwriter.drain()

        await asyncio.gather(pipe(creader, uwriter), pipe(ureader, cwriter))
    except Exception as exc:  # noqa: BLE001
        print(f"error {exc!r}", file=sys.stderr, flush=True)
        try:
            cwriter.close()
        except Exception:
            pass


async def main():
    print(
        f"whitelist proxy on {LISTEN[0]}:{LISTEN[1]} -> {UPSTREAM}, "
        f"{len(ALLOW)} allowed, {len(DIRECT)} direct",
          file=sys.stderr, flush=True)
    server = await asyncio.start_server(handle, *LISTEN)
    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    asyncio.run(main())
