//! A surgical edit of one bottle environment variable. Do not reserialize the
//! entire CrossOver config: comments, formatting and unrelated settings matter.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::ops::Range;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const CONFIG_FILE: &str = "cxbottle.conf";
const ENV_SECTION: &str = "EnvironmentVariables";
const IGNORE_DEVICES: &str = "SDL_GAMECONTROLLER_IGNORE_DEVICES";
// Same ID used by PR #21 for the generic macOS controller forwarded by Wine.
const DUPLICATE_DEVICE: &str = "0x05AC/0x0004";
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub fn duplicate_xinput_hidden(bottle: &Path) -> io::Result<bool> {
    let Snapshot { config, .. } = read_config(&bottle.join(CONFIG_FILE))?;
    let parsed = parse_config(&config)?;
    Ok(parsed
        .setting
        .is_some_and(|setting| config[setting.value].split(',').any(is_duplicate_device)))
}

pub fn set_duplicate_xinput_hidden(bottle: &Path, hidden: bool) -> io::Result<()> {
    let path = bottle.join(CONFIG_FILE);
    let original = read_config(&path)?;
    let updated = update_config(&original.config, hidden)?;
    if updated == original.config {
        return Ok(());
    }
    atomic_save(&path, &original, &updated, &FileSave)
}

// Tests inject failures at the filesystem operations and exercise the same
// save sequence as the menu, including cleanup and the final recheck.
trait SaveOperations {
    fn create_temp(&self, directory: &Path) -> io::Result<TemporaryConfig>;
    fn write_and_sync(&self, file: &mut File, contents: &[u8]) -> io::Result<()>;
    fn replace(&self, temp: &Path, path: &Path) -> io::Result<()>;
}

struct FileSave;

impl SaveOperations for FileSave {
    fn create_temp(&self, directory: &Path) -> io::Result<TemporaryConfig> {
        for _ in 0..128 {
            let path = directory.join(format!(
                ".crosspuck-cxbottle-{}-{}.tmp",
                std::process::id(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok(TemporaryConfig { path, file }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Could not create a unique temporary bottle config",
        ))
    }

    fn write_and_sync(&self, file: &mut File, contents: &[u8]) -> io::Result<()> {
        file.write_all(contents)?;
        file.sync_all()
    }

    fn replace(&self, temp: &Path, path: &Path) -> io::Result<()> {
        fs::rename(temp, path)
    }
}

struct TemporaryConfig {
    path: PathBuf,
    file: File,
}

impl Drop for TemporaryConfig {
    fn drop(&mut self) {
        // After a successful rename this path no longer exists.
        let _ = fs::remove_file(&self.path);
    }
}

fn atomic_save(
    path: &Path,
    original: &Snapshot,
    updated: &str,
    operations: &impl SaveOperations,
) -> io::Result<()> {
    let mut temp = operations.create_temp(path.parent().unwrap_or(Path::new(".")))?;
    temp.file.set_permissions(original.metadata.permissions())?;
    operations.write_and_sync(&mut temp.file, updated.as_bytes())?;
    let current = read_config(path)?;
    if current.config != original.config
        || !same_file_version(&current.metadata, &original.metadata)
    {
        return Err(changed_config());
    }
    // This detects observed changes; without a shared CrossOver lock there is
    // still a race between this check and rename. Rename prevents partial saves.
    operations.replace(&temp.path, path)
}

struct Snapshot {
    config: String,
    metadata: fs::Metadata,
}

fn read_config(path: &Path) -> io::Result<Snapshot> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(invalid_config(
            "cxbottle.conf must be a regular file, not a symlink",
        ));
    }
    if metadata.permissions().readonly() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cxbottle.conf is read-only",
        ));
    }
    let mut file = File::open(path)?;
    if !same_file_version(&metadata, &file.metadata()?) {
        return Err(changed_config());
    }
    let mut config = String::new();
    file.read_to_string(&mut config)?;
    if !same_file_version(&metadata, &file.metadata()?)
        || !same_file_version(&metadata, &fs::symlink_metadata(path)?)
    {
        return Err(changed_config());
    }
    Ok(Snapshot { config, metadata })
}

