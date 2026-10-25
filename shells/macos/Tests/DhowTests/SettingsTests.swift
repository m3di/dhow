import DhowCore
import Foundation
import Testing

@testable import Dhow

/// The Settings window's draft and save (UI-GUIDELINES §7): one patch with
/// only what changed, validated by the core, its refusal at the field.
@MainActor
@Suite struct SettingsTests {
    let core: TempEngine
    let settings: DhowSettings

    init() throws {
        core = try TempEngine()
        settings = try core.engine.settings()
    }

    @Test func anUneditedDraftPatchesNothing() async throws {
        #expect(SettingsDraft(settings, folder: "/x").patch(over: settings) == SettingsPatch())
        // Values the window rounds (a limit in bytes, set by the CLI) stay as they are.
        var p = SettingsPatch()
        p.globalSpeedLimit = .rate(bytesPerSec: 1500)
        p.firstByteTimeoutMs = 1234
        p.extraExtensions = [CategoryExtensions(category: .video, extensions: ["qqq", "zzz"])]
        p.proxyMode = .custom(
            config: ProxyConfig(http: "http://p.test:8080", noProxy: ["localhost", ".lan"]))
        let s = try core.engine.setSettings(patch: p)
        #expect(SettingsDraft(s, folder: "/x").patch(over: s) == SettingsPatch())
        await core.close()
    }

    @Test func aDraftPatchesWhatChangedInTheCoresUnits() async {
        var d = SettingsDraft(settings, folder: "/x")
        d.limitOn = true
        d.limitKB = 500
        d.firstByteTimeout = 2.5
        d.newcomerGetsConnection = false
        d.extraExtensions[.archive] = "foo, .bar baz"
        d.proxy = .custom
        d.proxyOther = " socks5://p.test:1080 "
        let p = d.patch(over: settings)
        var expected = SettingsPatch()
        expected.globalSpeedLimit = .rate(bytesPerSec: 512_000)
        expected.firstByteTimeoutMs = 2500
        expected.minConnectionsForNew = 0
        expected.extraExtensions = [
            CategoryExtensions(category: .archive, extensions: ["foo", ".bar", "baz"])
        ]
        expected.proxyMode = .custom(config: ProxyConfig(all: "socks5://p.test:1080"))
        #expect(p == expected)
        #expect(SettingsDraft.ms(-1) == 0)
        await core.close()
    }

    @Test func theCoresRefusalShowsAtTheFieldItNamesAndStoresNothing() async throws {
        let form = SettingsForm()
        form.load(settings, folder: "/old")
        form.draft.maxConnectionsPerDownload = 2
        form.draft.defaultFolder = "/new"
        var stored: [String] = []
        let send = { (p: SettingsPatch) in try self.core.engine.setSettings(patch: p) }
        var tab = await form.save(send, folder: { stored.append($0) })
        #expect(tab == .connections)
        #expect(form.error(for: "max_connections_per_download") != nil)
        #expect(form.error(for: "default_connections") == nil)
        #expect(stored.isEmpty)
        #expect(try core.engine.settings() == settings)
        #expect(form.isDirty)

        form.draft.maxConnectionsPerDownload = 20
        form.draft.proxy = .custom
        form.draft.proxyHttp = "not a url"
        tab = await form.save(send, folder: { stored.append($0) })
        #expect(tab == .network)
        #expect(form.error(for: "proxy_mode") != nil)

        form.draft.proxy = .system
        tab = await form.save(send, folder: { stored.append($0) })
        #expect(tab == nil)
        #expect(form.failure == nil)
        #expect(try core.engine.settings().maxConnectionsPerDownload == 20)
        #expect(stored == ["/new"])
        #expect(!form.isDirty)
        await core.close()
    }

    @Test func theDraftFollowsTheCoreUnlessEdited() async {
        let form = SettingsForm()
        form.load(settings, folder: "/x")
        var changed = settings
        changed.maxRetries = 9
        form.load(changed, folder: "/x")
        #expect(form.draft.maxRetries == 9)
        form.draft.idleTimeout = 99
        changed.maxRetries = 11
        form.load(changed, folder: "/x")
        #expect(form.draft.maxRetries == 9)
        #expect(form.draft.idleTimeout == 99)
        form.revert()
        #expect(form.draft.maxRetries == 11)
        await core.close()
    }

    @Test func everySettingTheCoreCanRefuseHasATabAndWords() {
        for key in [
            "headroom_percent", "global_connections", "per_host_connections",
            "min_connections_for_new", "default_connections", "max_connections_per_download",
            "first_byte_timeout_ms", "idle_timeout_ms", "max_retries", "commit_interval_ms",
            "proxy_mode", "extra_extensions",
        ] {
            #expect(SettingsTab.of(key: key) != nil, "\(key)")
            #expect(SettingsText.invalid(key, DhowError.ShutDown) != tr("error.shutDown"), "\(key)")
        }
    }
}
