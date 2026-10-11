import AppKit
import Foundation
import Sparkle

// This executable lives in a disposable .app, never the CrossPuck product.
// Informational probes parse the appcast without downloading/installing an update.
@MainActor
final class ProbeDelegate: NSObject, SPUUpdaterDelegate {
    var foundItem: SUAppcastItem?
    var noUpdateError: NSError?
    var cycleError: NSError?
    var completed = false
    var unexpectedlyStartedInstallation = false

    func updater(_ updater: SPUUpdater, didFindValidUpdate item: SUAppcastItem) {
        foundItem = item
    }

    func updaterDidNotFindUpdate(_ updater: SPUUpdater, error: Error) {
        noUpdateError = error as NSError
    }

    func updater(_ updater: SPUUpdater, didFinishUpdateCycleFor check: SPUUpdateCheck,
                 error: Error?) {
        cycleError = error as NSError?
        completed = true
    }

    func updater(_ updater: SPUUpdater, willDownloadUpdate item: SUAppcastItem,
                 with request: NSMutableURLRequest) {
        unexpectedlyStartedInstallation = true
    }

    func updater(_ updater: SPUUpdater, willExtractUpdate item: SUAppcastItem) {
        unexpectedlyStartedInstallation = true
    }

    func updater(_ updater: SPUUpdater, willInstallUpdate item: SUAppcastItem) {
        unexpectedlyStartedInstallation = true
    }
}

@main
struct SparkleFeedSmoke {
    @MainActor
    static func main() {
        guard CommandLine.arguments.count == 2,
              ["newer", "same"].contains(CommandLine.arguments[1]),
              let domain = Bundle.main.object(forInfoDictionaryKey: "SUDefaultsDomain") as? String,
              domain.hasPrefix("com.github.scryner.crosspuck.feed-smoke.") else {
            fail("expected an isolated fixture app and newer/same argument")
        }
        let expected = CommandLine.arguments[1]
        let defaults = UserDefaults.standard
        defaults.removePersistentDomain(forName: domain)
        defer {
            defaults.removePersistentDomain(forName: domain)
            defaults.synchronize()
        }

        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)
        guard let frameworks = Bundle.main.privateFrameworksURL,
              let framework = Bundle(url: frameworks.appendingPathComponent("Sparkle.framework")) else {
            fail("the real embedded Sparkle framework is missing")
        }
        do {
            try framework.loadAndReturnError()
        } catch {
            fail("the real embedded Sparkle framework could not load: \(error)")
        }
        let delegate = ProbeDelegate()
        let controller = SPUStandardUpdaterController(startingUpdater: false,
                                                     updaterDelegate: delegate,
                                                     userDriverDelegate: nil)
        let updater = controller.updater
        do {
            try updater.start()
        } catch {
            fail("Sparkle could not start: \(error)")
        }
        guard !updater.automaticallyChecksForUpdates,
              !updater.automaticallyDownloadsUpdates,
              !updater.allowsAutomaticUpdates,
              updater.canCheckForUpdates else {
            fail("fixture should permit manual checks and disable all automatic updating")
        }

        updater.checkForUpdateInformation()
        let deadline = Date(timeIntervalSinceNow: 12)
        while !delegate.completed && Date() < deadline {
            _ = RunLoop.main.run(mode: .default, before: Date(timeIntervalSinceNow: 0.05))
        }
        guard delegate.completed else { fail("informational update probe timed out after 12 seconds") }
        guard !delegate.unexpectedlyStartedInstallation else {
            fail("informational probe attempted to download, extract, or install")
        }
        if expected == "newer" {
            guard delegate.cycleError == nil,
                  let item = delegate.foundItem,
                  item.versionString == "2.0.0",
                  item.fileURL?.lastPathComponent == "CrossPuck-newer.dmg",
                  item.minimumSystemVersion == "12.0",
                  item.hardwareRequirements.contains("arm64"),
                  item.installationType == "application" else {
                fail("newer DMG was not recognized: \(String(describing: delegate.cycleError))")
            }
            print("PASS: real Sparkle detected a higher bundle version and its DMG enclosure")
        } else {
            guard delegate.foundItem == nil,
                  let error = delegate.noUpdateError,
                  error.domain == SUSparkleErrorDomain,
                  error.code == Int(SUError.noUpdateError.rawValue),
                  delegate.cycleError?.code == error.code else {
                fail("same bundle version did not return no-update: \(String(describing: delegate.cycleError))")
            }
            print("PASS: real Sparkle rejected the same bundle version as no update")
        }
        // Keep Sparkle's weak updater delegate alive until the probe is finished.
        withExtendedLifetime((controller, delegate)) {}
    }

    static func fail(_ message: String) -> Never {
        fputs("FAIL: \(message)\n", stderr)
        exit(1)
    }
}