fn same_file_version(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mode() == right.mode()
        && left.uid() == right.uid()
        && left.gid() == right.gid()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

fn changed_config() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "cxbottle.conf changed while editing; reopen the menu and try again",
    )
}

struct IgnoreSetting {
    value: Range<usize>,
}

struct Config {
    // Insert immediately after the existing section header, without moving
    // any other lines, or append a section when none exists.
    insert_at: Option<usize>,
    setting: Option<IgnoreSetting>,
}

fn parse_config(config: &str) -> io::Result<Config> {
    if config.starts_with('\u{feff}')
        || config.contains('\0')
        || config.replace("\r\n", "").contains('\r')
    {
        return Err(invalid_config(
            "Unsupported control characters in bottle config",
        ));
    }
    let mut parsed = Config {
        insert_at: None,
        setting: None,
    };
    let mut in_environment = false;
    let mut offset = 0;
    for line in config.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let text = body.trim_ascii();
        if text
            .chars()
            .next()
            .is_some_and(|ch| ch.is_whitespace() && !ch.is_ascii())
        {
            return Err(invalid_config("Unsupported whitespace in bottle config"));
        }
        if text.starts_with('[') {
            let end = text
                .find(']')
                .ok_or_else(|| invalid_config("Malformed section header"))?;
            let suffix = text[end + 1..].trim_ascii_start();
            // CXConfig uses a greedy section match; brackets in the comment
            // can change the effective name. Refuse this ambiguous spelling.
            if text[1..end].contains('[')
                || suffix.contains(']')
                || (!suffix.is_empty() && !suffix.starts_with(';'))
            {
                return Err(invalid_config("Unsupported section header syntax"));
            }
            in_environment = text[1..end].eq_ignore_ascii_case(ENV_SECTION);
            if in_environment && parsed.insert_at.replace(offset + line.len()).is_some() {
                return Err(invalid_config(
                    "Multiple EnvironmentVariables sections; edit them in CrossOver first",
                ));
            }
        } else if in_environment && !text.is_empty() && !text.starts_with(';') {
            if text.starts_with('#') {
                return Err(invalid_config(
                    "Unsupported # comment or environment variable name",
                ));
            }
            if let Some(equal) = body.find('=') {
                let key = body[..equal].trim_ascii();
                let quoted_key = key.starts_with('"');
                let key = if quoted_key {
                    key.strip_prefix('"')
                        .and_then(|key| key.strip_suffix('"'))
                        .ok_or_else(|| invalid_config("Malformed environment variable name"))?
                } else {
                    key
                };
                // Escaped names can alias the target after CXConfig unescapes
                // them. Preserve those assignments without guessing their key.
                if key.contains(['"', '\\']) || key.is_empty() {
                    return Err(invalid_config("Unsupported environment variable name"));
                }
                if key.trim().eq_ignore_ascii_case(IGNORE_DEVICES) {
                    // CrossOver compares keys without case but exports their
                    // original spelling; a lower-case env name is not SDL's hint.
                    if key != IGNORE_DEVICES || parsed.setting.is_some() {
                        return Err(invalid_config(
                            "Ambiguous SDL_GAMECONTROLLER_IGNORE_DEVICES setting",
                        ));
                    }
                    let start = equal + 1 + body[equal + 1..].len()
                        - body[equal + 1..].trim_ascii_start().len();
                    let value = &body[start..];
                    // CXConfig unquotes only when BOTH name and value are
                    // quoted. Mixed forms fall back to literal names/values.
                    if quoted_key != value.starts_with('"') {
                        return Err(invalid_config(
                            "Use either quoted name and value or unquoted name and value",
                        ));
                    }
                    let range = if let Some(quoted) = value.strip_prefix('"') {
                        let end = quoted
                            .find('"')
                            .ok_or_else(|| invalid_config("Unterminated ignore-devices value"))?;
                        let suffix = quoted[end + 1..].trim_ascii();
                        if !suffix.is_empty() && !suffix.starts_with(';') {
                            return Err(invalid_config("Unsupported ignore-devices value syntax"));
                        }
                        start + 1..start + 1 + end
                    } else {
                        start..start + value.trim_ascii_end().len()
                    };
                    validate_devices(&body[range.clone()])?;
                    parsed.setting = Some(IgnoreSetting {
                        value: offset + range.start..offset + range.end,
                    });
                }
            } else {
                return Err(invalid_config("Malformed environment variable assignment"));
            }
        }
        offset += line.len();
    }
    Ok(parsed)
}

