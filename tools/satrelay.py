"""GEO satellite hop emulator (userspace TCP relay).

usage: satrelay.py LISTEN_PORT TARGET_HOST TARGET_PORT ONE_WAY_MS RATE_MBPS

- every byte is delivered ONE_WAY_MS later, in order
- a new connection cannot carry data before its TCP handshake would have
  completed over the satellite (1 RTT), like a real SYN/SYN-ACK
- each direction shares one token bucket (per-seat bandwidth cap)
"""
import asyncio
import sys
import time

LISTEN, THOST, TPORT = int(sys.argv[1]), sys.argv[2], int(sys.argv[3])
OW = float(sys.argv[4]) / 1000.0
RATE = float(sys.argv[5]) * 1e6 / 8  # bytes/s


class Bucket:
    def __init__(self, rate):
        self.rate, self.t = rate, time.monotonic()

    async def take(self, n):
        # virtual-clock serializer: next byte may leave at self.t
        now = time.monotonic()
        start = max(now, self.t)
        self.t = start + n / self.rate
        if self.t - now > 0:
            await asyncio.sleep(self.t - now)


UP, DOWN = Bucket(RATE), Bucket(RATE)


async def pipe(reader, writer, bucket, earliest):
    q = asyncio.Queue()

    async def deliver():
        last = 0.0
        while True:
            item = await q.get()
            if item is None:
                try:
                    writer.write_eof()
                except Exception:
                    pass
                return
            t, data = item
            t = max(t, last)
            last = t
            d = t - time.monotonic()
            if d > 0:
                await asyncio.sleep(d)
            await bucket.take(len(data))
            writer.write(data)
            await writer.drain()

    task = asyncio.create_task(deliver())
    try:
        while True:
            data = await reader.read(16384)
            if not data:
                break
            await q.put((max(time.monotonic() + OW, earliest), data))
    except Exception:
        pass
    await q.put(None)
    try:
        await task
    except Exception:
        pass


async def handle(cr, cw):
    t0 = time.monotonic()
    try:
        sr, sw = await asyncio.open_connection(THOST, TPORT)
    except Exception:
        cw.close()
        return
    # client data can only leave after SYN/SYN-ACK (1 RTT), then needs one-way to arrive
    up_earliest = t0 + 2 * OW + OW
    await asyncio.gather(pipe(cr, sw, UP, up_earliest), pipe(sr, cw, DOWN, 0.0))
    for w in (cw, sw):
        try:
            w.close()
        except Exception:
            pass


async def main():
    srv = await asyncio.start_server(handle, "127.0.0.1", LISTEN)
    async with srv:
        await srv.serve_forever()


asyncio.run(main())
