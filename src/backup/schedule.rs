//! Running a backup without opening a window, and registering that command with Task Scheduler.
use super::{Kind, Msg, NO_WINDOW, Places, TASK_NAME, Which};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}
fn command_args(config: &Path, source: &Path) -> Result<String, String> {
    let (config, source) = (config.to_string_lossy(), source.to_string_lossy());
    if [config.as_ref(), source.as_ref()].iter().any(|s| s.contains(['"', '\n', '\r'])) {
        return Err("The backup path contains a character Task Scheduler cannot use.".into());
    }
    let quote = |s: &str| format!("\"{}{}\"", s, "\\".repeat(s.chars().rev().take_while(|c| *c == '\\').count()));
    Ok(format!("--backup-run --backup-settings {} --source {}", quote(&config), quote(&source)))
}
fn action(exe: &Path, config: &Path, source: &Path) -> Result<String, String> {
    Ok(format!(
        "<Actions Context=\"Author\"><Exec><Command>{}</Command><Arguments>{}</Arguments></Exec></Actions>",
        escape(&exe.to_string_lossy()),
        escape(&command_args(config, source)?)
    ))
}
fn read_output(bytes: &[u8]) -> String {
    if bytes.starts_with(&[255, 254]) {
        String::from_utf16_lossy(&bytes[2..].chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect::<Vec<_>>())
    } else {
        String::from_utf8_lossy(bytes).trim_start_matches('\u{feff}').into()
    }
}
fn task_xml() -> Result<Option<String>, String> {
    let out = Command::new("schtasks.exe")
        .args(["/query", "/tn", TASK_NAME, "/xml"])
        .stdin(Stdio::null())
        .creation_flags(NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let list = Command::new("schtasks.exe")
            .args(["/query", "/fo", "csv", "/nh"])
            .stdin(Stdio::null())
            .creation_flags(NO_WINDOW)
            .output()
            .map_err(|e| e.to_string())?;
        if !list.status.success() || read_output(&list.stdout).contains(&format!("\"\\{TASK_NAME}\"")) {
            return Err("Windows could not read the existing backup schedule. It was left alone.".into());
        }
        return Ok(None);
    }
    Ok(Some(read_output(&out.stdout)))
}
fn replace_action(old: &str, action: &str) -> Result<String, String> {
    let re = regex::Regex::new(r"(?s)<Actions\b[^>]*>.*?</Actions>").unwrap();
    if re.find_iter(old).count() != 1 {
        return Err("The saved schedule could not be read. It was left alone.".into());
    }
    let actions = re.find(old).unwrap().as_str();
    let kinds = regex::Regex::new(r"<(Exec|ComHandler|SendEmail|ShowMessage)\b").unwrap();
    if kinds.find_iter(actions).count() != 1 || !actions.contains("<Exec>") {
        return Err("This schedule has custom actions. It was left alone; check it in Task Scheduler.".into());
    }
    let context = regex::Regex::new(r#"<Actions\b[^>]*\bContext="([^"]+)""#).unwrap();
    let replacement = if let Some(c) = context.captures(old) {
        action.replacen("Context=\"Author\"", &format!("Context=\"{}\"", &c[1]), 1)
    } else {
        action.replace(" Context=\"Author\"", "")
    };
    let xml = re.replace(old, regex::NoExpand(&replacement)).into_owned();
    // Save as UTF-8, even when schtasks returned UTF-16.
    Ok(xml.replace("encoding=\"UTF-16\"", "encoding=\"UTF-8\"").replace("encoding=\"utf-16\"", "encoding=\"UTF-8\""))
}
fn new_xml(action: &str, sid: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
<RegistrationInfo><Description>Back up music with Bradley's Music Tool.</Description></RegistrationInfo>
<Triggers><CalendarTrigger><StartBoundary>{}T03:00:00</StartBoundary><Enabled>true</Enabled><ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger></Triggers>
<Principals><Principal id="Author"><UserId>{}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><StartWhenAvailable>true</StartWhenAvailable><ExecutionTimeLimit>PT12H</ExecutionTimeLimit></Settings>
{action}</Task>"#,
        chrono::Local::now().date_naive(),
        escape(sid)
    )
}
pub(super) fn install(at: &Places, source: &Path) -> Result<String, String> {
    let saved = super::read_config(&at.config);
    if saved.dest.is_empty() {
        return Err("Choose the main backup folder first.".into());
    }
    super::native::validate_binding(at, Which::Main, source, Path::new(&saved.dest))?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let action = action(&exe, &at.config, source)?;
    if at.test {
        std::fs::create_dir_all(&at.logs).map_err(|e| e.to_string())?;
        std::fs::write(at.logs.join("nightly-task-preview.xml"), new_xml(&action, "S-1-5-21-0-0-0-1000")).map_err(|e| e.to_string())?;
        return Ok("Lab: saved the nightly command. No Windows task was changed.".into());
    }
    let old = task_xml()?;
    let xml = if let Some(old) = &old {
        replace_action(old, &action)?
    } else {
        let out = Command::new("whoami.exe")
            .args(["/user", "/fo", "csv", "/nh"])
            .stdin(Stdio::null())
            .creation_flags(NO_WINDOW)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err("Windows could not identify the account for the nightly task.".into());
        }
        let output = read_output(&out.stdout);
        let sid = regex::Regex::new(r"S-1-(?:\d+-)*\d+").unwrap().find(&output).ok_or("Windows did not return an account for the nightly task.")?.as_str();
        new_xml(&action, sid)
    };
    std::fs::create_dir_all(&at.logs).map_err(|e| e.to_string())?;
    if let Some(old) = &old {
        std::fs::write(at.logs.join(format!("nightly-task-before-{}.xml", chrono::Local::now().format("%Y%m%d-%H%M%S"))), old).map_err(|e| e.to_string())?;
    }
    let path = at.logs.join("nightly-task.xml");
    std::fs::write(&path, xml).map_err(|e| e.to_string())?;
    let mut create = Command::new("schtasks.exe");
    create.args(["/create", "/tn", TASK_NAME, "/xml"]).arg(&path);
    if old.is_some() {
        create.arg("/f");
    }
    let out = create.stdin(Stdio::null()).creation_flags(NO_WINDOW).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("The nightly task could not be changed: {}", read_output(&out.stderr).trim()));
    }
    let installed = task_xml()?.ok_or("The nightly task was not found after saving it.")?;
    if !installed.contains("--backup-run") || !installed.contains(&escape(&exe.to_string_lossy())) {
        return Err("The nightly task could not be verified. Check it in Task Scheduler.".into());
    }
    Ok(if old.is_some() {
        "Nightly backups now use this app. Their time and other schedule settings were kept.".into()
    } else {
        "Nightly backups are set for 3:00 AM, or the next sign-in if the PC was off.".into()
    })
}