fn validate_devices(value: &str) -> io::Result<()> {
    if value.trim_ascii_start().starts_with('@') {
        return Err(invalid_config(
            "The ignore list uses @file; manage it manually to preserve that file",
        ));
    }
    for device in value
        .split(',')
        .filter(|device| !device.trim_ascii().is_empty())
    {
        if device_ids(device).is_none() {
            return Err(invalid_config(
                "Unsupported ignore list; expected comma-separated 0xVID/0xPID pairs",
            ));
        }
    }
    Ok(())
}

fn device_ids(device: &str) -> Option<(u16, u16)> {
    let (vendor, product) = device.trim_ascii().split_once('/')?;
    let hex = |value: &str| {
        // SDL_LoadVIDPIDListFromHint scans for the literal lower-case prefix "0x".
        // https://github.com/libsdl-org/SDL/blob/SDL2/src/joystick/SDL_joystick.c Do not accept
        // "0X", decimal, overflow or partial numbers as equivalent IDs.
        let digits = value.trim_ascii().strip_prefix("0x")?;
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        u16::from_str_radix(digits, 16).ok()
    };
    Some((hex(vendor)?, hex(product)?))
}

fn is_duplicate_device(device: &str) -> bool {
    device_ids(device) == Some((0x05ac, 0x0004))
}

fn update_config(config: &str, hidden: bool) -> io::Result<String> {
    let parsed = parse_config(config)?;
    if let Some(setting) = parsed.setting {
        let value = &config[setting.value.clone()];
        let already_hidden = value.split(',').any(is_duplicate_device);
        if hidden == already_hidden {
            return Ok(config.to_string());
        }
        let updated_value = if hidden {
            let separator = if value.is_empty() { "" } else { "," };
            format!("{value}{separator}{DUPLICATE_DEVICE}")
        } else {
            // Remove all spellings of only our VID/PID. Keep every other entry,
            // its spacing, and the assignment's quoting/inline comment intact.
            value
                .split(',')
                .filter(|device| !is_duplicate_device(device))
                .collect::<Vec<_>>()
                .join(",")
        };
        let mut updated = config.to_string();
        updated.replace_range(setting.value, &updated_value);
        return Ok(updated);
    }
    if !hidden {
        return Ok(config.to_string());
    }

    let newline = if config.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let assignment = format!("\"{IGNORE_DEVICES}\" = \"{DUPLICATE_DEVICE}\"");
    let mut updated = config.to_string();
    if let Some(at) = parsed.insert_at {
        let prefix = if config[..at].ends_with('\n') {
            ""
        } else {
            newline
        };
        let suffix = if at < config.len() || config.ends_with('\n') {
            newline
        } else {
            ""
        };
        updated.insert_str(at, &format!("{prefix}{assignment}{suffix}"));
    } else {
        let separator = if config.is_empty() {
            String::new()
        } else if config.ends_with('\n') {
            newline.to_string()
        } else {
            newline.repeat(2)
        };
        let suffix = if config.is_empty() || config.ends_with('\n') {
            newline
        } else {
            ""
        };
        updated.push_str(&format!(
            "{separator}[{ENV_SECTION}]{newline}{assignment}{suffix}"
        ));
    }
    Ok(updated)
}

