//! Run from a temporary app bundle with an isolated bundle/defaults identifier.
//! This only exercises the updater; it never starts the HID host or installs updates.

#[cfg(target_os = "macos")]
#[path = "../src/updates.rs"]
mod updates;

#[cfg(target_os = "macos")]
fn main() {
    use objc2::{rc::autoreleasepool, MainThreadMarker};
    use objc2_app_kit::{NSApplication, NSControlStateValueOff, NSControlStateValueOn, NSMenu};
    use objc2_foundation::{NSBundle, NSString, NSUserDefaults};

    autoreleasepool(|_| {
        let mtm = MainThreadMarker::new().unwrap();
        let _app = NSApplication::sharedApplication(mtm);
        let mode = std::env::args().nth(1).unwrap_or_default();
        let bundle = NSBundle::mainBundle();
        // Guard against accidentally exercising a user's production preference domain.
        assert!(bundle
            .bundleIdentifier()
            .unwrap()
            .to_string()
            .starts_with("com.github.scryner.crosspuck.sparkle-smoke."));
        if mode == "cleanup" {
            NSUserDefaults::standardUserDefaults()
                .removePersistentDomainForName(&bundle.bundleIdentifier().unwrap());
            #[allow(deprecated)]
            NSUserDefaults::standardUserDefaults().synchronize();
            return;
        }
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        let controller = updates::UpdateMenuController::new(mtm, &menu);
        let check = menu.itemAtIndex(1).unwrap();
        let automatic = menu.itemAtIndex(2).unwrap();
        if mode == "unavailable" {
            assert!(!check.isEnabled());
            assert!(!automatic.isEnabled());
            println!("Missing framework/configuration disables updates safely");
            return;
        }
        assert!(check.isEnabled(), "Sparkle failed to load or start");
        assert!(automatic.isEnabled());
        let was_enabled = mode == "persisted";
        assert_eq!(
            automatic.state(),
            if was_enabled {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            }
        );
        // No runloop is entered, so automatic checks cannot initiate network I/O.
        menu.performActionForItemAtIndex(2);
        controller.refresh();
        assert_eq!(
            automatic.state(),
            if was_enabled {
                NSControlStateValueOff
            } else {
                NSControlStateValueOn
            }
        );
        let defaults = NSUserDefaults::standardUserDefaults();
        assert_eq!(
            defaults.boolForKey(&NSString::from_str("SUEnableAutomaticChecks")),
            !was_enabled
        );
        assert!(!defaults.boolForKey(&NSString::from_str("SUAutomaticallyUpdate")));
        if mode != "persist" && !was_enabled {
            menu.performActionForItemAtIndex(2);
            assert_eq!(automatic.state(), NSControlStateValueOff);
        }
        #[allow(deprecated)]
        defaults.synchronize();
        println!("Sparkle loaded, manual check enabled, automatic preference persisted; no host/network/install started");
    });
}

#[cfg(not(target_os = "macos"))]
fn main() {}