pub(super) fn requested() -> Option<i32> {
    let args: Vec<_> = std::env::args_os().collect();
    if !args.iter().any(|a| a == "--backup-run") {
        return None;
    }
    let get = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(PathBuf::from);
    let result = (|| {
        let source = get("--source").ok_or("A backup command needs --source and --backup-settings.")?;
        let config = get("--backup-settings").ok_or("A backup command needs --source and --backup-settings.")?;
        if !source.is_absolute() || !config.is_absolute() {
            return Err("Backup commands need full folder paths.".into());
        }
        let logs = config.parent().ok_or("The backup settings path has no folder.")?.join("logs");
        let test = std::env::var_os("MUSIC_RECS_DATA").is_some_and(|s| !s.is_empty());
        if test {
            let expected = PathBuf::from(std::env::var_os("MUSIC_RECS_DATA").unwrap()).join("backup/config.json");
            if !config.to_string_lossy().replace('/', "\\").eq_ignore_ascii_case(&expected.to_string_lossy().replace('/', "\\")) {
                return Err("A lab command must use its own backup settings.".into());
            }
        }
        let at = Places { config, logs, test };
        let flag = |name: &str| args.iter().any(|a| a == name);
        if ["--preview", "--check-all", "--mirror"].iter().filter(|s| flag(s)).count() > 1 {
            return Err("Choose one backup action at a time.".into());
        }
        let which = if flag("--secondary") { Which::Second } else { Which::Main };
        let kind = if flag("--preview") {
            Kind::Preview
        } else if flag("--check-all") {
            Kind::CheckAll
        } else if flag("--mirror") {
            Kind::Mirror
        } else {
            Kind::Backup
        };
        super::native::execute(&at, which, &kind, &source, &AtomicBool::new(false), &mut |message| {
            if let Msg::Line(line) = message {
                println!("{line}");
            }
        })
    })();
    if let Err(why) = &result {
        eprintln!("{why}");
    }
    Some(if result.is_ok() { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn moving_a_nightly_task_to_the_app_keeps_its_trigger_and_settings() {
        let old = r#"<?xml version="1.0" encoding="UTF-16"?><Task><Triggers><At>04:15</At></Triggers><Settings><Enabled>false</Enabled></Settings><Actions Context="Owner"><Exec><Command>old-helper.exe</Command><Arguments>old</Arguments></Exec></Actions></Task>"#;
        let a = action(Path::new(r"C:\Apps & tools\Music Tool.exe"), Path::new(r"C:\Backup settings\config.json"), Path::new(r"M:\Music")).unwrap();
        let new = replace_action(old, &a).unwrap();
        assert!(new.contains("<At>04:15</At>") && new.contains("<Enabled>false</Enabled>"));
        assert!(new.contains("<Actions Context=\"Owner\">") && new.contains("--backup-run"));
        assert!(new.contains("Apps &amp; tools") && new.contains("&quot;C:\\Backup settings\\config.json&quot;"));
        assert!(!new.contains("old-helper") && !new.contains("UTF-16"));
        assert!(replace_action("<Task />", &a).is_err());
        assert!(replace_action(&old.replace("</Actions>", "<Exec><Command>other.exe</Command></Exec></Actions>"), &a).is_err());
        assert!(command_args(Path::new(r"C:\settings\config.json"), Path::new("M:\\")).unwrap().ends_with("\"M:\\\\\""));
    }
}
