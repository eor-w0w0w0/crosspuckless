//! Sparkle's native updater, loaded from the signed application bundle.
//!
//! Keeping loading separate from linking also allows ordinary `cargo run` and
//! tests without a packaged framework. Sparkle owns its persistent preferences;
//! the menu only changes them in response to a user action.

use objc2::rc::{Allocated, Retained};
use objc2::runtime::AnyObject;
use objc2::{
    define_class, extern_class, extern_methods, msg_send, sel, DefinedClass, MainThreadMarker,
    MainThreadOnly,
};
use objc2_app_kit::{NSControlStateValueOff, NSControlStateValueOn, NSMenu, NSMenuItem};
use objc2_foundation::{NSBundle, NSError, NSObject, NSObjectProtocol, NSString};
use std::cell::{Cell, RefCell};

extern_class!(
    // SAFETY: These declarations match Sparkle 2's public headers. The classes
    // are resolved only after loading the bundled framework below.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    struct SPUStandardUpdaterController;
);

extern_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    struct SPUUpdater;
);

impl SPUStandardUpdaterController {
    extern_methods!(
        #[unsafe(method(initWithStartingUpdater:updaterDelegate:userDriverDelegate:))]
        #[unsafe(method_family = init)]
        unsafe fn init(
            this: Allocated<Self>,
            start: bool,
            updater_delegate: Option<&AnyObject>,
            user_driver_delegate: Option<&AnyObject>,
        ) -> Retained<Self>;

        #[unsafe(method(updater))]
        #[unsafe(method_family = none)]
        fn updater(&self) -> Retained<SPUUpdater>;

        #[unsafe(method(checkForUpdates:))]
        #[unsafe(method_family = none)]
        fn check_for_updates(&self, sender: Option<&AnyObject>);
    );
}

impl SPUUpdater {
    extern_methods!(
        #[unsafe(method(startUpdater:_))]
        #[unsafe(method_family = none)]
        fn start(&self) -> Result<(), Retained<NSError>>;

        #[unsafe(method(canCheckForUpdates))]
        #[unsafe(method_family = none)]
        fn can_check(&self) -> bool;

        #[unsafe(method(automaticallyChecksForUpdates))]
        #[unsafe(method_family = none)]
        fn automatic_checks(&self) -> bool;

        #[unsafe(method(setAutomaticallyChecksForUpdates:))]
        #[unsafe(method_family = none)]
        fn set_automatic_checks(&self, enabled: bool);
    );
}

struct SparkleUpdater {
    controller: Retained<SPUStandardUpdaterController>,
    // NSBundle must outlive all Sparkle objects. Never unload the framework.
    _framework: Retained<NSBundle>,
}

impl SparkleUpdater {
    fn new(mtm: MainThreadMarker, delegate: &AnyObject) -> Result<Self, String> {
        let host = NSBundle::mainBundle();
        let feed = bundle_string(&host, "SUFeedURL")
            .ok_or("Missing Sparkle feed URL in the application bundle")?;
        if !feed.starts_with("https://") {
            return Err("The Sparkle feed must use HTTPS".into());
        }
        if bundle_string(&host, "SUPublicEDKey").is_none() {
            return Err("Missing Sparkle public signing key in the application bundle".into());
        }
        let frameworks = host
            .privateFrameworksPath()
            .ok_or("Run the packaged .app to use the updater")?;
        let path = NSString::from_str(&format!("{frameworks}/Sparkle.framework"));
        let framework = NSBundle::bundleWithPath(&path)
            .ok_or("The application does not contain Sparkle.framework")?;
        // SAFETY: Load only the framework embedded in our app, verified as part
        // of Developer ID signing. All calls and callbacks occur on the main thread.
        unsafe { framework.loadAndReturnError() }.map_err(|error| error.to_string())?;
        let controller = unsafe {
            SPUStandardUpdaterController::init(
                SPUStandardUpdaterController::alloc(mtm),
                false,
                None,
                Some(delegate),
            )
        };
        // Receive startup errors directly instead of showing a delayed modal
        // configuration-error alert when running an unpackaged development build.
        controller
            .updater()
            .start()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            controller,
            _framework: framework,
        })
    }
}

fn bundle_string(bundle: &NSBundle, key: &str) -> Option<String> {
    bundle
        .objectForInfoDictionaryKey(&NSString::from_str(key))?
        .downcast::<NSString>()
        .ok()
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty())
}

pub(crate) struct UpdateMenuControllerIvars {
    backend: RefCell<Option<SparkleUpdater>>,
    check_item: Retained<NSMenuItem>,
    automatic_item: Retained<NSMenuItem>,
    update_available: Cell<bool>,
}

