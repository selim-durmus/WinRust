//! Who the per-user settings belong to. UAC can be answered with another administrator's password: the installer then runs as
//! that account, and its HKCU, `$PROFILE` and scheduled-task user are the wrong ones. The right account is the one whose
//! explorer.exe (or, with Explorer replaced, topbar / rust-explorer) runs in this session; its SID goes to every child in
//! WINRUST_SID, and `out()` aims `HKCU\...` at `HKU\<sid>` while it is set.
use crate::*;
use std::sync::OnceLock;

pub(crate) struct Session {
    pub sid: String,
    /// The elevated token belongs to someone else: only then is WINRUST_SID set, so the usual case keeps using plain HKCU.
    pub other: bool,
}

static SESSION: OnceLock<Result<Session, String>> = OnceLock::new();

const FIND: &str = r#"$s=(Get-Process -Id $PID).SessionId; $o=$null
foreach($n in 'explorer','topbar','rust-explorer','rs-shell'){
  $o=Get-CimInstance Win32_Process -Filter "Name='$n.exe'" | Where-Object { $_.SessionId -eq $s } | ForEach-Object { (Invoke-CimMethod -InputObject $_ -MethodName GetOwnerSid).Sid } | Where-Object { $_ } | Select-Object -First 1
  if($o){break}
}
if(-not $o){ exit 3 }
$o; [Security.Principal.WindowsIdentity]::GetCurrent().User.Value"#;

/// (session user's SID, elevated token's SID) from the two lines the script prints.
pub(crate) fn parse(out: &str) -> Result<(String, String), String> {
    let mut l = out.lines().map(str::trim).filter(|l| !l.is_empty());
    match (l.next(), l.next()) {
        (Some(a), Some(b)) if i18n::reg::valid_sid(a) && i18n::reg::valid_sid(b) => Ok((a.into(), b.into())),
        _ => Err(format!("could not determine the signed-in user: {}", out.trim())),
    }
}

fn detect() -> Result<Session, String> {
    let (sid, token) = parse(&out("powershell", &["-NoProfile", "-NonInteractive", "-Command", FIND]).map_err(|e| {
        format!("could not determine the signed-in user (no running explorer.exe, topbar or rust-explorer): {e}")
    })?)?;
    if matches!(sid.as_str(), "S-1-5-18" | "S-1-5-19" | "S-1-5-20") {
        return Err("the session shell does not run as a normal user (SYSTEM or a service)".into());
    }
    Ok(Session { other: sid != token, sid })
}

/// Settles the target user once and, when it is not the elevated account, tells the children. An Err means: change nothing.
pub(crate) fn init() -> Result<&'static Session, String> {
    let s = SESSION.get_or_init(detect).as_ref().map_err(Clone::clone)?;
    if s.other {
        log(&format!("elevated as another account: per-user settings go to {}", s.sid));
        unsafe { env::set_var(i18n::reg::SID_ENV, &s.sid) };
    }
    Ok(s)
}

/// Uninstall must work even when the user cannot be told (a broken session is when it is needed most): then the elevated
/// account's own HKCU is used and the failure becomes a warning.
pub(crate) fn init_soft() {
    if let Err(e) = init() {
        soft("signed-in user unknown, per-user settings are removed from the current account", Err(e));
    }
}

/// PowerShell: `$u` = the account scheduled tasks are registered for.
pub(crate) fn task_user() -> String {
    match env::var(i18n::reg::SID_ENV).ok().filter(|s| i18n::reg::valid_sid(s)) {
        Some(sid) => format!("$u=(New-Object Security.Principal.SecurityIdentifier '{sid}').Translate([Security.Principal.NTAccount]).Value;"),
        None => "$u=[Security.Principal.WindowsIdentity]::GetCurrent().Name;".into(),
    }
}

/// PowerShell: `$p` = this shell's CurrentUserCurrentHost profile of the target user (Documents may be redirected: the
/// User Shell Folders value of that user's hive says where).
pub(crate) fn profile_path() -> String {
    match env::var(i18n::reg::SID_ENV).ok().filter(|s| i18n::reg::valid_sid(s)) {
        Some(sid) => format!(
            r"$h=(Get-ItemProperty 'Registry::HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList\{sid}').ProfileImagePath; $d=(Get-ItemProperty 'Registry::HKEY_USERS\{sid}\Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders').Personal; $d=[Environment]::ExpandEnvironmentVariables($d.Replace('%USERPROFILE%',$h)); $p=Join-Path $d ($(if($PSVersionTable.PSEdition -eq 'Core'){{'PowerShell'}}else{{'WindowsPowerShell'}}) + '\Microsoft.PowerShell_profile.ps1');"
        ),
        None => "$p=$PROFILE;".into(),
    }
}
