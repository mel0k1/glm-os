//! tree — v1.9: walk the persistent-disk DIRECTORY TREE from ring 3.
//!
//! Proof that the filesystem is genuinely hierarchical now: the program
//! lists a directory (SYS_FILE_LIST #42 with a path), then RECURSES into
//! every subdirectory record it meets, building child paths on the fly.
//! Output is an indented tree; the exit code is the total number of
//! entries visited across all levels (machine-checkable in the klog).
//!
//!   run TREE.ELF               -> walk from the task's cwd
//!   run TREE.ELF /HOME         -> walk /HOME
//!   run TREE.ELF /HOME/DOCS    -> walk a leaf directory

#![no_std]
#![no_main]

use glm_user::{arg_str, cstr_into, exit, file_list, file_record, fmt_u64, write};

const MAX_DEPTH: usize = 4;
const MAX_TOTAL: usize = 96;

/// One directory level. Every recursive frame owns its list buffer and
/// its child-path array, so no allocation is needed anywhere.
fn walk(path: &[u8], depth: usize, total: &mut usize) {
    if depth > MAX_DEPTH || *total >= MAX_TOTAL {
        return;
    }
    let mut buf = [0u8; 2048];
    let n = file_list(core::str::from_utf8(path).unwrap_or("/"), &mut buf, 32);
    if n <= 0 {
        return;
    }
    let mut off = 0usize;
    for _ in 0..n {
        if *total >= MAX_TOTAL {
            return;
        }
        let Some((kind, name, size, next)) = file_record(&buf, off) else {
            break;
        };
        off = next;
        *total += 1;

        for _ in 0..depth {
            write("  ");
        }
        let is_dir = kind == 1;
        write(if is_dir { " <DIR>  " } else { "        " });
        write(core::str::from_utf8(name).unwrap_or("(bad utf8)"));
        if !is_dir {
            write("  (");
            let mut nb = [0u8; 20];
            write(core::str::from_utf8(fmt_u64(size as u64, &mut nb)).unwrap_or("?"));
            write(" B)");
        }
        write("\n");

        if is_dir {
            // child path = path (trimmed of trailing '/') + '/' + name,
            // built inside THIS frame so the borrow outlives the call
            let mut child = [0u8; 128];
            let mut l = 0usize;
            let mut end = path.len();
            while end > 0 && path[end - 1] == b'/' {
                end -= 1;
            }
            if end > 0 {
                let c = usize::min(end, 127);
                child[..c].copy_from_slice(&path[..c]);
                l = c;
                if l < 127 {
                    child[l] = b'/';
                    l += 1;
                }
            }
            for (i, &b) in name.iter().enumerate() {
                if l + i >= 127 {
                    break;
                }
                child[l + i] = b;
            }
            let clen = usize::min(127, l + name.len());
            walk(&child[..clen], depth + 1, total);
        }
    }
}

#[no_mangle]
pub extern "C" fn _start(argc: i64, argv: *const *const u8) -> ! {
    let mut sbuf = [0u8; 128];
    let root: &str = if argc >= 2 {
        arg_str(cstr_into(unsafe { *argv.add(1) }, &mut sbuf))
    } else {
        ""
    };

    write("TREE: walking the persistent-disk tree from ring 3 (v1.9)\n");
    write("  root: ");
    write(if root.is_empty() { "(cwd)" } else { root });
    write("\n");

    let mut total = 0usize;
    walk(root.as_bytes(), 0, &mut total);

    write("  visited ");
    let mut tb = [0u8; 20];
    write(core::str::from_utf8(fmt_u64(total as u64, &mut tb)).unwrap_or("?"));
    write(" entries; exit code = that count\n");
    exit(total as i64);
}

#[panic_handler]
fn ph(_: &core::panic::PanicInfo) -> ! {
    exit(-2)
}
