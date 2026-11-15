//! 仅用于代理单元测试的线程内分配计数；不进入产品依赖。

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default)]
pub struct Allocations {
    pub calls: usize,
    pub bytes: usize,
}

thread_local! {
    static TRACKING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<Allocations> = const { Cell::new(Allocations { calls: 0, bytes: 0 }) };
}

pub struct TrackedAllocator;

fn allocated(bytes: usize) {
    let _ = TRACKING.try_with(|tracking| {
        if tracking.get() {
            let _ = ALLOCATIONS.try_with(|counter| {
                let mut value = counter.get();
                value.calls = value.calls.saturating_add(1);
                value.bytes = value.bytes.saturating_add(bytes);
                counter.set(value);
            });
        }
    });
}

// 仅由代理单元测试装配为全局分配器；关闭的其他线程仍使用原系统分配器。
unsafe impl GlobalAlloc for TrackedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        allocated(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        allocated(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        allocated(size);
        unsafe { System.realloc(pointer, layout, size) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
}

pub struct AllocationScope;

impl AllocationScope {
    pub fn start() -> Self {
        TRACKING.with(|enabled| assert!(!enabled.replace(true)));
        ALLOCATIONS.with(|counter| counter.set(Allocations::default()));
        Self
    }

    pub fn finish(self) -> Allocations {
        drop(self);
        ALLOCATIONS.with(Cell::get)
    }
}

impl Drop for AllocationScope {
    fn drop(&mut self) {
        TRACKING.with(|enabled| enabled.set(false));
    }
}