fn invalid_config(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TestBottle(PathBuf);
    impl TestBottle {
        fn new(config: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "crosspuck-config-test-{}-{}",
                std::process::id(),
                TEMP_ID.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::write(path.join(CONFIG_FILE), config).unwrap();
            Self(path)
        }
        fn read(&self) -> String {
            fs::read_to_string(self.0.join(CONFIG_FILE)).unwrap()
        }
    }
    impl Drop for TestBottle {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn preserves_existing_list_formatting_comments_and_other_sections() {
        let original = "[Bottle]\r\n\"Preview\" = \"1\"\r\n\r\n[EnvironmentVariables] ; settings\r\n;;\"SDL_GAMECONTROLLER_IGNORE_DEVICES\" = \"0x05AC/0x0004\"\r\n\"WINEMSYNC\" = \"1\"\r\n  \"SDL_GAMECONTROLLER_IGNORE_DEVICES\"=\"0x045E/0x028E, 0x054c/0x05c4 \" ; keep\r\n\r\n[Other]\r\n\"SDL_GAMECONTROLLER_IGNORE_DEVICES\" = \"0x05AC/0x0004\"";
        let enabled = update_config(original, true).unwrap();
        assert_eq!(
            enabled,
            original.replacen("0x054c/0x05c4 ", "0x054c/0x05c4 ,0x05AC/0x0004", 1)
        );
        assert_eq!(update_config(&enabled, true).unwrap(), enabled);
        assert_eq!(update_config(&enabled, false).unwrap(), original);
        assert_eq!(update_config(original, false).unwrap(), original);
    }

    #[test]
    fn recognizes_pr21_value_and_mixed_hex_case_and_removes_only_target() {
        for value in [
            "0x05AC/0x0004",
            "0x05ac/0x4",
            "0x045e/0x028e,0x05aC/0x0004,0x05AC/0x0004",
        ] {
            let bottle = TestBottle::new(&format!(
                "[EnvironmentVariables]\n\"{IGNORE_DEVICES}\" = \"{value}\" ; keep comment\n"
            ));
            assert!(duplicate_xinput_hidden(&bottle.0).unwrap());
            set_duplicate_xinput_hidden(&bottle.0, false).unwrap();
            assert!(!duplicate_xinput_hidden(&bottle.0).unwrap());
            assert!(bottle.read().ends_with("\" ; keep comment\n"));
            if value.starts_with("0x045e") {
                assert!(bottle.read().contains("0x045e/0x028e"));
            }
        }
    }

    #[test]
    fn adds_missing_variable_or_section_and_preserves_newlines() {
        for original in [
            "",
            "[Bottle]\n\"Preview\" = \"1\"\n",
            "[EnvironmentVariables]",
            "[EnvironmentVariables]\r\n;;\"PROMPT\" = \"$p$g\"\r\n",
            "[environmentvariables] ; comment\n[Other]\n",
        ] {
            let enabled = update_config(original, true).unwrap();
            let parsed = parse_config(&enabled).unwrap();
            assert_eq!(&enabled[parsed.setting.unwrap().value], DUPLICATE_DEVICE);
            assert_eq!(update_config(original, false).unwrap(), original);
            assert_eq!(update_config(&enabled, true).unwrap(), enabled);
            if !original.is_empty() {
                assert_eq!(enabled.ends_with('\n'), original.ends_with('\n'));
            }
            if original.contains("\r\n") {
                assert!(!enabled.replace("\r\n", "").contains('\n'));
            }
        }
    }

    #[test]
    fn edits_empty_and_unquoted_values() {
        for value in ["", "0x045e/0x028e", "0x045e/0x028e,", ",0x045e/0x028e,"] {
            let original = format!("[EnvironmentVariables]\n{IGNORE_DEVICES}={value}\n");
            let enabled = update_config(&original, true).unwrap();
            assert_eq!(update_config(&enabled, false).unwrap(), original);
        }
        let original = format!("[EnvironmentVariables]\n\"{IGNORE_DEVICES}\" = \" \"\n");
        let enabled = update_config(&original, true).unwrap();
        assert_eq!(update_config(&enabled, false).unwrap(), original);
    }

    #[test]
    fn refuses_file_lists_malformed_and_ambiguous_settings_without_writing() {
        for config in [
            format!("[EnvironmentVariables]\n\"{IGNORE_DEVICES}\" = \"@/tmp/custom.txt\"\n"),
            format!("[EnvironmentVariables]\n\"{IGNORE_DEVICES}\" = \"$CUSTOM_DEVICES\"\n"),
            format!("[EnvironmentVariables]\n\"{IGNORE_DEVICES}\" = \"0x05AC/0x0004oops\"\n"),
            format!("[EnvironmentVariables]\n\"{IGNORE_DEVICES}\" = \"unterminated\n"),
            format!("[EnvironmentVariables]\n\"{IGNORE_DEVICES}\" = \"\"\n\"{IGNORE_DEVICES}\" = \"\"\n"),
            "[EnvironmentVariables]\n[environmentvariables]\n".to_string(),
            "[EnvironmentVariables]\n\"sdl_gamecontroller_ignore_devices\" = \"\"\n".to_string(),
        ] {
            let bottle = TestBottle::new(&config);
            assert!(duplicate_xinput_hidden(&bottle.0).is_err());
            for hidden in [true, false] {
                assert!(set_duplicate_xinput_hidden(&bottle.0, hidden).is_err());
                assert_eq!(bottle.read(), config);
            }
        }
    }

    #[test]
    fn changes_only_selected_bottle_and_retains_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let original = "[EnvironmentVariables]\n\"WINEMSYNC\" = \"1\"\n";
        let selected = TestBottle::new(original);
        let other = TestBottle::new(original);
        fs::set_permissions(
            selected.0.join(CONFIG_FILE),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        set_duplicate_xinput_hidden(&selected.0, true).unwrap();
        assert!(duplicate_xinput_hidden(&selected.0).unwrap());
        assert_eq!(other.read(), original);
        assert_eq!(
            fs::metadata(selected.0.join(CONFIG_FILE))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        assert_eq!(fs::read_dir(&selected.0).unwrap().count(), 1);
    }

    #[test]
    fn refuses_missing_read_only_and_symlink_configs() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let bottle = TestBottle::new("[EnvironmentVariables]\n");
        let path = bottle.0.join(CONFIG_FILE);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        assert!(set_duplicate_xinput_hidden(&bottle.0, true).is_err());
        assert_eq!(bottle.read(), "[EnvironmentVariables]\n");
        fs::remove_file(&path).unwrap();
        assert!(set_duplicate_xinput_hidden(&bottle.0, true).is_err());
        let other = TestBottle::new("[EnvironmentVariables]\n");
        symlink(other.0.join(CONFIG_FILE), &path).unwrap();
        assert!(duplicate_xinput_hidden(&bottle.0).is_err());
        assert!(set_duplicate_xinput_hidden(&bottle.0, true).is_err());
        assert_eq!(other.read(), "[EnvironmentVariables]\n");
    }

    #[test]
    fn refuses_unsupported_spellings_and_preserves_original_bytes() {
        for assignment in [
            "SDL_GAMECONTROLLER_IGNORE_DEVICES=0X05AC/0x0004",
            "SDL_GAMECONTROLLER_IGNORE_DEVICES=0x05AC/0X0004",
            "SDL_GAMECONTROLLER_IGNORE_DEVICES=0x105AC/0x0004",
            "SDL_GAMECONTROLLER_IGNORE_DEVICES=1452/4",
            "SDL_GAMECONTROLLER_IGNORE_DEVICES=0x05AC/0x0004 ; literal comment",
            "SDL_GAMECONTROLLER_IGNORE_DEVICES=\"0x05AC/0x0004\"",
            "\"SDL_GAMECONTROLLER_IGNORE_DEVICES\"=0x05AC/0x0004",
            "\"SDL_GAMECONTROLLER_IGNORE_DEVICES\" = \"0x05AC/0x0004\" trailing",
            "\"SDL_GAMECONTROLLER_IGNORE_DEVIC\\ES\" = \"0x05AC/0x0004\"",
            "sdl_gamecontroller_ignore_devices=0x05AC/0x0004",
            "\u{a0}SDL_GAMECONTROLLER_IGNORE_DEVICES=0x05AC/0x0004",
            "\" SDL_GAMECONTROLLER_IGNORE_DEVICES \"=\"0x05AC/0x0004\"",
            "\"SDL_GAMECONTROLLER_IGNORE_DEVICES\"=\"0x05AC/0x0004\"\u{a0}",
            "SDL_GAMECONTROLLER_IGNORE_DEVICES",
            "#SDL_GAMECONTROLLER_IGNORE_DEVICES=0x05AC/0x0004",
        ] {
            let config = format!("[EnvironmentVariables]\n{assignment}\n");
            let bottle = TestBottle::new(&config);
            assert!(duplicate_xinput_hidden(&bottle.0).is_err(), "{assignment}");
            for hidden in [true, false] {
                assert!(set_duplicate_xinput_hidden(&bottle.0, hidden).is_err());
                assert_eq!(
                    fs::read(bottle.0.join(CONFIG_FILE)).unwrap(),
                    config.as_bytes()
                );
                assert_eq!(fs::read_dir(&bottle.0).unwrap().count(), 1);
            }
        }
        for config in [
            "[EnvironmentVariables] ; ]\n",
            "[EnvironmentVariables\n",
            "[EnvironmentVariables] garbage\n",
            "[EnvironmentVariables]\n[ENVIRONMENTVARIABLES]\n",
            "[EnvironmentVariables]\n\0",
            "[EnvironmentVariables]\rSDL_GAMECONTROLLER_IGNORE_DEVICES=0x05AC/0x0004",
        ] {
            assert!(update_config(config, true).is_err(), "{config:?}");
        }
    }

    #[test]
    fn removes_all_target_entries_in_any_position_and_keeps_other_tokens() {
        for value in [
            "0x05ac/0x4, 0x045e/0x028e ,0x05AC/0x0004,0x054C/0x05C4",
            " 0x045e/0x028e ,0x054C/0x05C4,0x05AC/0x0004",
            "0x05AC/0x0004, 0x045e/0x028e ,0x054C/0x05C4,0x05ac/0x4",
        ] {
            let original =
                format!("[EnvironmentVariables]\n\"{IGNORE_DEVICES}\"=\"{value}\" ; keep\n");
            let bottle = TestBottle::new(&original);
            assert!(duplicate_xinput_hidden(&bottle.0).unwrap());
            set_duplicate_xinput_hidden(&bottle.0, true).unwrap();
            assert_eq!(bottle.read(), original);
            set_duplicate_xinput_hidden(&bottle.0, false).unwrap();
            assert_eq!(bottle.read(), format!("[EnvironmentVariables]\n\"{IGNORE_DEVICES}\"=\" 0x045e/0x028e ,0x054C/0x05C4\" ; keep\n"));
            assert!(!duplicate_xinput_hidden(&bottle.0).unwrap());
        }
    }

    #[test]
    fn noop_does_not_replace_the_file_and_invalid_utf8_is_preserved() {
        let bottle = TestBottle::new("[EnvironmentVariables]\n");
        let path = bottle.0.join(CONFIG_FILE);
        let before = fs::metadata(&path).unwrap();
        set_duplicate_xinput_hidden(&bottle.0, false).unwrap();
        assert!(same_file_version(&before, &fs::metadata(&path).unwrap()));
        fs::write(&path, b"[EnvironmentVariables]\n\xff").unwrap();
        let before = fs::read(&path).unwrap();
        assert!(duplicate_xinput_hidden(&bottle.0).is_err());
        assert!(set_duplicate_xinput_hidden(&bottle.0, true).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[derive(Clone, Copy, Debug)]
    enum Failure {
        Create,
        PartialWrite,
        Sync,
        Rename,
        ContentsChanged,
        InodeChanged,
        ModeChanged,
    }

    struct FailingSave {
        failure: Failure,
        path: PathBuf,
    }

    impl SaveOperations for FailingSave {
        fn create_temp(&self, directory: &Path) -> io::Result<TemporaryConfig> {
            if matches!(self.failure, Failure::Create) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "injected create failure",
                ));
            }
            FileSave.create_temp(directory)
        }

        fn write_and_sync(&self, file: &mut File, contents: &[u8]) -> io::Result<()> {
            use std::os::unix::fs::PermissionsExt;
            if matches!(self.failure, Failure::PartialWrite) {
                file.write_all(&contents[..17])?;
                return Err(io::Error::other("injected partial write failure"));
            }
            if matches!(self.failure, Failure::Sync) {
                file.write_all(contents)?;
                return Err(io::Error::other("injected sync failure"));
            }
            FileSave.write_and_sync(file, contents)?;
            match self.failure {
                Failure::ContentsChanged => fs::write(&self.path, "external edit")?,
                Failure::InodeChanged => {
                    let replacement = self.path.with_extension("external");
                    fs::write(&replacement, fs::read(&self.path)?)?;
                    fs::rename(replacement, &self.path)?;
                }
                Failure::ModeChanged => {
                    fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?
                }
                _ => {}
            }
            Ok(())
        }

        fn replace(&self, temp: &Path, path: &Path) -> io::Result<()> {
            if matches!(self.failure, Failure::Rename) {
                // A real OS rename failure: the destination's parent is a file.
                return fs::rename(temp, path.join("not-a-directory"));
            }
            FileSave.replace(temp, path)
        }
    }

    #[test]
    fn save_failures_preserve_original_bytes_permissions_and_cleanup() {
        use std::os::unix::fs::PermissionsExt;
        for failure in [
            Failure::Create,
            Failure::PartialWrite,
            Failure::Sync,
            Failure::Rename,
        ] {
            let original =
                "[EnvironmentVariables]\n\"SDL_GAMECONTROLLER_IGNORE_DEVICES\"=\"0x045E/0x028E\"\n";
            let bottle = TestBottle::new(original);
            let path = bottle.0.join(CONFIG_FILE);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
            let snapshot = read_config(&path).unwrap();
            let updated = update_config(original, true).unwrap();
            assert!(
                atomic_save(
                    &path,
                    &snapshot,
                    &updated,
                    &FailingSave {
                        failure,
                        path: path.clone()
                    }
                )
                .is_err(),
                "{failure:?}"
            );
            assert_eq!(fs::read(&path).unwrap(), original.as_bytes(), "{failure:?}");
            assert!(same_file_version(
                &snapshot.metadata,
                &fs::metadata(&path).unwrap()
            ));
            assert_eq!(fs::read_dir(&bottle.0).unwrap().count(), 1, "{failure:?}");
        }
    }

    #[test]
    fn observed_external_content_identity_and_permission_changes_are_not_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        for failure in [
            Failure::ContentsChanged,
            Failure::InodeChanged,
            Failure::ModeChanged,
        ] {
            let bottle = TestBottle::new("[EnvironmentVariables]\n");
            let path = bottle.0.join(CONFIG_FILE);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
            let snapshot = read_config(&path).unwrap();
            let updated = update_config(&snapshot.config, true).unwrap();
            let error = atomic_save(
                &path,
                &snapshot,
                &updated,
                &FailingSave {
                    failure,
                    path: path.clone(),
                },
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "{failure:?}");
            match failure {
                Failure::ContentsChanged => assert_eq!(bottle.read(), "external edit"),
                Failure::InodeChanged => {
                    assert_eq!(bottle.read(), snapshot.config);
                    assert_ne!(fs::metadata(&path).unwrap().ino(), snapshot.metadata.ino());
                }
                Failure::ModeChanged => {
                    assert_eq!(bottle.read(), snapshot.config);
                    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
                }
                _ => unreachable!(),
            }
            assert_eq!(fs::read_dir(&bottle.0).unwrap().count(), 1);
        }
    }

    #[test]
    fn config_replaced_by_a_symlink_before_save_is_preserved() {
        use std::os::unix::fs::symlink;
        let selected = TestBottle::new("[EnvironmentVariables]\n");
        let other = TestBottle::new("[Bottle]\n");
        let path = selected.0.join(CONFIG_FILE);
        let snapshot = read_config(&path).unwrap();
        let updated = update_config(&snapshot.config, true).unwrap();
        fs::remove_file(&path).unwrap();
        symlink(other.0.join(CONFIG_FILE), &path).unwrap();
        assert!(atomic_save(&path, &snapshot, &updated, &FileSave).is_err());
        assert!(fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(other.read(), "[Bottle]\n");
        assert_eq!(fs::read_dir(&selected.0).unwrap().count(), 1);
    }
}
