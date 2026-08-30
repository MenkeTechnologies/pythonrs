"""`async`/`await`: coroutines, async iteration, and the async `with`.

Everything here is deterministic — `asyncio.sleep(0)` only yields control, so
the interleaving is fixed by the order the tasks were scheduled in rather than
by a clock.
"""

import asyncio


async def double(n):
    await asyncio.sleep(0)
    return n * 2


async def gathered():
    # `gather` returns results in ARGUMENT order, not completion order.
    return await asyncio.gather(*(double(i) for i in range(5)))


print(asyncio.run(gathered()))


async def ordered():
    """Awaiting in sequence: each coroutine finishes before the next starts."""
    out = []
    for i in range(3):
        out.append(await double(i))
    return out


print(asyncio.run(ordered()))


async def counter(n):
    """An async generator — iterated with `async for`, not `for`."""
    for i in range(n):
        await asyncio.sleep(0)
        yield i * i


async def consume():
    plain = []
    async for v in counter(4):
        plain.append(v)
    # An async comprehension does the same in one expression.
    comp = [v async for v in counter(4)]
    filtered = [v async for v in counter(5) if v % 2 == 0]
    return plain, comp, filtered


print(asyncio.run(consume()))


class Session:
    """The async context-manager protocol: `__aenter__` / `__aexit__`."""

    def __init__(self, name, log):
        self.name = name
        self.log = log

    async def __aenter__(self):
        await asyncio.sleep(0)
        self.log.append(f"enter {self.name}")
        return self.name

    async def __aexit__(self, exc_type, exc, tb):
        self.log.append(f"exit {self.name} ({exc_type.__name__ if exc_type else None})")
        # Falsy: an exception raised inside propagates.
        return False


async def with_sessions():
    log = []
    async with Session("outer", log) as outer:
        async with Session("inner", log) as inner:
            log.append(f"body {outer}/{inner}")
    # __aexit__ still runs when the body raises, innermost first.
    try:
        async with Session("failing", log):
            raise ValueError("inside")
    except ValueError as e:
        log.append(f"caught {e}")
    return log


for line in asyncio.run(with_sessions()):
    print(line)


async def cancelled():
    """A task cancelled before it finishes raises CancelledError at the await."""
    async def slow():
        try:
            await asyncio.sleep(3600)
        except asyncio.CancelledError:
            return "cancelled"
        return "finished"

    task = asyncio.ensure_future(slow())
    await asyncio.sleep(0)
    task.cancel()
    try:
        return await task
    except asyncio.CancelledError:
        return "propagated"


print(asyncio.run(cancelled()))


async def exceptions():
    """`gather(return_exceptions=True)` collects failures instead of raising."""
    async def boom(i):
        await asyncio.sleep(0)
        if i == 2:
            raise ValueError(f"bad {i}")
        return i

    results = await asyncio.gather(*(boom(i) for i in range(4)), return_exceptions=True)
    return [r if not isinstance(r, Exception) else f"{type(r).__name__}: {r}" for r in results]


print(asyncio.run(exceptions()))
