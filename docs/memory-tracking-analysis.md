# Memory Tracking Analysis: Pages Outside used_pages, free_pages, and spans

## Summary

Memory can exist outside of `used_pages`, `free_pages`, and `spans` in some scenarios. This amount is small right now, but should be looked into afterwards. For example, 7 MB of memory is not tracked at the end of 100 iterations of ClickBench Q20.

=== Aggregated Memory Stats (All 56 CPUs) ===
Total pages tracked: 1299
Total segments owned: 56 (896 MB)
Memory in allocated pages: 428864 KB (418.81 MB)
Memory actually used: 426916 KB (416.91 MB)
Internal fragmentation: 1948 KB (1.90 MB, 0.5%)
Free memory in spans: 481472 KB (470.19 MB)
Total memory managed by all caches: 917504 KB (896.00 MB)
⚠️  Unaccounted memory: 7168 KB (7.00 MB, 0.8%) - This may indicate pages not tracked in free_pages/used_pages/spans

Bugs identified by Cursor earlier:

## 1. **Foreign frees not retired**

**Location:** `pool.rs` lines 171-174, `tcache.rs` cleanup_pages()

**Issue:** When a buffer is freed on a different CPU:
- The block is enqueued in the page's MPSC `thread_free_list`
- The page remains in `used_pages` or `free_pages` even if all blocks are freed
- The page is only retired when `cleanup_pages()` runs (every 128 allocations) or when the owning thread accesses it

**Impact:** Pages that are fully freed by other threads remain in `used_pages`/`free_pages` until cleanup runs. This is by design but can cause memory hoarding.

## 2. **Race conditions during allocation**

**Location:** `tcache.rs` `find_page_from_spans()` → `allocate()`

**Issue:** There's a brief window between:
1. Removing a slice from spans (`remove_slice_from_span`)
2. Setting `block_size` (making it allocated)
3. Adding it to `free_pages` or `used_pages`

During this window, the page is allocated but not in any tracking list. However, this is very brief and only matters if stats are collected during allocation.

## 3. **Pages in transition during retirement**

**Location:** `tcache.rs` `retire_page()` lines 199-233

**Issue:** When retiring a page:
1. It's removed from `used_pages`/`free_pages`
2. `block_size` is set to 0
3. It may be merged with adjacent free slices
4. Finally added to spans

During merging (lines 207-208, 221-224), slices are temporarily removed from spans, merged, and re-added. There's a brief moment where free slices exist but aren't in spans.

## 5. **Pages never added to tracking (potential bug)**

**Location:** Various allocation paths

**Issue:** If any allocation path:
- Calls `find_page_from_spans()` 
- Sets `block_size`
- But fails to add to `free_pages` or `used_pages` before returning or erroring

This would leave allocated pages untracked. The code should ensure all allocated pages are tracked.

## Recommendations

1. **Fix the large allocation bug** (#1) - this is the most critical issue
2. **Add assertions** to verify all allocated pages (block_size > 0) are in either `free_pages` or `used_pages`
3. **Consider tracking** pages during foreign frees more aggressively
4. **Add validation** in `collect_memory_stats()` to detect untracked allocated pages
