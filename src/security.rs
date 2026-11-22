use std::fs;
use std::io::{BufReader, Read, Seek};
use std::path::Path;
use zip::ZipArchive;

#[derive(Debug, Clone)]
pub struct Finding {
    pub location: String,
    pub reason: String,
    pub explanation: String,
}

#[derive(Debug, Clone, Default)]
pub struct AuditReport {
    pub findings: Vec<Finding>,
}

const BLOCKED_EXTENSIONS: &[&str] = &[
    "exe", "dll", "so", "dylib", "sh", "bash", "bat", "cmd", "ps1", "vbs", "msi", "appimage",
];

pub fn is_blocked_executable(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            BLOCKED_EXTENSIONS
                .iter()
                .any(|b| ext.eq_ignore_ascii_case(b))
        })
}

fn is_known_library_path(rel_path: &str) -> bool {
    let lower = rel_path.to_ascii_lowercase();
    lower.contains("/libs/") || lower.starts_with("libs/") || lower.contains("libstub")
}

fn has_heavy_byte_escapes(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut esc_count = 0usize;
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == b'\\'
            && (bytes[i + 1].is_ascii_digit()
                || (bytes[i + 1] == b'x' && bytes[i + 2].is_ascii_hexdigit()))
        {
            esc_count += 1;
            if esc_count >= 12 {
                return true;
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    false
}

fn has_encoded_string_char(line: &str) -> bool {
    let Some(pos) = line.find("string.char(") else {
        return false;
    };
    let rest = &line[pos + "string.char(".len()..];
    let args = rest.split(')').next().unwrap_or("");
    let comma_count = args.bytes().filter(|&b| b == b',').count();
    comma_count >= 8 && args.bytes().all(|b| b.is_ascii_digit() || b == b',' || b == b' ')
}

fn has_hardcoded_string_first_arg(line: &str, fn_name: &str) -> bool {
    let Some(pos) = line.find(fn_name) else {
        return false;
    };
    let after = line[pos + fn_name.len()..].trim_start();
    after.starts_with('"') || after.starts_with('\'')
}

fn has_hardcoded_whisper_target(line: &str) -> bool {
    let Some(pos) = line.find("SendChatMessage(") else {
        return false;
    };
    let args: Vec<&str> = line[pos + "SendChatMessage(".len()..]
        .split(',')
        .collect();
    let Some(chat_type) = args.get(1) else {
        return false;
    };
    let ct = chat_type.trim().to_ascii_uppercase();
    if ct != "\"WHISPER\"" && ct != "'WHISPER'" {
        return false;
    }
    args.get(3)
        .map(|a| a.trim())
        .is_some_and(|t| t.starts_with('"') || t.starts_with('\''))
}

pub fn scan_lua_source(rel_path: &str, content: &str, out: &mut Vec<Finding>) {
    let has_send_mail = content.contains("SendMail(");
    let has_set_money = content.contains("SetSendMailMoney(");
    let is_lib = is_known_library_path(rel_path);

    let mut file_has_obfuscation = false;
    for line in content.lines() {
        let t = line.trim();
        if !t.is_empty() && !t.starts_with("--") && (has_heavy_byte_escapes(t) || has_encoded_string_char(t)) {
            file_has_obfuscation = true;
            break;
        }
    }

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("--") {
            continue;
        }
        let loc = format!("{rel_path}:{}", idx + 1);

        if (trimmed.contains("loadstring(") || trimmed.contains("RunScript("))
            && (!is_lib || file_has_obfuscation)
        {
            out.push(Finding {
                location: loc.clone(),
                reason: "Dynamic Lua execution (loadstring/RunScript)".to_string(),
                explanation: "Executes runtime-built Lua strings outside a standard library; often used to hide malicious code from inspection.".to_string(),
            });
        }

        if has_heavy_byte_escapes(trimmed) || has_encoded_string_char(trimmed) {
            out.push(Finding {
                location: loc.clone(),
                reason: "Obfuscated Lua payload".to_string(),
                explanation: "Decodes hidden strings via byte escapes or string.char(...), a hallmark of trojanized WoW addons.".to_string(),
            });
        }

        if has_hardcoded_string_first_arg(trimmed, "SendMail(") {
            out.push(Finding {
                location: loc.clone(),
                reason: "Hardcoded SendMail recipient".to_string(),
                explanation: "Legitimate mail addons send to user-chosen variables; hardcoding a character name in SendMail() is a classic item/gold-stealer pattern.".to_string(),
            });
        }

        if trimmed.contains("SetSendMailMoney(") && has_send_mail {
            out.push(Finding {
                location: loc.clone(),
                reason: "Automated gold mail transfer (SetSendMailMoney + SendMail)".to_string(),
                explanation: "Attaches gold and sends mail in the same file, which can silently drain character gold when a mailbox is opened.".to_string(),
            });
        } else if trimmed.contains("WithdrawGuildBankMoney(") {
            out.push(Finding {
                location: loc.clone(),
                reason: "Programmatic guild bank withdrawal".to_string(),
                explanation: "Calls WithdrawGuildBankMoney() directly; verify this addon is not silently draining guild funds.".to_string(),
            });
        }

        if has_hardcoded_whisper_target(trimmed) {
            out.push(Finding {
                location: loc.clone(),
                reason: "Hardcoded whisper target (data exfiltration / spam)".to_string(),
                explanation: "Whispers a hardcoded character name via SendChatMessage, which can exfiltrate character/gold/bag data in-game.".to_string(),
            });
        }

        if trimmed.contains("AcceptTrade(") && has_set_money {
            out.push(Finding {
                location: loc,
                reason: "Auto-accept trade paired with gold transfer".to_string(),
                explanation: "Calls AcceptTrade() alongside gold-transfer APIs, a potential trade-scam pattern.".to_string(),
            });
        }
    }
}

pub fn audit_zip_reader<R: Read + Seek>(reader: R) -> Result<AuditReport, String> {
    let mut archive = ZipArchive::new(reader).map_err(|e| format!("Invalid zip archive: {e}"))?;
    let mut findings = Vec::new();

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| format!("Zip read error: {e}"))?;
        let Some(rel) = file.enclosed_name() else {
            continue;
        };
        if file.is_dir() {
            continue;
        }
        if is_blocked_executable(&rel) {
            return Err(format!(
                "Blocked suspicious executable/script in zip: {}",
                rel.display()
            ));
        }
        if rel
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("lua"))
        {
            let mut text = String::new();
            let _ = file.by_ref().take(2 * 1024 * 1024).read_to_string(&mut text);
            scan_lua_source(&rel.display().to_string(), &text, &mut findings);
        }
    }

    Ok(AuditReport { findings })
}

