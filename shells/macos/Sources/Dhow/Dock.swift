import AppKit
import Observation

/// The Dock tile (UI-GUIDELINES §8 macOS): a badge with the number of
/// active downloads, and a bar with their progress together. Redrawn when
/// the store changes what it shows, never on a timer; idle, the plain icon.
@MainActor
final class DockTile {
    struct Shown: Equatable {
        var active: Int
        /// Whole percent, so a redraw is worth it.
        var percent: Int?

        @MainActor
        init(_ store: DownloadStore) {
            active = store.activeCount
            percent = store.runningFraction.map { Int(($0 * 100).rounded(.down)) }
        }

        init(active: Int, percent: Int?) {
            self.active = active
            self.percent = percent
        }

        /// The badge: the count, or none.
        var badge: String? { active > 0 ? active.formatted() : nil }
    }

    private let store: DownloadStore
    private var shown: Shown?
    private let view = DockProgressView()

    init(store: DownloadStore) {
        self.store = store
    }

    func start() {
        track()
    }

    private func track() {
        let now = withObservationTracking {
            Shown(store)
        } onChange: { [weak self] in
            DispatchQueue.main.async { self?.track() }
        }
        guard now != shown else { return }
        shown = now
        let tile = NSApp.dockTile
        tile.badgeLabel = now.badge
        if let percent = now.percent, now.active > 0 {
            view.fraction = Double(percent) / 100
            view.frame = NSRect(origin: .zero, size: tile.size)
            tile.contentView = view
        } else {
            tile.contentView = nil
        }
        tile.display()
    }
}

/// The app icon with a progress bar across its foot.
final class DockProgressView: NSView {
    var fraction = 0.0

    override func draw(_ dirtyRect: NSRect) {
        NSApp.applicationIconImage?.draw(in: bounds)
        let bar = NSRect(
            x: bounds.width * 0.1, y: bounds.height * 0.08, width: bounds.width * 0.8,
            height: bounds.height * 0.09)
        let radius = bar.height / 2
        NSColor.black.withAlphaComponent(0.55).setFill()
        NSBezierPath(roundedRect: bar, xRadius: radius, yRadius: radius).fill()
        var filled = bar.insetBy(dx: 2, dy: 2)
        filled.size.width *= max(0, min(1, fraction))
        NSColor.controlAccentColor.setFill()
        NSBezierPath(roundedRect: filled, xRadius: filled.height / 2, yRadius: filled.height / 2)
            .fill()
    }
}