define_class!(
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = UpdateMenuControllerIvars]
    pub(crate) struct UpdateMenuController;

    unsafe impl NSObjectProtocol for UpdateMenuController {}

    impl UpdateMenuController {
        #[unsafe(method(checkForUpdates:))]
        fn check_for_updates(&self, sender: Option<&AnyObject>) {
            if let Some(backend) = self.ivars().backend.borrow().as_ref() {
                if backend.controller.updater().can_check() {
                    backend.controller.check_for_updates(sender);
                }
            }
            self.refresh();
        }

        #[unsafe(method(toggleAutomaticUpdateChecks:))]
        fn toggle_automatic_checks(&self, _sender: Option<&AnyObject>) {
            if let Some(backend) = self.ivars().backend.borrow().as_ref() {
                let updater = backend.controller.updater();
                updater.set_automatic_checks(!updater.automatic_checks());
            }
            self.refresh();
        }

        // Sparkle uses respondsToSelector: for these optional delegate methods.
        // Declaring them on our NSObject also allows the class to be created in
        // development builds before the optional framework has been loaded.
        #[unsafe(method(supportsGentleScheduledUpdateReminders))]
        fn supports_gentle_reminders(&self) -> bool {
            true
        }

        #[unsafe(method(standardUserDriverShouldHandleShowingScheduledUpdate:andInImmediateFocus:))]
        fn should_show_scheduled_update(&self, _update: &AnyObject, _immediate: bool) -> bool {
            // Background checks put an indicator in the menu. Clicking it
            // brings Sparkle's standard update alert into focus.
            false
        }

        #[unsafe(method(standardUserDriverWillHandleShowingUpdate:forUpdate:state:))]
        fn will_show_update(&self, _showing: bool, _update: &AnyObject, state: &AnyObject) {
            // SAFETY: Sparkle supplies an SPUUserUpdateState with this BOOL property.
            let user_initiated: bool = unsafe { msg_send![state, userInitiated] };
            self.ivars().update_available.set(!user_initiated);
            self.refresh();
        }

        #[unsafe(method(standardUserDriverDidReceiveUserAttentionForUpdate:))]
        fn received_attention(&self, _update: &AnyObject) {
            self.clear_reminder();
        }

        #[unsafe(method(standardUserDriverWillFinishUpdateSession))]
        fn finish_update_session(&self) {
            self.clear_reminder();
        }
    }
);

impl UpdateMenuController {
    pub(crate) fn new(mtm: MainThreadMarker, menu: &NSMenu) -> Retained<Self> {
        let key = NSString::from_str("");
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        // SAFETY: Both selectors below are implemented with the AppKit action
        // signature on this controller and their targets are retained by our owner.
        let (check_item, automatic_item) = unsafe {
            (
                menu.addItemWithTitle_action_keyEquivalent(
                    &NSString::from_str("Check for Updates..."),
                    Some(sel!(checkForUpdates:)),
                    &key,
                ),
                menu.addItemWithTitle_action_keyEquivalent(
                    &NSString::from_str("Automatically Check for Updates"),
                    Some(sel!(toggleAutomaticUpdateChecks:)),
                    &key,
                ),
            )
        };
        let this = Self::alloc(mtm).set_ivars(UpdateMenuControllerIvars {
            backend: RefCell::new(None),
            check_item,
            automatic_item,
            update_available: Cell::new(false),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        unsafe {
            this.ivars().check_item.setTarget(Some(this.as_ref()));
            this.ivars().automatic_item.setTarget(Some(this.as_ref()));
        }
        match SparkleUpdater::new(mtm, this.as_ref()) {
            Ok(backend) => {
                log::info!("Sparkle updater started");
                *this.ivars().backend.borrow_mut() = Some(backend);
            }
            Err(error) => log::warn!("App updates unavailable: {error}"),
        }
        this.refresh();
        this
    }

    pub(crate) fn refresh(&self) {
        let backend = self.ivars().backend.borrow();
        let updater = backend.as_ref().map(|backend| backend.controller.updater());
        self.ivars()
            .check_item
            .setEnabled(updater.as_ref().is_some_and(|updater| updater.can_check()));
        self.ivars().check_item.setTitle(&NSString::from_str(
            if self.ivars().update_available.get() {
                "Update Available..."
            } else {
                "Check for Updates..."
            },
        ));
        self.ivars().automatic_item.setEnabled(updater.is_some());
        self.ivars().automatic_item.setState(
            if updater.is_some_and(|updater| updater.automatic_checks()) {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            },
        );
    }

    fn clear_reminder(&self) {
        self.ivars().update_available.set(false);
        self.refresh();
    }
}
