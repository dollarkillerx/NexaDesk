//! Key combinations as an agent names them (`"cmd+shift+4"`, `"ctrl+c"`, `"enter"`)
//! to USB HID usages (keyboard page 0x07), the same codes `KeyPress` carries.
//!
//! The names are the host's: `cmd` is ⌘ on a Mac host and the Windows key on a
//! Windows host. No rules between this machine's keyboard and the host's apply
//! (`HeadlessDeps::rewrites_keys`).

/// Modifiers first, in the order given, then at most one other key.
pub fn parse(combo: &str) -> Result<Vec<u16>, String> {
    let combo = combo.trim();
    if combo.is_empty() {
        return Err("no keys given".into());
    }
    let mut mods = Vec::new();
    let mut key = None;
    // "ctrl++" and a lone "+": the last part is the plus key's base, "=".
    let parts: Vec<&str> = if combo == "+" {
        vec!["+"]
    } else if let Some(head) = combo.strip_suffix("++") {
        head.split('+').chain(std::iter::once("+")).collect()
    } else {
        combo.split('+').collect()
    };
    for part in parts {
        let name = part.trim().to_ascii_lowercase();
        if name.is_empty() {
            return Err(format!("\"{combo}\" has an empty part"));
        }
        if let Some(m) = modifier(&name) {
            if !mods.contains(&m) {
                mods.push(m);
            }
            continue;
        }
        let Some(k) = usage(&name) else {
            return Err(format!("unknown key \"{part}\""));
        };
        if key.replace(k).is_some() {
            return Err(format!(
                "\"{combo}\" names more than one key besides modifiers; send them one at a time"
            ));
        }
    }
    mods.extend(key);
    Ok(mods)
}

fn modifier(name: &str) -> Option<u16> {
    Some(match name {
        "ctrl" | "control" | "ctl" => 0xE0,
        "shift" => 0xE1,
        "alt" | "option" | "opt" => 0xE2,
        "cmd" | "command" | "meta" | "super" | "win" | "windows" => 0xE3,
        _ => return None,
    })
}

fn usage(name: &str) -> Option<u16> {
    let b = name.as_bytes();
    if b.len() == 1 {
        let c = b[0];
        return match c {
            b'a'..=b'z' => Some(0x04 + u16::from(c - b'a')),
            b'1'..=b'9' => Some(0x1E + u16::from(c - b'1')),
            b'0' => Some(0x27),
            b'-' => Some(0x2D),
            b'=' | b'+' => Some(0x2E),
            b'[' => Some(0x2F),
            b']' => Some(0x30),
            b'\\' => Some(0x31),
            b';' => Some(0x33),
            b'\'' => Some(0x34),
            b'`' => Some(0x35),
            b',' => Some(0x36),
            b'.' => Some(0x37),
            b'/' => Some(0x38),
            _ => None,
        };
    }
    if let Some(n) = name.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()) {
        return (1..=12).contains(&n).then(|| 0x3A + n - 1);
    }
    Some(match name {
        "enter" | "return" => 0x28,
        "esc" | "escape" => 0x29,
        "backspace" => 0x2A,
        "tab" => 0x2B,
        "space" => 0x2C,
        "minus" => 0x2D,
        "equal" | "equals" | "plus" => 0x2E,
        "capslock" => 0x39,
        "printscreen" | "prtsc" => 0x46,
        "insert" | "ins" => 0x49,
        "home" => 0x4A,
        "pageup" | "pgup" => 0x4B,
        "delete" | "del" | "forwarddelete" => 0x4C,
        "end" => 0x4D,
        "pagedown" | "pgdn" => 0x4E,
        "right" | "arrowright" => 0x4F,
        "left" | "arrowleft" => 0x50,
        "down" | "arrowdown" => 0x51,
        "up" | "arrowup" => 0x52,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combinations_put_modifiers_first() {
        assert_eq!(parse("cmd+shift+4").unwrap(), vec![0xE3, 0xE1, 0x21]);
        assert_eq!(parse("Ctrl + C").unwrap(), vec![0xE0, 0x06]);
        assert_eq!(parse("c+ctrl").unwrap(), vec![0xE0, 0x06]);
        assert_eq!(parse("alt+f4").unwrap(), vec![0xE2, 0x3D]);
        assert_eq!(parse("win").unwrap(), vec![0xE3]);
    }

    #[test]
    fn named_keys() {
        assert_eq!(parse("enter").unwrap(), vec![0x28]);
        assert_eq!(parse("Escape").unwrap(), vec![0x29]);
        assert_eq!(parse("f12").unwrap(), vec![0x45]);
        assert_eq!(parse("pagedown").unwrap(), vec![0x4E]);
        assert_eq!(parse("ctrl++").unwrap(), vec![0xE0, 0x2E]);
        assert_eq!(parse("+").unwrap(), vec![0x2E]);
        assert_eq!(parse("cmd+,").unwrap(), vec![0xE3, 0x36]);
    }

    #[test]
    fn mistakes_are_named() {
        assert!(parse("").is_err());
        assert!(parse("ctrl+").is_err());
        assert!(parse("hyper+a").unwrap_err().contains("hyper"));
        assert!(parse("f13").is_err());
        assert!(parse("a+b").unwrap_err().contains("more than one"));
    }
}
