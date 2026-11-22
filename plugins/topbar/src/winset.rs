//! Windows Settings pages for the Start search: name, `ms-settings:` link, and the words people search them by
//! ("add or remove programs" finds Installed apps). Windows' own settings index (ImmersiveControlPanel\Settings\
//! AllSystemSettings_*.xml) has no links to open its entries, so the list is kept here (Windows 11 page names).

/// (name as in Settings, link, search words separated by ';')
pub const PAGES: &[(&str, &str, &str)] = &[
    // System
    ("Display", "ms-settings:display", "brightness;resolution;scale;screen;monitor;refresh rate;orientation;multiple displays"),
    ("Night light", "ms-settings:nightlight", "blue light;warm colors;night mode"),
    ("HDR", "ms-settings:display-hdr", "high dynamic range;auto hdr"),
    ("Graphics", "ms-settings:display-advancedgraphics", "gpu;graphics performance;variable refresh rate;high performance"),
    ("Sound", "ms-settings:sound", "audio;volume;speakers;headphones;microphone;output device;input device"),
    ("Sound devices", "ms-settings:sound-devices", "audio devices;disable speakers;microphone devices"),
    ("Volume mixer", "ms-settings:apps-volume", "app volume;per app audio;sound mixer"),
    ("Notifications", "ms-settings:notifications", "do not disturb;alerts;banners;focus"),
    ("Power & battery", "ms-settings:powersleep", "sleep;screen timeout;turn off display;power mode;battery;energy saver;power options"),
    ("Storage", "ms-settings:storagesense", "disk space;drives;cleanup;temporary files;free up space;disk cleanup"),
    ("Storage Sense", "ms-settings:storagepolicies", "automatic cleanup;recycle bin cleanup"),
    ("Nearby sharing", "ms-settings:crossdevice", "share files;share between devices"),
    ("Multitasking", "ms-settings:multitasking", "snap windows;alt tab;virtual desktops;title bar shake"),
    ("For developers", "ms-settings:developers", "developer mode;sudo;end task;file explorer options"),
    ("Activation", "ms-settings:activation", "product key;license;activate windows;edition"),
    ("Troubleshoot", "ms-settings:troubleshoot", "troubleshooters;fix problems"),
    ("Recovery", "ms-settings:recovery", "reset this pc;factory reset;advanced startup;go back;reinstall windows"),
    ("Projecting to this PC", "ms-settings:project", "miracast;wireless display;cast"),
    ("Remote Desktop", "ms-settings:remotedesktop", "rdp;remote access"),
    ("Clipboard", "ms-settings:clipboard", "clipboard history;copy paste;sync clipboard"),
    ("About", "ms-settings:about", "pc name;rename this pc;device specifications;system info;windows version;ram;processor"),
    ("Optional features", "ms-settings:optionalfeatures", "add a feature;windows features;more windows features"),
    // Bluetooth & devices
    ("Bluetooth & devices", "ms-settings:bluetooth", "pair;add device;connect headphones;bluetooth"),
    ("Devices", "ms-settings:connecteddevices", "connected devices;device list"),
    ("Printers & scanners", "ms-settings:printers", "printer;scanner;print"),
    ("Phone Link", "ms-settings:mobile-devices", "mobile devices;phone;android;iphone"),
    ("Cameras", "ms-settings:camera", "webcam;camera settings"),
    ("Mouse", "ms-settings:mousetouchpad", "pointer speed;scroll;primary mouse button;mouse acceleration"),
    ("Touchpad", "ms-settings:devices-touchpad", "trackpad;gestures;tap to click"),
    ("Pen & Windows Ink", "ms-settings:pen", "stylus;handwriting"),
    ("AutoPlay", "ms-settings:autoplay", "removable drive;memory card"),
    ("USB", "ms-settings:usb", "usb notifications;usb battery saver"),
    ("Typing", "ms-settings:typing", "autocorrect;spelling;touch keyboard;text suggestions"),
    // Network & internet
    ("Network & internet", "ms-settings:network-status", "network status;internet connection;network"),
    ("Wi-Fi", "ms-settings:network-wifi", "wifi;wireless;wlan"),
    ("Manage known networks", "ms-settings:network-wifisettings", "saved networks;forget network;wifi passwords"),
    ("Ethernet", "ms-settings:network-ethernet", "wired;lan;cable;ip address;dns"),
    ("VPN", "ms-settings:network-vpn", "virtual private network"),
    ("Mobile hotspot", "ms-settings:network-mobilehotspot", "hotspot;tethering;share internet"),
    ("Airplane mode", "ms-settings:network-airplanemode", "flight mode"),
    ("Proxy", "ms-settings:network-proxy", "proxy server"),
    ("Advanced network settings", "ms-settings:network-advancedsettings", "network adapters;network reset;reset network;adapter options"),
    ("Data usage", "ms-settings:datausage", "data limit;metered connection"),
    // Personalization
    ("Personalization", "ms-settings:personalization", "customize;look and feel"),
    ("Background", "ms-settings:personalization-background", "wallpaper;desktop background;slideshow"),
    ("Colors", "ms-settings:personalization-colors", "dark mode;light mode;accent color;transparency effects;theme color"),
    ("Themes", "ms-settings:themes", "desktop icons;desktop icon settings;theme"),
    ("Lock screen", "ms-settings:lockscreen", "lock screen picture;screen saver"),
    ("Start menu", "ms-settings:personalization-start", "start;recommended;recently added apps"),
    ("Taskbar", "ms-settings:taskbar", "system tray;taskbar alignment;taskbar icons;hide taskbar"),
    ("Fonts", "ms-settings:fonts", "install fonts;typeface"),
    ("Device usage", "ms-settings:deviceusage", "tailored experiences"),
    // Apps
    ("Installed apps", "ms-settings:appsfeatures", "add or remove programs;uninstall;programs and features;apps & features;remove apps"),
    ("Default apps", "ms-settings:defaultapps", "default browser;file associations;open with;default programs"),
    ("Advanced app settings", "ms-settings:advanced-apps", "app execution aliases;choose where to get apps;archive apps"),
    ("Offline maps", "ms-settings:maps", "maps"),
    ("Apps for websites", "ms-settings:appsforwebsites", "open links in apps"),
    ("Video playback", "ms-settings:videoplayback", "hdr video;video quality"),
    ("Startup apps", "ms-settings:startupapps", "startup programs;autostart;run at startup;launch at login"),
    // Accounts
    ("Your info", "ms-settings:yourinfo", "account;profile picture;microsoft account"),
    ("Email & accounts", "ms-settings:emailandaccounts", "email;mail accounts;add account"),
    ("Sign-in options", "ms-settings:signinoptions", "password;pin;windows hello;face recognition;fingerprint;dynamic lock;change password"),
    ("Family", "ms-settings:family-group", "family safety;child account"),
    ("Other users", "ms-settings:otherusers", "add user;family & other users;guest;local account"),
    ("Windows Backup", "ms-settings:backup", "backup;onedrive backup;remember my apps"),
    ("Access work or school", "ms-settings:workplace", "work account;school account;mdm"),
    // Time & language
    ("Date & time", "ms-settings:dateandtime", "time zone;clock;set time;sync time"),
    ("Language & region", "ms-settings:regionlanguage", "display language;region;country;regional format;add language"),
    ("Advanced keyboard settings", "ms-settings:keyboard", "input method;language bar;switch input;keyboard layout"),
    ("Speech", "ms-settings:speech", "speech recognition;voice;text to speech"),
    // Gaming
    ("Game Mode", "ms-settings:gaming-gamemode", "gaming performance"),
    ("Game Bar", "ms-settings:gaming-gamebar", "xbox game bar"),
    ("Captures", "ms-settings:gaming-gamedvr", "screen recording;game clips;record"),
    // Accessibility
    ("Accessibility", "ms-settings:easeofaccess", "ease of access"),
    ("Text size", "ms-settings:easeofaccess-display", "font size;bigger text;make text bigger"),
    ("Visual effects", "ms-settings:easeofaccess-visualeffects", "animation effects;transparency effects;always show scrollbars"),
    ("Mouse pointer and touch", "ms-settings:easeofaccess-mousepointer", "cursor size;pointer size;pointer color"),
    ("Text cursor", "ms-settings:easeofaccess-cursor", "text cursor indicator;caret thickness"),
    ("Magnifier", "ms-settings:easeofaccess-magnifier", "zoom;magnify"),
    ("Color filters", "ms-settings:easeofaccess-colorfilter", "color blind;grayscale;invert colors"),
    ("Contrast themes", "ms-settings:easeofaccess-highcontrast", "high contrast"),
    ("Narrator", "ms-settings:easeofaccess-narrator", "screen reader"),
    ("Captions", "ms-settings:easeofaccess-closedcaptioning", "live captions;subtitles;closed captions"),
    ("Keyboard accessibility", "ms-settings:easeofaccess-keyboard", "sticky keys;filter keys;toggle keys;on-screen keyboard"),
    ("Mouse keys", "ms-settings:easeofaccess-mouse", "control mouse with keyboard"),
    // Privacy & security
    ("Privacy & security", "ms-settings:privacy", "privacy;permissions;app permissions"),
    ("Windows Security", "ms-settings:windowsdefender", "antivirus;defender;firewall;virus protection;security"),
    ("Find my device", "ms-settings:findmydevice", "lost device;locate"),
    ("Device encryption", "ms-settings:deviceencryption", "bitlocker;encrypt"),
    ("Location", "ms-settings:privacy-location", "location services;gps"),
    ("Camera permissions", "ms-settings:privacy-webcam", "camera access;webcam access"),
    ("Microphone permissions", "ms-settings:privacy-microphone", "microphone access;mic access"),
    ("Diagnostics & feedback", "ms-settings:privacy-feedback", "telemetry;diagnostic data;feedback frequency"),
    ("Activity history", "ms-settings:privacy-activityhistory", "timeline;history"),
    ("Search permissions", "ms-settings:search-permissions", "safesearch;cloud content search;search history"),
    ("Searching Windows", "ms-settings:cortana-windowssearch", "search index;indexer;indexing options;find my files"),
    ("General privacy", "ms-settings:privacy-general", "advertising id;ads"),
    // Windows Update
    ("Windows Update", "ms-settings:windowsupdate", "updates;check for updates;update windows;patch"),
    ("Update history", "ms-settings:windowsupdate-history", "installed updates;uninstall updates"),
    ("Advanced update options", "ms-settings:windowsupdate-options", "active hours;optional updates;restart options"),
    ("Delivery Optimization", "ms-settings:delivery-optimization", "download bandwidth;updates from other pcs"),
    ("Windows Insider Program", "ms-settings:windowsinsider", "insider;preview builds"),
];

