//! The `heapq` standard-library module: a binary min-heap maintained inside an
//! ordinary Python list, ported from CPython's `Lib/heapq.py` (`_siftdown` /
//! `_siftup` and the public wrappers). Element ordering uses the host's value
//! comparison (`PyHost::compare`), the same `<` path `sorted()`/`min()` use, so
//! heterogeneous but comparable elements order exactly as CPython would.
//!
//! Mutating operations (`heappush`, `heappop`, `heapify`, `heapreplace`,
//! `heappushpop`) rewrite the *same* heap object in place, preserving its
//! identity (like `random.shuffle`): snapshot the list, run the algorithm on the
//! snapshot, then write it back through `get_mut`.
//!
//! Wiring (done by the parent): an `import_module` arm for `"heapq"` calling
//! [`entries`], and a `call_builtin_function` arm routing `heapq.*` to [`call`].

use crate::host::{type_error, PyHost, PyObj};
use fusevm::{NumOp, Value};

/// The module namespace: every callable is a first-class `PyObj::Builtin` handle
/// the VM invokes back through [`call`].
pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    [
        "heappush",
        "heappop",
        "heapify",
        "heappushpop",
        "heapreplace",
        "nlargest",
        "nsmallest",
    ]
    .iter()
    .map(|name| {
        (
            (*name).to_string(),
            h.alloc(PyObj::Builtin(format!("heapq.{name}"))),
        )
    })
    .collect()
}

/// Dispatch a `heapq.*` builtin. `fname` is already stripped of the `heapq.`
/// prefix. Returns `None` for names this module does not own.
pub fn call(h: &mut PyHost, fname: &str, args: &[Value]) -> Option<Result<Value, String>> {
    match fname {
        "heappush" => Some(heappush(h, args)),
        "heappop" => Some(heappop(h, args)),
        "heapify" => Some(heapify(h, args)),
        "heappushpop" => Some(heappushpop(h, args)),
        "heapreplace" => Some(heapreplace(h, args)),
        "nlargest" => Some(nlargest(h, args)),
        "nsmallest" => Some(nsmallest(h, args)),
        _ => None,
    }
}

// ── comparison + list access helpers ─────────────────────────────────────────

/// `a < b` via the host's ordering (the `sorted()`/`min()` comparison path).
fn lt(h: &mut PyHost, a: &Value, b: &Value) -> Result<bool, String> {
    let v = h.compare(NumOp::Lt, a, b)?;
    Ok(h.truthy(&v))
}

/// Snapshot the list argument at `args[idx]`, or a TypeError if it is not a list.
fn snapshot(h: &PyHost, args: &[Value], idx: usize, fname: &str) -> Result<Vec<Value>, String> {
    let v = args
        .get(idx)
        .ok_or_else(|| type_error(&format!("{fname} expected a heap argument")))?;
    match h.get(v) {
        Some(PyObj::List(l)) => Ok(l.clone()),
        _ => Err(type_error(&format!(
            "heap argument must be a list, not '{}'",
            h.type_name(v)
        ))),
    }
}

/// Write `heap` back into the same list object so its identity is preserved.
fn write_back(h: &mut PyHost, target: &Value, heap: Vec<Value>) {
    if let Some(PyObj::List(l)) = h.get_mut(target) {
        *l = heap;
    }
}

// ── the core sift operations (CPython Lib/heapq.py) ───────────────────────────

/// Restore the heap invariant after appending: bubble `heap[pos]` up towards
/// `startpos` while it is smaller than its parent.
fn siftdown(
    h: &mut PyHost,
    heap: &mut [Value],
    startpos: usize,
    mut pos: usize,
) -> Result<(), String> {
    let newitem = heap[pos].clone();
    while pos > startpos {
        let parentpos = (pos - 1) >> 1;
        if lt(h, &newitem, &heap[parentpos])? {
            heap[pos] = heap[parentpos].clone();
            pos = parentpos;
            continue;
        }
        break;
    }
    heap[pos] = newitem;
    Ok(())
}

/// Sift `heap[pos]` down: move the smaller child up until the leaf level, then
/// bubble the moved item back to its final resting position (CPython's fused
/// _siftup: it pushes to a leaf and then sift-downs from there).
fn siftup(h: &mut PyHost, heap: &mut [Value], mut pos: usize) -> Result<(), String> {
    let endpos = heap.len();
    let startpos = pos;
    let newitem = heap[pos].clone();
    let mut childpos = 2 * pos + 1;
    while childpos < endpos {
        let rightpos = childpos + 1;
        // Prefer the smaller child; ties keep the left child (as CPython does).
        if rightpos < endpos && !lt(h, &heap[childpos], &heap[rightpos])? {
            childpos = rightpos;
        }
        heap[pos] = heap[childpos].clone();
        pos = childpos;
        childpos = 2 * pos + 1;
    }
    heap[pos] = newitem;
    siftdown(h, heap, startpos, pos)
}

// ── public operations ────────────────────────────────────────────────────────

