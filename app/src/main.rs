//! NXConvert. With no arguments it opens the window; with arguments it runs
//! the same commands as the headless `nxconvert` and never opens a window.
mod gui;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        console::release();
        if let Err(e) = gui::run() {
            eprintln!("ERROR: {e}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = nxformat_packer::cli::run(args, None, None) {
        eprintln!("ERROR: {e}");
        std::process::exit(1);
    }
}

/// The binary stays a console program, so scripts get its output and exit
/// code. Started from Explorer, Windows gives it a console of its own; close
/// that one before the window opens. A console shared with a shell stays.
#[cfg(windows)]
mod console {
    extern "system" {
        fn GetConsoleProcessList(list: *mut u32, count: u32) -> u32;
        fn FreeConsole() -> i32;
    }
    pub fn release() {
        let mut ids = [0u32; 2];
        // SAFETY: plain kernel32 calls with a valid buffer.
        unsafe {
            if GetConsoleProcessList(ids.as_mut_ptr(), 2) == 1 {
                FreeConsole();
            }
        }
    }
}
#[cfg(not(windows))]
mod console {
    pub fn release() {}
}
