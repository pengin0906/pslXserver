#!/usr/bin/env python3
"""pslx-proxy: X11 compression proxy for Xserver.

Runs on the REMOTE machine. Accepts plain X11 connections and forwards
them to Xserver via zstd-compressed TCP using a simple frame protocol:
  [4-byte LE length][zstd-compressed data]

Usage:
  python3 pslx_proxy.py <pslxserver_host> [--listen-port 6001] [--server-port 6100]

Then: DISPLAY=localhost:1 chromium --no-sandbox ...
Xserver: --tcp --compress-port 6100
"""

import asyncio
import struct
import sys

try:
    import zstandard as zstd
except ImportError:
    print("pip3 install zstandard", file=sys.stderr)
    sys.exit(1)

CHUNK = 512 * 1024


async def pipe_compress(reader, writer, label):
    """Read plain X11, compress each chunk as independent zstd frame, send with length prefix."""
    cctx = zstd.ZstdCompressor(level=1)
    try:
        while True:
            data = await reader.read(CHUNK)
            if not data:
                break
            compressed = cctx.compress(data)
            header = struct.pack('<I', len(compressed))
            writer.write(header + compressed)
            await writer.drain()
    except (ConnectionError, BrokenPipeError, asyncio.CancelledError, OSError):
        pass
    finally:
        try:
            if writer.can_write_eof():
                writer.write_eof()
            writer.close()
            await writer.wait_closed()
        except Exception:
            pass


async def read_exact(reader, n):
    """Read exactly n bytes."""
    buf = bytearray()
    while len(buf) < n:
        chunk = await reader.read(n - len(buf))
        if not chunk:
            return None
        buf.extend(chunk)
    return bytes(buf)


async def pipe_decompress(reader, writer, label):
    """Read length-prefixed zstd frames, decompress, send plain to client."""
    dctx = zstd.ZstdDecompressor()
    try:
        while True:
            header = await read_exact(reader, 4)
            if not header:
                break
            frame_len = struct.unpack('<I', header)[0]
            if frame_len == 0:
                continue
            compressed = await read_exact(reader, frame_len)
            if not compressed:
                break
            decompressed = dctx.decompress(compressed)
            writer.write(decompressed)
            await writer.drain()
    except (ConnectionError, BrokenPipeError, asyncio.CancelledError, OSError):
        pass
    finally:
        try:
            writer.close()
            await writer.wait_closed()
        except Exception:
            pass


async def handle_client(client_reader, client_writer, server_host, server_port):
    addr = client_writer.get_extra_info('peername')
    print(f"pslx-proxy: new client from {addr}", file=sys.stderr)

    try:
        server_reader, server_writer = await asyncio.open_connection(server_host, server_port)
    except Exception as e:
        print(f"pslx-proxy: cannot connect to {server_host}:{server_port}: {e}", file=sys.stderr)
        client_writer.close()
        return

    c2s = asyncio.create_task(pipe_compress(client_reader, server_writer, "c2s"))
    s2c = asyncio.create_task(pipe_decompress(server_reader, client_writer, "s2c"))

    done, pending = await asyncio.wait([c2s, s2c], return_when=asyncio.FIRST_COMPLETED)
    for t in pending:
        t.cancel()
    print(f"pslx-proxy: connection from {addr} closed", file=sys.stderr)


async def main():
    args = sys.argv[1:]
    if not args:
        print(__doc__, file=sys.stderr)
        sys.exit(1)

    server_host = args[0]
    listen_port = 6001
    server_port = 6100

    i = 1
    while i < len(args):
        if args[i] == '--listen-port':
            i += 1; listen_port = int(args[i])
        elif args[i] == '--server-port':
            i += 1; server_port = int(args[i])
        else:
            print(f"Unknown arg: {args[i]}", file=sys.stderr); sys.exit(1)
        i += 1

    server = await asyncio.start_server(
        lambda r, w: handle_client(r, w, server_host, server_port),
        '0.0.0.0', listen_port,
    )
    print(f"pslx-proxy: listening on :{listen_port} (DISPLAY=:{listen_port - 6000})", file=sys.stderr)
    print(f"pslx-proxy: forwarding to {server_host}:{server_port} with zstd compression", file=sys.stderr)

    async with server:
        await server.serve_forever()


if __name__ == '__main__':
    asyncio.run(main())
