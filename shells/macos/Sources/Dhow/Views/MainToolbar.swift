import DhowCore
import SwiftUI

/// Add, Resume all, Pause all and the speed readout (UI-GUIDELINES §3);
/// search is the window's `.searchable`.
struct MainToolbar: ToolbarContent {
    @Environment(AppModel.self) private var model

    var body: some ToolbarContent {
        let store = model.store
        ToolbarItemGroup(placement: .primaryAction) {
            Button {
                model.showAdd()
            } label: {
                Label(tr("toolbar.add"), systemImage: "plus")
            }
            .keyboardShortcut("n")
            .help(tr("toolbar.addHelp"))
            .disabled(model.engine == nil)
            Button {
                model.resumeAll()
            } label: {
                Label(tr("toolbar.resumeAll"), systemImage: "play.fill")
            }
            .help(tr("toolbar.resumeAll"))
            .disabled(!store.downloads.values.contains { $0.state == .paused })
            Button {
                model.pauseAll()
            } label: {
                Label(tr("toolbar.pauseAll"), systemImage: "pause.fill")
            }
            .help(tr("toolbar.pauseAll"))
            .disabled(!store.downloads.values.contains { Actions.canPause($0.state) })
        }
        ToolbarItem(placement: .status) {
            SpeedReadout(speed: store.totalSpeed, samples: store.speedSamples)
        }
    }
}

/// The global speed with a tiny sparkline: the only chart on the main
/// view, ≤ 120 pt wide (§8 macOS).
struct SpeedReadout: View {
    var speed: UInt64
    var samples: [UInt64]

    var body: some View {
        HStack(spacing: 6) {
            Text(Format.speed(speed))
                .monospacedDigit()
                .font(.callout)
                .foregroundStyle(samples.isEmpty ? .tertiary : .secondary)
                .lineLimit(1)
                .frame(width: 64, alignment: .trailing)
            Sparkline(samples: samples)
                .stroke(.secondary, lineWidth: 1)
                .frame(width: 40, height: 14)
        }
        .padding(.horizontal, 4)
        .frame(width: 120)
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(tr("toolbar.speed \(Format.speed(speed))"))
    }
}

/// Samples left to right, oldest first, scaled to the largest; the slot
/// fills from the right as samples arrive.
struct Sparkline: Shape {
    var samples: [UInt64]

    func path(in rect: CGRect) -> Path {
        var path = Path()
        guard samples.count > 1 else { return path }
        let top = Double(max(samples.max() ?? 1, 1))
        let step = rect.width / CGFloat(DownloadStore.sparklineLength - 1)
        let x0 = rect.maxX - step * CGFloat(samples.count - 1)
        for (i, s) in samples.enumerated() {
            let point = CGPoint(
                x: x0 + step * CGFloat(i), y: rect.maxY - rect.height * CGFloat(Double(s) / top))
            if i == 0 { path.move(to: point) } else { path.addLine(to: point) }
        }
        return path
    }
}
