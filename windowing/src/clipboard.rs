use crate::StrResultExt;
use arboard::Clipboard;

#[cfg(target_os = "linux")]
const CLIPBOARD_ARG: &str = "--serve-clipboard";

/// On sane systems, when you set the clipboard you hand the OS a
/// piece of data and the OS finds a safe location to store it for
/// programs to retrieve.
/// On linux, when you set the clipboard you actually register a
/// callback, and whenever a program wants to fetch the clipboard
/// data your callback is executed and returns the data.
/// This means that if the program that set the clipboard quits,
/// the clipboard contents are lost.
/// Therefore, to persistently set the clipboard, we spawn a
/// daemon process that safely serves the clipboard until its
/// contents are replaced by another program.
pub fn maybe_serve() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        use arboard::SetExtLinux;
        use std::env;
        use std::io::Read;

        if env::args().nth(1).as_deref() == Some(CLIPBOARD_ARG) {
            nix::unistd::setsid().map_err(|e| format!("could not call setsid(): {}", e))?;
            let mut text = String::new();
            std::io::stdin()
                .lock()
                .read_to_string(&mut text)
                .str_err()?;
            let textlen = text.len();
            println!(
                "clipboard daemon is serving {} bytes of text until clipboard contents are replaced",
                textlen
            );
            Clipboard::new()
                .str_err()?
                .set()
                .wait()
                .text(text)
                .str_err()?;
            // println!("stopped serving {} bytes of text", textlen);
            std::process::exit(0)
        }
    }
    Ok(())
}

pub fn set(text: &str) -> Result<(), String> {
    // The clipboard is very dumb on linux
    #[cfg(target_os = "linux")]
    {
        use std::env;
        use std::io::Write;
        let path = env::current_exe().str_err()?;
        let mut child = std::process::Command::new(path)
            .arg(CLIPBOARD_ARG)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .str_err()?;
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(text.as_bytes()).str_err()?;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Clipboard::new().str_err()?.set_text(text).str_err()?;
        Ok(())
    }
}

pub fn get() -> Result<Option<String>, String> {
    let text = match Clipboard::new().str_err()?.get_text() {
        Ok(text) => Some(text),
        Err(arboard::Error::ContentNotAvailable) => None,
        Err(err) => return Err(format!("{}", err)),
    };
    Ok(text)
}
