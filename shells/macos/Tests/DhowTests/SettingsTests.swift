import DhowCore
import Foundation
import Testing

@testable import Dhow

/// The Settings window's draft and save (UI-GUIDELINES §7): one patch with
/// only the user's edits, validated by the core, its refusal at the field.
@MainActor
@Suite struct SettingsTests {
    let core: TempEngine
    let settings: DhowSettings

    init() throws {
        core = try TempEngine()
        settings = try core.engine.settings()
    }

    var send: (SettingsPatch) async throws -> DhowSettings? {
        { [core] p in try core.engine.setSettings(patch: p) }
    }

    @Test func anUneditedDraftPatchesNothing() async throws {
        #expect(try SettingsDraft(settings).patch(over: settings) == SettingsPatch())
        // Values the window shows rounded (a limit in bytes, set by the CLI)
        // stay as they are; milliseconds survive as seconds.
        var p = SettingsPatch()
        p.globalSpeedLimit = .rate(bytesPerSec: 1500)
        p.firstByteTimeoutMs = 1234
        p.extraExtensions = [CategoryExtensions(category: .video, extensions: ["qqq", "zzz"])]
        p.proxyMode = .custom(
            config: ProxyConfig(http: "http://p.test:8080", noProxy: ["localhost", ".lan"]))
        let s = try core.engine.setSettings(patch: p)
        #expect(try SettingsDraft(s).patch(over: s) == SettingsPatch())
        let form = SettingsForm()
        form.load(s)
        #expect(!form.isDirty)
        await core.close()
    }

    @Test func aDraftPatchesWhatChangedInTheCoresUnits() async throws {
        var d = SettingsDraft(settings)
        d.limitOn = true
        d.limitKB = 500
        d.firstByteTimeout = 2.5
        d.newcomerGetsConnection = false
        d.extraExtensions[.archive] = "foo, .bar baz"
        d.proxy = .custom
        d.proxyOther = " socks5://p.test:1080 "
        let p = try d.patch(over: settings)
        var expected = SettingsPatch()
        expected.globalSpeedLimit = .rate(bytesPerSec: 500_000)
        expected.firstByteTimeoutMs = 2500
        expected.minConnectionsForNew = 0
        expected.extraExtensions = [
            CategoryExtensions(category: .archive, extensions: ["foo", ".bar", "baz"])
        ]
        expected.proxyMode = .custom(config: ProxyConfig(all: "socks5://p.test:1080"))
        #expect(p == expected)
        await core.close()
    }

    /// 300 for a byte, -5 for a count: refused at the field, never clamped.
    @Test func aValueItsTypeCantHoldIsRefusedNotClamped() async throws {
        let form = SettingsForm()
        form.load(settings)
        for (edit, key, tab) in [
            (
                { (d: inout SettingsDraft) in d.maxConnectionsPerDownload = 300 },
                "max_connections_per_download", SettingsTab.connections
            ),
            ({ $0.globalConnections = -5 }, "global_connections", .connections),
            ({ $0.idleTimeout = -1 }, "idle_timeout_ms", .connections),
            ({ $0.headroomPercent = 1000 }, "headroom_percent", .speed),
            (
                {
                    $0.limitOn = true
                    $0.limitKB = -1
                }, "global_speed_limit", .speed
            ),
        ] {
            form.revert()
            edit(&form.draft)
            #expect(form.isDirty)
            let shown = await form.save(send)
            #expect(shown == tab, "\(key)")
            #expect(form.error(for: key) != nil, "\(key)")
        }
        #expect(try core.engine.settings() == settings)
        await core.close()
    }

    @Test func theCoresRefusalShowsAtTheFieldItNamesAndStoresNothing() async throws {
        let form = SettingsForm()
        form.load(settings)
        form.draft.maxConnectionsPerDownload = 2
        var tab = await form.save(send)
        #expect(tab == .connections)
        #expect(form.error(for: "max_connections_per_download") != nil)
        #expect(form.error(for: "default_connections") == nil)
        #expect(try core.engine.settings() == settings)
        #expect(form.isDirty)

        form.draft.maxConnectionsPerDownload = 20
        form.draft.proxy = .custom
        form.draft.proxyHttp = "not a url"
        tab = await form.save(send)
        #expect(tab == .network)
        #expect(form.error(for: "proxy_mode") != nil)

        // An extension refused: at the category edited.
        form.draft.proxy = .system
        form.draft.extraExtensions[.video] = "a/b"
        tab = await form.save(send)
        #expect(tab == .categories)
        #expect(form.error(for: DhowCore.Category.video) != nil)
        #expect(form.error(for: DhowCore.Category.audio) == nil)

        form.draft.extraExtensions[.video] = ""
        tab = await form.save(send)
        #expect(tab == nil)
        #expect(form.failure == nil)
        #expect(try core.engine.settings().maxConnectionsPerDownload == 20)
        #expect(!form.isDirty)
        await core.close()
    }

    /// Edits pending while another process changes other settings: Save
    /// sends the edits only, and the draft shows the others' new values.
    @Test func savingAfterAnOutsideChangeKeepsIt() async throws {
        let form = SettingsForm()
        form.load(settings)
        form.draft.idleTimeout = 99
        var other = SettingsPatch()
        other.maxRetries = 11
        let changed = try core.engine.setSettings(patch: other)
        form.load(changed)
        #expect(form.draft.maxRetries == 11)
        #expect(form.draft.idleTimeout == 99)
        #expect(form.isDirty)

        await form.save(send)
        let now = try core.engine.settings()
        #expect(now.maxRetries == 11)
        #expect(now.idleTimeoutMs == 99_000)
        #expect(!form.isDirty)
        await core.close()
    }

    @Test func theDraftFollowsTheCoreUnlessEdited() async {
        let form = SettingsForm()
        form.load(settings)
        var changed = settings
        changed.maxRetries = 9
        form.load(changed)
        #expect(form.draft.maxRetries == 9)
        form.draft.maxRetries = 5
        changed.maxRetries = 11
        form.load(changed)
        #expect(form.draft.maxRetries == 5)
        form.revert()
        #expect(form.draft.maxRetries == 11)
        await core.close()
    }

    @Test func everySettingTheCoreCanRefuseHasATabAndWords() {
        for key in [
            "global_speed_limit", "headroom_percent", "global_connections",
            "per_host_connections", "min_connections_for_new", "default_connections",
            "max_connections_per_download", "first_byte_timeout_ms", "idle_timeout_ms",
            "max_retries", "commit_interval_ms", "proxy_mode", "extra_extensions",
        ] {
            #expect(SettingsTab.of(key: key) != nil, "\(key)")
            #expect(SettingsText.invalid(key, DhowError.ShutDown) != tr("error.shutDown"), "\(key)")
        }
    }
}
