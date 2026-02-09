# Buffer Pool Memory Hoarding: Bugs and Design Flaws

This document summarizes potential bugs and design flaws in the fixed buffer pool and thread-local caches that can cause memory to be hoarded and not returned to the arena in a timely manner.

---

## 1. **Cleanup only on allocation (every 128 allocs)**

**Location:** `tcache.rs` lines 384–387

```rust
if self.stats.total_allocations & 0x7f == 0 {
    self.cleanup_pages();
}
```

**Issue:** `cleanup_pages()` is the only path that retires pages in `used_pages` that have been fully freed by **other** threads (via `collect_foreign_frees()` + `retire_page()`). It runs only when `total_allocations % 128 == 0`.

**Effect:** If a thread does a burst of allocations (many pages end up in `used_pages`), then other threads free all those buffers, the owning thread can keep those pages in `used_pages` until it performs 128 more allocations. If that thread becomes idle or allocates rarely, memory is hoarded indefinitely.

**Recommendation:** Run cleanup on the free path when a **foreign** free occurs (e.g. periodically or when `foreign_free` count passes a threshold), or run cleanup on a timer / background task, or reduce the 128 threshold and/or add a maximum age for pages in `used_pages`.

---

## 2. **Foreign frees never trigger retirement on the owning thread**

**Location:** `pool.rs` lines 171–174

```rust
} else {
    unsafe { (*page_ptr).foreign_free(ptr); }
    let pool = FIXED_BUFFER_POOL.get().unwrap();
    pool.foreign_free.fetch_add(1, Ordering::Relaxed);
}
```

**Issue:** When a buffer is freed on a different CPU from the one that owns the segment, the code only enqueues the block in the page’s MPSC `thread_free_list`. The owning thread is never notified. Retirement of that page happens only when the owner:

- Runs `cleanup_pages()` (every 128 allocations), or  
- Reuses that page in `find_page_from_used()` and then frees until the page is unused.

**Effect:** Memory can sit in a thread’s `used_pages` (or `free_pages`) long after all blocks have been freed by other threads, especially if the owning thread is idle or does few allocations.

**Recommendation:** Use `foreign_free` (e.g. the existing `foreign_free` counter) to trigger cleanup on the **freeing** thread (e.g. “if I’m doing a foreign free, try to signal or help the owner”) or on the **owning** thread (e.g. per-thread “recent foreign frees” and run cleanup when it exceeds a threshold). Alternatively, run cleanup on the free path when `sched_getcpu() != thread_id`.

---

## 3. **CPU-based cache indexing vs. thread identity**

**Location:** `pool.rs` lines 86–89, 92–94; `tcache.rs` line 341

```rust
// pool.rs
fn get_thread_local_cache() -> &'static Mutex<TCache> {
    let cpu = unsafe { libc::sched_getcpu() };
    &FIXED_BUFFER_POOL.get().unwrap().local_caches[cpu as usize]
}
// ...
let thread_id = unsafe { (*segment_ptr).thread_id };  // set at segment allocation
```

**Issue:** The “thread-local” cache is actually **CPU-local** (indexed by `sched_getcpu()`). Segments are tagged with `thread_id`, which is the **CPU index** at the time the segment was allocated. So:

- If a thread migrates to another CPU, later frees are treated as **foreign** (different CPU), so blocks go to the MPSC queue and the page is not retired on the free path.
- Segments stay associated with the CPU that first allocated them. A CPU that no longer runs the original allocating thread can still “own” segments whose buffers are freed elsewhere, and retirement depends on that CPU’s allocation rate.

**Effect:** Memory is tied to CPUs rather than to the logical threads that use it. After thread migration or load changes, memory can be stuck in caches of CPUs that rarely allocate, and foreign frees don’t trigger retirement there.

**Recommendation:** Consider true thread-local caches (e.g. thread id or `ThreadId`) so that ownership and cleanup are tied to the thread that actually allocates/frees, and consider triggering cleanup on “foreign” frees as in (2).

---

## 4. **Segments retained until fully empty**

**Location:** `tcache.rs` lines 192–196; `segment.rs` (segment size and retirement)

```rust
segment.allocated -= page_ref.slice_count;
if segment.allocated == 0 {
    self.retire_segment(segment_ptr);
    return;
}
```

**Issue:** A segment is returned to the arena only when `segment.allocated == 0`, i.e. when every slice in that segment has been retired. One segment is 32 MB. A thread can hold a 32 MB segment with only a single small allocation (e.g. one 4 KB block); the rest of the segment remains in `spans` as free slices available only to that thread.