pub fn audit_zip_file(zip_path: &Path) -> Result<AuditReport, String> {
    let file = fs::File::open(zip_path)
        .map_err(|e| format!("Failed opening zip {}: {e}", zip_path.display()))?;
    audit_zip_reader(BufReader::new(file))
}

pub fn audit_installed_folders(addons_dir: &Path, folders: &[String]) -> AuditReport {
    let mut findings = Vec::new();
    for folder in folders {
        let root = addons_dir.join(folder);
        if root.is_dir() {
            walk_dir(&root, folder, &mut findings);
        }
    }
    AuditReport { findings }
}

fn walk_dir(dir: &Path, prefix: &str, findings: &mut Vec<Finding>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let rel = format!("{prefix}/{name}");
        if path.is_dir() {
            walk_dir(&path, &rel, findings);
        } else if path.is_file() {
            if is_blocked_executable(&path) {
                findings.push(Finding {
                    location: rel,
                    reason: "Executable or script file inside AddOns".to_string(),
                    explanation: "WoW addons only need .lua/.toc/.xml/media files and should never ship native binaries or OS scripts.".to_string(),
                });
            } else if path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("lua"))
                && let Ok(f) = fs::File::open(&path)
            {
                let mut text = String::new();
                let _ = f.take(2 * 1024 * 1024).read_to_string(&mut text);
                scan_lua_source(&rel, &text, findings);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    #[test]
    fn blocks_executables_in_zip() {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            zw.start_file("BadAddon/payload.exe", SimpleFileOptions::default())
                .unwrap();
            zw.write_all(b"MZ").unwrap();
            zw.finish().unwrap();
        }
        let err = audit_zip_reader(Cursor::new(buf.into_inner())).unwrap_err();
        assert!(err.contains("Blocked suspicious executable"));
    }

    #[test]
    fn flags_contextual_malicious_patterns_without_noise() {
        let mut findings = Vec::new();
        let code = r#"
-- safe comment loadstring("ignored")
SendChatMessage("Pulling boss!", "RAID")
SendChatMessage("Ready!", "WHISPER", nil, targetPlayer)
SendChatMessage("WHISPER me for invite", "RAID", nil, "x")
SendMail(recipientVar, "Flasks", "")
local f = loadstring("\108\111\097\100\115\116\114\105\110\103\040\041")
SendMail("GoldMule", "Gift", "")
SetSendMailMoney(1000000)
SendChatMessage(GetMoney(), "WHISPER", nil, "AttackerName")
"#;
        scan_lua_source("Test/Core.lua", code, &mut findings);
        assert_eq!(findings.len(), 5);
    }
}
