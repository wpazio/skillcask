import SkillcaskDomain
import SwiftUI

/// One filter chip of the list header (Components board, "Filter chips and buttons"): a 22 pt capsule
/// (`Size.chip`, `Radius.pill`) in `calloutMedium`. Idle it is `textSecondary` on a transparent fill with a
/// `separatorStrong` hairline; active it is `accent` on `accentSoft` with no border. An optional count sits
/// in the small `CountBadge` (`Size.badgeSmall`, 16 pt) filled with the severity's dot colour, and an optional SF
/// Symbol before the title (the Add Skill sheet's source glyphs, `Pill.symbol(of:)`).
///
/// It is a `Button` with a custom style, so the focus ring, keyboard activation and the accessibility
/// button trait come from the system; only the drawing is custom.
public struct FilterChip: View {
    public let title: String
    /// The number in the badge; `nil` or `0` draws no badge (the board shows none on "Not managed").
    public let count: Int?
    /// The badge fill: `.error` for "Needs attention", `.update` for "Updates", `.neutral` otherwise.
    public let countSeverity: Severity
    public let isActive: Bool
    /// The glyph before the title; nil draws none (the list's filter chips).
    public let symbol: String?
    /// The space either side of the label: the board's 12 pt (`Spacing.m`), 10 pt in the list's chip bar.
    public let horizontalPadding: CGFloat
    public let action: () -> Void

    public init(
        _ title: String, symbol: String? = nil, count: Int? = nil, countSeverity: Severity = .neutral,
        isActive: Bool, horizontalPadding: CGFloat = Spacing.m, action: @escaping () -> Void
    ) {
        self.title = title
        self.symbol = symbol
        self.count = count
        self.countSeverity = countSeverity
        self.isActive = isActive
        self.horizontalPadding = horizontalPadding
        self.action = action
    }

    /// The width a shown count badge adds to a chip: the one-digit small badge and the gap before it.
    nonisolated public static let badgeReserve: CGFloat = Size.badgeSmall + Spacing.xs

    /// The badge text of a count: `nil` for no count or zero, otherwise the number in the user's locale.
    nonisolated public static func countText(_ count: Int?) -> String? {
        guard let count, count > 0 else { return nil }
        return count.formatted()
    }

    public var body: some View {
        Button(action: action) {
            HStack(spacing: Spacing.xs) {
                if let symbol {
                    Image(systemName: symbol)
                        .imageScale(.small)
                        .accessibilityHidden(true)
                }
                Text(title)
                if let count {
                    CountBadge(count, severity: countSeverity, variant: .small)
                }
            }
        }
        .buttonStyle(ChipButtonStyle(isActive: isActive, horizontalPadding: horizontalPadding))
        .accessibilityAddTraits(isActive ? .isSelected : [])
    }
}

/// The chip's drawing: capsule, hairline when idle, `accentSoft` when active; the label dims while pressed.
private struct ChipButtonStyle: ButtonStyle {
    let isActive: Bool
    let horizontalPadding: CGFloat

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(Font.Token.calloutMedium)
            .foregroundStyle(isActive ? Color.accent : Color.textSecondary)
            .lineLimit(1)
            .padding(.horizontal, horizontalPadding)
            .frame(height: Size.chip)
            .background(isActive ? Color.accentSoft : Color.clear, in: Capsule())
            .overlay {
                CapsuleRing(width: Size.hairline)
                    .fill(isActive ? Color.clear : Color.separatorStrong, style: FillStyle(eoFill: true))
            }
            .contentShape(Capsule())
            .opacity(configuration.isPressed ? Self.pressedOpacity : 1)
    }

    /// Press feedback; the board has no pressed state, so this is the system push button's dimming.
    private static let pressedOpacity = 0.6
}

/// The idle hairline as a filled ring (the capsule minus the capsule inset by `width`), not a stroke: the
/// preview renderer's `cacheDisplay` draws a stroked 22 pt capsule with stray vertical bars at both ends
/// (the chip previews), while fills come out exact there and on screen alike.
struct CapsuleRing: Shape {
    let width: CGFloat

    func path(in rect: CGRect) -> Path {
        var path = Capsule().path(in: rect)
        path.addPath(Capsule().path(in: rect.insetBy(dx: width, dy: width)))
        return path
    }
}

#Preview("FilterChip, light") {
    HStack(spacing: Spacing.s) {
        FilterChip("All", isActive: true) {}
        FilterChip("Needs attention", count: 5, countSeverity: .error, isActive: false) {}
        FilterChip("Updates", count: 2, countSeverity: .update, isActive: false) {}
        FilterChip("Not managed", isActive: false) {}
    }
    .padding(Spacing.l)
    .background(Color.surfaceWindow)
}

#Preview("FilterChip, dark") {
    HStack(spacing: Spacing.s) {
        FilterChip("All", isActive: true) {}
        FilterChip("Needs attention", count: 5, countSeverity: .error, isActive: false) {}
        FilterChip("Updates", count: 2, countSeverity: .update, isActive: false) {}
        FilterChip("Not managed", isActive: false) {}
    }
    .padding(Spacing.l)
    .background(Color.surfaceWindow)
    .preferredColorScheme(.dark)
}