fn heappush(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let target = args
        .first()
        .cloned()
        .ok_or_else(|| type_error("heappush expected 2 arguments"))?;
    let item = args
        .get(1)
        .cloned()
        .ok_or_else(|| type_error("heappush expected 2 arguments"))?;
    let mut heap = snapshot(h, args, 0, "heappush")?;
    heap.push(item);
    let last = heap.len() - 1;
    siftdown(h, &mut heap, 0, last)?;
    write_back(h, &target, heap);
    Ok(Value::Undef)
}

fn heappop(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let target = args
        .first()
        .cloned()
        .ok_or_else(|| type_error("heappop expected 1 argument"))?;
    let mut heap = snapshot(h, args, 0, "heappop")?;
    let lastelt = match heap.pop() {
        Some(v) => v,
        None => return Err("IndexError: index out of range".into()),
    };
    let returnitem = if heap.is_empty() {
        lastelt
    } else {
        let top = heap[0].clone();
        heap[0] = lastelt;
        siftup(h, &mut heap, 0)?;
        top
    };
    write_back(h, &target, heap);
    Ok(returnitem)
}

fn heapify(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let target = args
        .first()
        .cloned()
        .ok_or_else(|| type_error("heapify expected 1 argument"))?;
    let mut heap = snapshot(h, args, 0, "heapify")?;
    // Transform bottom-up over the internal nodes (indices 0 .. n/2).
    for i in (0..heap.len() / 2).rev() {
        siftup(h, &mut heap, i)?;
    }
    write_back(h, &target, heap);
    Ok(Value::Undef)
}

fn heapreplace(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let target = args
        .first()
        .cloned()
        .ok_or_else(|| type_error("heapreplace expected 2 arguments"))?;
    let item = args
        .get(1)
        .cloned()
        .ok_or_else(|| type_error("heapreplace expected 2 arguments"))?;
    let mut heap = snapshot(h, args, 0, "heapreplace")?;
    if heap.is_empty() {
        return Err("IndexError: index out of range".into());
    }
    let returnitem = heap[0].clone();
    heap[0] = item;
    siftup(h, &mut heap, 0)?;
    write_back(h, &target, heap);
    Ok(returnitem)
}

fn heappushpop(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let target = args
        .first()
        .cloned()
        .ok_or_else(|| type_error("heappushpop expected 2 arguments"))?;
    let mut item = args
        .get(1)
        .cloned()
        .ok_or_else(|| type_error("heappushpop expected 2 arguments"))?;
    let mut heap = snapshot(h, args, 0, "heappushpop")?;
    // If the new item is larger than the smallest, swap it in and re-sift; the
    // old root becomes the return value. Otherwise the pushed item pops straight
    // back out and the heap is unchanged.
    if !heap.is_empty() && lt(h, &heap[0], &item)? {
        std::mem::swap(&mut item, &mut heap[0]);
        siftup(h, &mut heap, 0)?;
    }
    write_back(h, &target, heap);
    Ok(item)
}

// ── nlargest / nsmallest ─────────────────────────────────────────────────────

/// Sort `items` ascending in place using the host's `<` ordering (stable).
fn sort_ascending(h: &mut PyHost, items: &mut [Value]) -> Result<(), String> {
    let mut err: Option<String> = None;
    items.sort_by(|a, b| {
        if err.is_some() {
            return std::cmp::Ordering::Equal;
        }
        match h.compare(NumOp::Lt, a, b) {
            Ok(v) => {
                if h.truthy(&v) {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                }
            }
            Err(e) => {
                err = Some(e);
                std::cmp::Ordering::Equal
            }
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn nlargest(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let (n, iterable) = split_n_iterable(h, args, "nlargest")?;
    let mut items = h.iter_items(&iterable)?;
    if n <= 0 {
        return Ok(h.new_list(vec![]));
    }
    // Equivalent to sorted(iterable, reverse=True)[:n].
    sort_ascending(h, &mut items)?;
    items.reverse();
    items.truncate(n as usize);
    Ok(h.new_list(items))
}

fn nsmallest(h: &mut PyHost, args: &[Value]) -> Result<Value, String> {
    let (n, iterable) = split_n_iterable(h, args, "nsmallest")?;
    let mut items = h.iter_items(&iterable)?;
    if n <= 0 {
        return Ok(h.new_list(vec![]));
    }
    // Equivalent to sorted(iterable)[:n].
    sort_ascending(h, &mut items)?;
    items.truncate(n as usize);
    Ok(h.new_list(items))
}

fn split_n_iterable(h: &PyHost, args: &[Value], fname: &str) -> Result<(i64, Value), String> {
    let n = args
        .first()
        .and_then(|v| h.as_int(v))
        .ok_or_else(|| type_error(&format!("{fname}() n must be an integer")))?;
    let iterable = args
        .get(1)
        .cloned()
        .ok_or_else(|| type_error(&format!("{fname} expected 2 arguments")))?;
    Ok((n, iterable))
}
