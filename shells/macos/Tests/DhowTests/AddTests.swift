import DhowCore
import Foundation
import Testing

@testable import Dhow

@MainActor
@Suite struct AddTests {
    @Test func linksAreTheHttpURLsInPastedText() {
        let text = """
            https://example.com/a.iso  ftp://example.com/b
            not a link http://example.com/c.zip
            https://example.com/a.iso mailto:x@y.z https://
            """
        #expect(
            AddModel.links(in: text) == ["https://example.com/a.iso", "http://example.com/c.zip"])
    }

    @Test func headersAreNameColonValueLines() {
        let headers = AddModel.parseHeaders("Cookie: a=b; c=d\n\n  Referer : https://x/ \n")
        #expect(headers?.map(\.name) == ["Cookie", "Referer"])
        #expect(headers?.map(\.value) == ["a=b; c=d", "https://x/"])
        #expect(AddModel.parseHeaders("no colon") == nil)
        #expect(AddModel.parseHeaders(": value") == nil)
    }

    @Test func itemsFollowTheTextAndKeepTheirProbes() {
        let add = AddModel(
            text: "https://a.test/1", directory: URL(fileURLWithPath: "/tmp"), queue: "q")
        add.rename("https://a.test/1", to: "one.bin")
        add.text = "https://a.test/1\nhttps://a.test/2"
        #expect(add.items.map(\.url) == ["https://a.test/1", "https://a.test/2"])
        #expect(add.items[0].name == "one.bin")
        // Probes still running: nothing can be confirmed (§4).
        #expect(add.probing && !add.canConfirm)
    }

    /// The probe goes through the core; one that fails offers "Download
    /// anyway", which adds blind.
    @Test func aFailedProbeOffersABlindAdd() async throws {
        let core = try TempEngine()
        // Nothing listens on port 1.
        let add = AddModel(
            text: "http://127.0.0.1:1/file.bin", directory: core.dir,
            queue: try core.engine.listQueues()[0].id)
        await add.probe(with: core.engine)
        let item = try #require(add.items.first)
        #expect(!add.probing)
        #expect(ErrorText.message(try #require(item.failure)) == ErrorText.probe(.connect))
        #expect(add.isBlind && add.canConfirm)
        #expect(add.request(for: item).blind)
        #expect(add.request(for: item).filename == nil)

        add.start = .paused
        #expect(await add.confirm(with: core.engine), "\(add.error ?? "")")
        #expect(add.items.isEmpty)
        let added = try core.engine.list()
        #expect(added.count == 1 && added[0].state == .paused)
        await core.close()
    }

    @Test func aBadSchemeCannotBeAddedBlind() async throws {
        let error = DhowError.Probe(reason: .unsupportedScheme)
        #expect(!ErrorText.blindCanHelp(error))
        #expect(ErrorText.blindCanHelp(DhowError.Probe(reason: .timeout)))
        #expect(!ErrorText.blindCanHelp(DhowError.InvalidArgument(message: "")))
    }
}