**Effect:** One thread can hold many segments (e.g. 32 MB each) with very low utilization. Memory is not returned to the global arena for other threads until the owning thread frees everything in that segment and retirement runs.

**Recommendation:** Consider returning segments (or large contiguous slices) to the arena when utilization drops below a threshold (e.g. when most of the segment is free), or when the number of segments per cache exceeds a limit, so that high-capacity memory is shared instead of hoarded per cache.

---

## 5. **No bound on pages in `used_pages`**

**Location:** `tcache.rs` lines 69–70, 250–261, 384–387

**Issue:** `used_pages[size_class]` is a `Vec<*mut Page>` with no maximum length. Every time the single `free_pages[size_class]` page becomes full, that page is pushed onto `used_pages`. Entries are removed only when:

- The page is retired (after `collect_foreign_frees()` and `is_unused()` in `cleanup_pages()` or on local free), or  
- The page is chosen in `find_page_from_used()` and moved to `free_pages`.

**Effect:** A burst of allocations can grow `used_pages` quickly. If many of those buffers are freed by other threads, retirement is delayed until cleanup runs (see (1) and (2)). So the size of `used_pages` can grow without a cap and retain memory for a long time.

**Recommendation:** Run cleanup more aggressively (e.g. on foreign free path, or higher frequency), and/or enforce a maximum number of pages per size class in `used_pages` (e.g. retire or return to segment when over the limit).

---

## 6. **Large allocations always pushed to `used_pages[NUM_SIZE_CLASSES]`**

**Location:** `tcache.rs` lines 364–365, 377–378

```rust
self.used_pages[NUM_SIZE_CLASSES].push(free_page);
```

**Issue:** Every large allocation (> PAGE_SIZE) pushes the page to `used_pages[NUM_SIZE_CLASSES]`. There is no `free_pages` slot for large size classes, and no `find_page_from_used` path for large allocations. So large pages are only retired when they become unused and `cleanup_pages()` runs or a local free triggers `retire_page()`.

**Effect:** Same as (1) and (2): if all blocks in these large pages are freed by other threads, the owning thread keeps them until it does enough allocations to trigger cleanup (or reuses the page via some other path). Large pages (e.g. 64 KB or multi-page) can therefore be hoarded in `used_pages[NUM_SIZE_CLASSES]`.

**Recommendation:** Apply the same cleanup and foreign-free handling as for small size classes; consider triggering cleanup when large pages are freed from other threads.

---

## 7. **Design consideration: segment retirement invariant**

**Location:** `tcache.rs` `retire_page` → `retire_segment`

**Issue (original):** In the `used_pages` loop, when we call `retire_page(page)`, that page is removed from `used_pages[size_class]` inside `remove_page_from_used_queue()`. The loop does not increment `page_idx` when retiring, so the next iteration reads `used_pages[i][page_idx]` again, which is correct (the next element shifts down). However, `retire_page` can call `retire_segment`, which retires the whole segment. If any **other** page in `used_pages` (or `free_pages`) belongs to that segment, we would be holding a pointer into a segment that was just returned to the arena and could be reused. Those other pages are not updated or removed when the segment is retired.

**Effect:** Retiring a segment while the same cache still holds pointers to other pages in that segment in `used_pages` or `free_pages` can lead to use-after-free or corruption if the arena hands that segment to another thread. This is a more serious correctness bug if it can occur.

**Recommendation:** When retiring a segment, scan this cache’s `free_pages` and `used_pages` and remove (and optionally retire) any page that belongs to that segment, so no cache holds a pointer into a retired segment.

---

## Summary table

| # | Issue | Severity | Causes hoarding? |
|---|--------|----------|-------------------|
| 1 | Cleanup only every 128 allocs | High | Yes |
| 2 | Foreign frees don’t trigger cleanup | High | Yes |
| 3 | CPU vs thread identity | Medium | Yes (after migration) |
| 4 | Segments kept until fully empty | High | Yes (32 MB per segment) |
| 5 | No bound on `used_pages` | Medium | Yes (burst + foreign frees) |
| 6 | Large pages only retired via cleanup | Medium | Yes |
| 7 | Segment retirement invariant | Low (documentation) | No |

Recommended order of fixes: (1), (2), and (4) for hoarding; then (3), (5), and (6) as needed.
