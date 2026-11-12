//! Запуск консольных утилит из GUI-процесса без всплывающего окна.
//!
//! `tasklist`, `reg`, `clip` — консольные программы. Tauri-приложение собрано
//! с подсистемой Windows, поэтому каждый такой запуск без `CREATE_NO_WINDOW`
//! открывал отдельную консоль: она мигала при переходе в «Перенос чатов»,
//! где список харнессов спрашивает запущенные процессы.

use std::process::Command;

/// Открыть URL или deep link обработчиком из реестра. ShellExecuteW, а не
/// `rundll32 url.dll,FileProtocolHandler`: тот возвращает 0 и при неудаче, и
/// перенос считался доставленным в приложение, хотя ничего не открылось.
#[cfg(windows)]
pub fn shell_open(target: &str) -> bool {
    use windows::core::{w, HSTRING};
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let file = HSTRING::from(target);
    // SAFETY: все строки живут до конца вызова; окно-владелец не нужно.
    let instance = unsafe { ShellExecuteW(None, w!("open"), &file, None, None, SW_SHOWNORMAL) };
    // По контракту ShellExecute значение больше 32 — успех, меньше — код ошибки.
    instance.0 as isize > 32
}

#[cfg(not(windows))]
pub fn shell_open(_target: &str) -> bool {
    false
}

pub fn quiet(program: &str) -> Command {
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}