/// The Settings app (its icon is shown on the settings rows).
pub const SETTINGS_APP: &str = r"shell:AppsFolder\windows.immersivecontrolpanel_cw5n1h2txyewy!microsoft.windows.immersivecontrolpanel";

pub fn is_page(path: &str) -> bool {
    path.get(..12).is_some_and(|h| h.eq_ignore_ascii_case("ms-settings:"))
}

/// The search words of a settings page, by its link.
pub fn keywords(link: &str) -> impl Iterator<Item = &'static str> {
    PAGES.iter().filter(move |p| p.1.eq_ignore_ascii_case(link)).flat_map(|p| p.2.split(';'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_are_well_formed() {
        let mut names = std::collections::HashSet::new();
        let mut links = std::collections::HashSet::new();
        for &(name, link, words) in PAGES {
            assert!(is_page(link) && link.len() > 12 && !link.contains(' '), "{link}");
            assert!(names.insert(name.to_lowercase()), "name twice: {name}");
            assert!(links.insert(link), "link twice: {link}");
            assert!(words.split(';').all(|w| !w.is_empty() && w == w.to_lowercase() && w.trim() == w), "{name}: {words}");
        }
    }

    #[test]
    fn keywords_by_link() {
        assert!(keywords("ms-settings:appsfeatures").any(|k| k == "add or remove programs"));
        assert!(keywords("MS-SETTINGS:APPSFEATURES").any(|k| k == "uninstall"));
        assert_eq!(keywords("ms-settings:nope").count(), 0);
        assert!(!is_page(r"C:\x.lnk") && is_page("ms-settings:display"));
    }
}
