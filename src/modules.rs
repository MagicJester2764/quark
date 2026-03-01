use crate::multiboot2::{self, ModuleInfo, MAX_MODULES};

static mut MODULE_COUNT: usize = 0;
static mut MODULES: [ModuleInfo; MAX_MODULES] = [ModuleInfo::empty(); MAX_MODULES];

/// Initialize the module registry from multiboot2 boot info.
///
/// # Safety
/// Must be called once with a valid multiboot2 info address.
pub unsafe fn init(info_addr: usize) {
    let (count, parsed) = multiboot2::parse_modules(info_addr);
    MODULE_COUNT = count;
    MODULES = parsed;
}

/// Number of loaded modules.
pub fn count() -> usize {
    unsafe { MODULE_COUNT }
}

/// Get a module by index.
pub fn get(index: usize) -> Option<&'static ModuleInfo> {
    if index < unsafe { MODULE_COUNT } {
        Some(unsafe { &MODULES[index] })
    } else {
        None
    }
}

/// Find a module by name prefix match.
pub fn find(name: &[u8]) -> Option<&'static ModuleInfo> {
    let count = unsafe { MODULE_COUNT };
    for i in 0..count {
        let m = unsafe { &MODULES[i] };
        if starts_with(&m.name, name) {
            return Some(m);
        }
    }
    None
}

/// Get the raw data of a module as a byte slice.
///
/// # Safety
/// The module's memory region must be valid and identity-mapped.
pub unsafe fn data(module: &ModuleInfo) -> &'static [u8] {
    let len = module.end - module.start;
    core::slice::from_raw_parts(module.start as *const u8, len)
}

/// Check if `haystack` starts with `needle` (stops at null in haystack).
fn starts_with(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    for i in 0..needle.len() {
        if haystack[i] == 0 || haystack[i] != needle[i] {
            return false;
        }
    }
    true
}

/// Get the name of a module as a byte slice (up to the null terminator).
pub fn name_str(module: &ModuleInfo) -> &[u8] {
    let mut len = 0;
    while len < module.name.len() && module.name[len] != 0 {
        len += 1;
    }
    &module.name[..len]
}
